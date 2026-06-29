//! Destructive install orchestration: the worker thread that drives
//! `systemd-repart --dry-run=no` and the post-write steps, feeding a progress
//! channel the Progress screen renders live. No async runtime; the ratatui loop
//! stays responsive by draining the channel each tick.
//!
//! # Safety chokepoint
//!
//! Execution is gated behind [`InstallPlan::authorize`], the *only* constructor
//! of an [`InstallPlan`]. It returns `Some` only when every invariant holds:
//!
//! 1. `!dry_run` — never run when the global dry-run flag is set;
//! 2. the user typed the target device name *exactly* on the Confirm screen;
//! 3. that typed name equals the DiskSelect-chosen device.
//!
//! [`spawn`] consumes an `InstallPlan` by value, so the destructive path is
//! unreachable without first clearing the gate. The worker always sends a
//! terminal [`Progress::Done`]; it never panics out (any error becomes
//! [`Outcome::Failed`]), so the main thread keeps ownership of the terminal and
//! the panic-restore hook is never raced from the worker.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use serde::Deserialize;

use crate::repart::generate::OUTPUT_DIR;
use crate::repart::runner;

/// Where the target root (and the cloned `/usr` beneath it) is mounted for the
/// post-write steps. Under `/run`, cleared on reboot.
const TARGET_MOUNT: &str = "/run/archetype-install/target";

/// A message from the install worker to the Progress screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// A new discrete step began.
    Step(String),
    /// A log line (repart output, command output, or a note).
    Line(String),
    /// Terminal message: the install finished (succeeded or failed).
    Done(Outcome),
}

/// The terminal result of an install run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// repart wrote the table AND every required post-step (mount, /etc seed)
    /// completed. Safe to reboot.
    Success,
    /// repart wrote the table but a required post-step did not complete (e.g.
    /// root unlock/mount or /etc seeding). The partitions exist but the install
    /// is not finished; `detail` names what's outstanding. MUST NOT offer reboot
    /// -- booting now risks an unbootable/half-seeded system.
    Incomplete { detail: String },
    /// A fatal step failed. `step` names it; `error` is the verbatim reason.
    Failed { step: String, error: String },
}

/// An authorized, destructive install. Construct only via
/// [`InstallPlan::authorize`]; holding one means the safety gate has passed.
pub struct InstallPlan {
    device: String,
    definitions_dir: PathBuf,
}

impl InstallPlan {
    /// The sole constructor. Returns `Some` only when all safety invariants
    /// hold (see module docs). `chosen_device` is the DiskSelect target;
    /// `typed_name` is what the user typed on Confirm.
    pub fn authorize(dry_run: bool, chosen_device: &str, typed_name: &str) -> Option<Self> {
        if dry_run {
            return None;
        }
        if chosen_device.is_empty() {
            return None;
        }
        if typed_name != chosen_device {
            return None;
        }
        Some(Self {
            device: chosen_device.to_string(),
            definitions_dir: PathBuf::from(OUTPUT_DIR),
        })
    }
}

/// A running install: the progress channel and the worker handle.
pub struct Install {
    pub progress: Receiver<Progress>,
    handle: Option<JoinHandle<()>>,
}

impl Install {
    /// Join the worker thread, ignoring its (unit) result. Idempotent.
    pub fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn the install worker for an authorized `plan`. The returned [`Install`]
/// carries the progress channel; the worker runs to a terminal
/// [`Progress::Done`] on its own thread.
pub fn spawn(plan: InstallPlan) -> Install {
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || run(plan, tx));
    Install {
        progress: rx,
        handle: Some(handle),
    }
}

/// The worker body. Always finishes by sending exactly one [`Progress::Done`].
fn run(plan: InstallPlan, tx: Sender<Progress>) {
    let outcome = match write_partitions(&plan, &tx) {
        Ok(()) => match post_steps(&plan, &tx) {
            Ok(()) => Outcome::Success,
            Err(detail) => Outcome::Incomplete { detail },
        },
        Err((step, error)) => Outcome::Failed { step, error },
    };
    let _ = tx.send(Progress::Done(outcome));
}

