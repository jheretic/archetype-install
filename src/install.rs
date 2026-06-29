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

use crate::firstboot::FirstbootConfig;
use crate::repart::generate::OUTPUT_DIR;
use crate::repart::runner;

/// Where the target root (and the cloned `/usr` beneath it) is mounted for the
/// post-write steps. Under `/run`, cleared on reboot.
const TARGET_MOUNT: &str = "/run/archetype-install/target";

/// The device-mapper name for the unlocked target root. The opened LUKS2 volume
/// appears at `/dev/mapper/<ROOT_VOLUME>`.
const ROOT_VOLUME: &str = "archetype-root";

/// `systemd-cryptsetup` is not on `PATH`; it ships at this fixed location.
const CRYPTSETUP_BIN: &str = "/usr/lib/systemd/systemd-cryptsetup";

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
    firstboot: FirstbootConfig,
}

impl InstallPlan {
    /// The sole constructor. Returns `Some` only when all safety invariants
    /// hold (see module docs). `chosen_device` is the DiskSelect target;
    /// `typed_name` is what the user typed on Confirm.
    pub fn authorize(
        dry_run: bool,
        chosen_device: &str,
        typed_name: &str,
        firstboot: FirstbootConfig,
    ) -> Option<Self> {
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
            firstboot,
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

    // mount_and_seed opens the LUKS2 root (/dev/mapper/<ROOT_VOLUME>) and mounts
    // it. From here on, any failure must tear that down so we don't leave an
    // unlocked encrypted volume + mounts dangling on the recovery console.
    mount_and_seed(&parts, tx)?;
    let root_mount = Path::new(TARGET_MOUNT);
    let rest = apply_firstboot(&plan.firstboot, root_mount, tx)
        .and_then(|()| write_machine_info(&plan.firstboot, root_mount, tx));
    if let Err(detail) = rest {
        detach_root(tx);
        return Err(detail);
    }
    bootloader_note(tx);
    Ok(())
}

/// Best-effort teardown of the opened+mounted target root after a post-open
/// failure: unmount /usr and root, then detach the LUKS2 mapper so no unlocked
/// encrypted volume is left dangling. Never fatal; logged only.
fn detach_root(tx: &Sender<Progress>) {
    let _ = tx.send(Progress::Step("Tearing down target root".to_string()));
    let root_mount = Path::new(TARGET_MOUNT);
    run_cmd(
        tx,
        "umount",
        &[&root_mount.join("usr").display().to_string()],
    );
    run_cmd(tx, "umount", &[&root_mount.display().to_string()]);
    run_cmd(tx, CRYPTSETUP_BIN, &["detach", ROOT_VOLUME]);
}

/// Apply first-boot config to the mounted target via
/// `systemd-firstboot --root=<target>`. Required for a configured system; a
/// failure leaves the disk written but unconfigured, hence
/// [`Outcome::Incomplete`] (recoverable, no reboot).
fn apply_firstboot(
    firstboot: &FirstbootConfig,
    root_mount: &Path,
    tx: &Sender<Progress>,
) -> Result<(), String> {
    let _ = tx.send(Progress::Step("Applying first-boot config".to_string()));
    let args = firstboot.firstboot_args(root_mount);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    if run_cmd(tx, "systemd-firstboot", &arg_refs) {
        Ok(())
    } else {
        Err("systemd-firstboot failed to apply first-boot config to the target".to_string())
    }
}

/// Write `<target>/etc/machine-info` with `CHASSIS=`. A discrete step: `CHASSIS=`
/// is a machine-info(5) field, not a firstboot flag. `/etc` exists after the
/// factory seed but the file itself usually does not, so it is created.
fn write_machine_info(
    firstboot: &FirstbootConfig,
    root_mount: &Path,
    tx: &Sender<Progress>,
) -> Result<(), String> {
    let _ = tx.send(Progress::Step("Writing /etc/machine-info".to_string()));
    let path = root_mount.join("etc/machine-info");
    log(tx, &format!("writing {}", path.display()));
    std::fs::write(&path, firstboot.machine_info())
        .map_err(|err| format!("could not write {}: {err}", path.display()))
}

/// TPM2-unlock the LUKS2 root via `systemd-cryptsetup attach`. The KEY-FILE
/// positional is `-` (none) so the key is taken from the TPM2 keyslot in the
/// LUKS2 header; `tpm2-device=auto` selects the local TPM2 and `headless=yes`
/// forbids an interactive passphrase fallback. Opens at
/// `/dev/mapper/<ROOT_VOLUME>`. Returns true on a clean attach.
fn tpm2_unlock(tx: &Sender<Progress>, root_device: &str) -> bool {
    let args = cryptsetup_attach_args(root_device);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_cmd(tx, CRYPTSETUP_BIN, &arg_refs)
}

/// Build the `systemd-cryptsetup attach` argv for the TPM2 root unlock. KEY-FILE
/// is `-` (none); the crypttab options select the TPM2 keyslot non-interactively.
fn cryptsetup_attach_args(root_device: &str) -> Vec<String> {
    vec![
        "attach".to_string(),
        ROOT_VOLUME.to_string(),
        root_device.to_string(),
        "-".to_string(),
        "tpm2-device=auto,headless=yes".to_string(),
    ]
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

    let _ = tx.send(Progress::Step("Unlocking target root (TPM2)".to_string()));
    // A stale /dev/mapper/<ROOT_VOLUME> from an earlier aborted run would make
    // the attach fail; clear it first (best effort, ignored if absent).
    if Path::new(&format!("/dev/mapper/{ROOT_VOLUME}")).exists() {
        warn(
            tx,
            &format!("stale /dev/mapper/{ROOT_VOLUME}; detaching first"),
        );
        run_cmd(tx, CRYPTSETUP_BIN, &["detach", ROOT_VOLUME]);
    }
    if !tpm2_unlock(tx, root) {
        return Err(format!(
            "could not TPM2-unlock the target root {root}; the partitions are \
             written but root could not be opened (recoverable, no reboot)"
        ));
    }
    // Root is now OPEN. Any failure past this point must detach_root before
    // returning so we don't leak an unlocked encrypted volume.
    let opened = format!("/dev/mapper/{ROOT_VOLUME}");
    if !try_mount(tx, &opened, root_mount) {
        detach_root(tx);
        return Err(format!(
            "unlocked root {opened} but could not mount it at {TARGET_MOUNT} \
             (recoverable, no reboot)"
        ));
    }

    let usr = match parts.usr.as_ref() {
        Some(usr) => usr,
        None => {
            detach_root(tx);
            return Err("no /usr partition found on the target".to_string());
        }
    };
    let _ = tx.send(Progress::Step("Mounting cloned /usr".to_string()));
    log(tx, &format!("/usr partition: {usr}"));
    warn(
        tx,
        "/usr is dm-verity; a bare mount needs the hash+sig (veritysetup / \
         systemd-dissect). UNVERIFIED. TODO: open the verity volume before mount.",
    );
    let usr_mount = root_mount.join("usr");
    if let Err(err) = std::fs::create_dir_all(&usr_mount) {
        detach_root(tx);
        return Err(format!("could not create {}: {err}", usr_mount.display()));
    }
    if !try_mount(tx, usr, &usr_mount) {
        detach_root(tx);
        return Err("could not mount the cloned /usr; /etc seeding skipped".to_string());
    }

    seed_etc(tx, root_mount).inspect_err(|_| detach_root(tx))
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

/// Redact secret-bearing argv flags before they are logged to the progress TUI.
/// The root password hash must never appear in the on-screen log.
fn redact_arg(arg: &str) -> String {
    const SECRET_FLAGS: [&str; 2] = ["--root-password-hashed=", "--root-password="];
    for flag in SECRET_FLAGS {
        if let Some(rest) = arg.strip_prefix(flag) {
            if !rest.is_empty() {
                return format!("{flag}<redacted>");
            }
        }
    }
    arg.to_string()
}

/// Run a command, logging the invocation and its captured output. Returns true
/// on a zero exit. A spawn failure or non-zero exit is logged as a warning but
/// is never fatal (post-steps are best-effort; see [`post_steps`]).
fn run_cmd(tx: &Sender<Progress>, program: &str, args: &[&str]) -> bool {
    let shown: Vec<String> = args.iter().map(|a| redact_arg(a)).collect();
    log(tx, &format!("$ {program} {}", shown.join(" ")));
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
    use crate::firstboot::FirstbootConfig;

    fn fb() -> FirstbootConfig {
        FirstbootConfig::default()
    }

    #[test]
    fn authorize_rejects_dry_run() {
        assert!(InstallPlan::authorize(true, "/dev/sdb", "/dev/sdb", fb()).is_none());
    }

    #[test]
    fn redact_arg_hides_the_password_hash_only() {
        assert_eq!(
            redact_arg("--root-password-hashed=$6$salt$hash"),
            "--root-password-hashed=<redacted>"
        );
        assert_eq!(redact_arg("--hostname=archetype"), "--hostname=archetype");
        assert_eq!(redact_arg("--setup-machine-id"), "--setup-machine-id");
        // An empty value is not a secret to hide (and shouldn't be emitted anyway).
        assert_eq!(
            redact_arg("--root-password-hashed="),
            "--root-password-hashed="
        );
    }

    #[test]
    fn authorize_rejects_name_mismatch() {
        assert!(InstallPlan::authorize(false, "/dev/sdb", "/dev/sda", fb()).is_none());
        assert!(InstallPlan::authorize(false, "/dev/sdb", "", fb()).is_none());
        assert!(InstallPlan::authorize(false, "/dev/sdb", "/dev/sdb ", fb()).is_none());
        assert!(InstallPlan::authorize(false, "/dev/sdb", "sdb", fb()).is_none());
    }

    #[test]
    fn authorize_rejects_empty_chosen_device() {
        assert!(InstallPlan::authorize(false, "", "", fb()).is_none());
    }

    #[test]
    fn authorize_accepts_exact_match_when_not_dry_run() {
        let plan =
            InstallPlan::authorize(false, "/dev/sdb", "/dev/sdb", fb()).expect("should authorize");
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

    #[test]
    fn cryptsetup_attach_args_unlock_root_via_tpm2() {
        assert_eq!(
            cryptsetup_attach_args("/dev/sdb4"),
            [
                "attach",
                ROOT_VOLUME,
                "/dev/sdb4",
                "-",
                "tpm2-device=auto,headless=yes",
            ]
        );
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
        let plan = InstallPlan::authorize(false, &device, &device, fb())
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