/// Fatal step: drive `systemd-repart --dry-run=no` against the target. A
/// non-zero exit or a spawn failure aborts the install.
fn write_partitions(plan: &InstallPlan, tx: &Sender<Progress>) -> Result<(), (String, String)> {
    let step = format!("Writing partition table to {}", plan.device);
    let _ = tx.send(Progress::Step(step.clone()));

    let line_tx = tx.clone();
    let outcome = runner::execute(&plan.definitions_dir, &plan.device, |line| {
        let _ = line_tx.send(Progress::Line(line.to_string()));
    });

    match outcome {
        Ok(outcome) if outcome.success() => Ok(()),
        Ok(outcome) => Err((
            step,
            format!(
                "systemd-repart exited {}: {}",
                outcome
                    .exit_code
                    .map_or_else(|| "by signal".to_string(), |code| code.to_string()),
                outcome.stderr.trim()
            ),
        )),
        Err(err) => Err((step, err.to_string())),
    }
}

/// Required post-write steps. repart (the destructive write) has already
/// succeeded; these finish the install. Returns `Err(detail)` if a step needed
/// for a bootable system did not complete, so the caller reports
/// [`Outcome::Incomplete`] (no reboot) rather than a false "complete". A failure
/// still leaves a recoverable console -- it never panics or halts.
fn post_steps(plan: &InstallPlan, tx: &Sender<Progress>) -> Result<(), String> {
    settle(tx);

    let parts = locate_partitions(&plan.device).map_err(|err| {
        let _ = tx.send(Progress::Step("Locating target partitions".to_string()));
        warn(tx, &format!("could not enumerate partitions: {err}"));
        format!("target partitions could not be enumerated: {err}")
    })?;

    mount_and_seed(&parts, tx)?;
    bootloader_note(tx);
    Ok(())
}

/// Ask udev to settle so the freshly written partition nodes appear.
fn settle(tx: &Sender<Progress>) {
    let _ = tx.send(Progress::Step("Settling device nodes".to_string()));
    run_cmd(tx, "udevadm", &["settle"]);
}

/// Mount the target root, mount the cloned `/usr` beneath it, then seed `/etc`
/// from the factory tree via `systemd-tmpfiles`. Each is best-effort and loudly
/// flagged where the mechanism is unverified (encrypted root, verity `/usr`).
fn mount_and_seed(parts: &TargetPartitions, tx: &Sender<Progress>) -> Result<(), String> {
    let _ = tx.send(Progress::Step("Mounting target root".to_string()));
    let root_mount = Path::new(TARGET_MOUNT);
    std::fs::create_dir_all(root_mount)
        .map_err(|err| format!("could not create {TARGET_MOUNT}: {err}"))?;

    let root = parts
        .root
        .as_ref()
        .ok_or_else(|| "no root partition found on the target".to_string())?;
    log(tx, &format!("root partition: {root}"));
    warn(
        tx,
        "root is Encrypt=tpm2; unlocking via the TPM2-enrolled key is \
         UNVERIFIED here. TODO: wire cryptsetup/systemd-cryptsetup unlock \
         before mount. Attempting a plain mount (expected to fail on LUKS).",
    );
    if !try_mount(tx, root, root_mount) {
        return Err(
            "could not mount the target root (TPM2 unlock not yet wired); \
             /usr mount + /etc seeding skipped"
                .to_string(),
        );
    }

    let usr = parts
        .usr
        .as_ref()
        .ok_or_else(|| "no /usr partition found on the target".to_string())?;
    let _ = tx.send(Progress::Step("Mounting cloned /usr".to_string()));
    log(tx, &format!("/usr partition: {usr}"));
    warn(
        tx,
        "/usr is dm-verity; a bare mount needs the hash+sig (veritysetup / \
         systemd-dissect). UNVERIFIED. TODO: open the verity volume before mount.",
    );
    let usr_mount = root_mount.join("usr");
    std::fs::create_dir_all(&usr_mount)
        .map_err(|err| format!("could not create {}: {err}", usr_mount.display()))?;
    if !try_mount(tx, usr, &usr_mount) {
        return Err("could not mount the cloned /usr; /etc seeding skipped".to_string());
    }

    seed_etc(tx, root_mount)
}

/// `systemd-tmpfiles --root=<target> --boot --create`: seed persistent `/etc`
/// from `/usr/share/factory/etc` (per 70-root.conf).
fn seed_etc(tx: &Sender<Progress>, root_mount: &Path) -> Result<(), String> {
    let _ = tx.send(Progress::Step("Seeding /etc from factory".to_string()));
    let ok = run_cmd(
        tx,
        "systemd-tmpfiles",
        &[
            &format!("--root={}", root_mount.display()),
            "--boot",
            "--create",
        ],
    );
    if ok {
        Ok(())
    } else {
        Err("systemd-tmpfiles failed to seed /etc from the factory tree".to_string())
    }
}

/// The bootloader step. Per the Phase-1 spike (PLAN \u{a7}13 Q4), the runtime ESP
/// template's `CopyFiles` populates the loader + UKIs and excludes
/// `installer.addon.efi`, so repart itself handles the ESP during the write.
/// Whether `bootctl install --root` / `kernel-install` is additionally needed
/// is not confirmed in-image; flag it rather than guess.
fn bootloader_note(tx: &Sender<Progress>) {
    let _ = tx.send(Progress::Step("Bootloader".to_string()));
    log(
        tx,
        "ESP populated by repart CopyFiles (installer.addon.efi excluded, per spike).",
    );
    warn(
        tx,
        "TODO/UNVERIFIED: confirm on a live VM whether bootctl install --root / \
         kernel-install is also required, or repart's ESP CopyFiles suffices.",
    );
}

/// Attempt `mount <source> <target>`, logging the result. Returns true on a
/// clean mount.
fn try_mount(tx: &Sender<Progress>, source: &str, target: &Path) -> bool {
    run_cmd(tx, "mount", &[source, &target.display().to_string()])
}

/// Run a command, logging the invocation and its captured output. Returns true
/// on a zero exit. A spawn failure or non-zero exit is logged as a warning but
/// is never fatal (post-steps are best-effort; see [`post_steps`]).
fn run_cmd(tx: &Sender<Progress>, program: &str, args: &[&str]) -> bool {
    log(tx, &format!("$ {program} {}", args.join(" ")));
    match Command::new(program).args(args).output() {
        Ok(output) => {
            for stream in [&output.stdout, &output.stderr] {
                for line in String::from_utf8_lossy(stream).lines() {
                    log(tx, &format!("  {line}"));
                }
            }
            if output.status.success() {
                true
            } else {
                warn(
                    tx,
                    &format!(
                        "{program} exited {}",
                        output
                            .status
                            .code()
                            .map_or_else(|| "by signal".to_string(), |c| c.to_string())
                    ),
                );
                false
            }
        }
        Err(err) => {
            warn(tx, &format!("failed to run {program}: {err}"));
            false
        }
    }
}

fn log(tx: &Sender<Progress>, line: &str) {
    let _ = tx.send(Progress::Line(line.to_string()));
}

fn warn(tx: &Sender<Progress>, line: &str) {
    let _ = tx.send(Progress::Line(format!("! {line}")));
}

/// The target partitions we care about for post-write steps, resolved from
/// `lsblk` by GPT partition type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TargetPartitions {
    esp: Option<String>,
    root: Option<String>,
    usr: Option<String>,
}

/// Enumerate `device`'s partitions and classify them by GPT type. Returns an
/// error only when `lsblk` cannot be run or its JSON cannot be parsed.
fn locate_partitions(device: &str) -> Result<TargetPartitions, String> {
    let output = Command::new("lsblk")
        .args(["--json", "-o", "PATH,PARTTYPENAME", device])
        .output()
        .map_err(|err| format!("failed to run lsblk: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "lsblk exited {}",
            output
                .status
                .code()
                .map_or_else(|| "by signal".to_string(), |c| c.to_string())
        ));
    }
    parse_partitions(&String::from_utf8_lossy(&output.stdout))
}

#[derive(Debug, Deserialize)]
struct LsblkPartOutput {
    blockdevices: Vec<LsblkNode>,
}

#[derive(Debug, Deserialize)]
struct LsblkNode {
    path: String,
    #[serde(default, rename = "parttypename")]
    parttypename: Option<String>,
    #[serde(default)]
    children: Vec<LsblkNode>,
}

/// Classify the first partition matching each GPT type. Walks the whole tree so
/// it does not depend on the device being the top-level node.
fn parse_partitions(json: &str) -> Result<TargetPartitions, String> {
    let parsed: LsblkPartOutput =
        serde_json::from_str(json).map_err(|err| format!("failed to parse lsblk JSON: {err}"))?;
    let mut parts = TargetPartitions::default();
    for node in &parsed.blockdevices {
        classify(node, &mut parts);
    }
    Ok(parts)
}

fn classify(node: &LsblkNode, parts: &mut TargetPartitions) {
    if let Some(kind) = node.parttypename.as_deref() {
        let slot = match kind {
            "EFI System" => &mut parts.esp,
            k if k.starts_with("Linux root") => &mut parts.root,
            k if k.starts_with("Linux /usr") && !k.contains("verity") => &mut parts.usr,
            _ => return walk_children(node, parts),
        };
        if slot.is_none() {
            *slot = Some(node.path.clone());
        }
    }
    walk_children(node, parts);
}

fn walk_children(node: &LsblkNode, parts: &mut TargetPartitions) {
    for child in &node.children {
        classify(child, parts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_rejects_dry_run() {
        assert!(InstallPlan::authorize(true, "/dev/sdb", "/dev/sdb").is_none());
    }

    #[test]
    fn authorize_rejects_name_mismatch() {
        assert!(InstallPlan::authorize(false, "/dev/sdb", "/dev/sda").is_none());
        assert!(InstallPlan::authorize(false, "/dev/sdb", "").is_none());
        assert!(InstallPlan::authorize(false, "/dev/sdb", "/dev/sdb ").is_none());
        assert!(InstallPlan::authorize(false, "/dev/sdb", "sdb").is_none());
    }

    #[test]
    fn authorize_rejects_empty_chosen_device() {
        assert!(InstallPlan::authorize(false, "", "").is_none());
    }

    #[test]
    fn authorize_accepts_exact_match_when_not_dry_run() {
        let plan = InstallPlan::authorize(false, "/dev/sdb", "/dev/sdb").expect("should authorize");
        assert_eq!(plan.device, "/dev/sdb");
        assert_eq!(plan.definitions_dir, PathBuf::from(OUTPUT_DIR));
    }

    #[test]
    fn parse_partitions_classifies_by_gpt_type() {
        let json = r#"{
           "blockdevices": [
              {
                 "path": "/dev/sdb", "parttypename": null,
                 "children": [
                    {"path": "/dev/sdb1", "parttypename": "EFI System"},
                    {"path": "/dev/sdb2", "parttypename": "Linux /usr (x86-64)"},
                    {"path": "/dev/sdb3", "parttypename": "Linux /usr verity (x86-64)"},
                    {"path": "/dev/sdb4", "parttypename": "Linux root (x86-64)"},
                    {"path": "/dev/sdb5", "parttypename": "Linux swap"}
                 ]
              }
           ]
        }"#;
        let parts = parse_partitions(json).unwrap();
        assert_eq!(parts.esp.as_deref(), Some("/dev/sdb1"));
        assert_eq!(parts.root.as_deref(), Some("/dev/sdb4"));
        // The verity partition must not be picked as the /usr data partition.
        assert_eq!(parts.usr.as_deref(), Some("/dev/sdb2"));
    }

    #[test]
    fn parse_partitions_handles_no_matches() {
        let json = r#"{"blockdevices": [{"path": "/dev/sdb", "parttypename": null}]}"#;
        let parts = parse_partitions(json).unwrap();
        assert_eq!(parts, TargetPartitions::default());
    }

    #[test]
    fn parse_partitions_errors_on_garbage() {
        assert!(parse_partitions("not json").is_err());
    }

    /// Destructive end-to-end smoke test. Requires root and a SCRATCH loop
    /// device or disk in ARCHETYPE_INSTALL_TEST_DEV; it WIPES that device.
    /// Ignored by default — run only in a throwaway VM:
    ///   ARCHETYPE_INSTALL_TEST_DEV=/dev/loopN cargo test -- --ignored execute_smoke
    #[test]
    #[ignore = "destructive: writes partitions; run only in a VM against a scratch device"]
    fn execute_smoke_writes_to_scratch_device() {
        let device = std::env::var("ARCHETYPE_INSTALL_TEST_DEV")
            .expect("set ARCHETYPE_INSTALL_TEST_DEV to a SCRATCH device");
        let plan = InstallPlan::authorize(false, &device, &device)
            .expect("authorize should pass for an exact match");
        let install = spawn(plan);
        let mut outcome = None;
        for message in install.progress.iter() {
            if let Progress::Done(done) = message {
                outcome = Some(done);
            }
        }
        assert!(matches!(outcome, Some(Outcome::Success)), "install failed");
    }
}
