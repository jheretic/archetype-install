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

/// device-mapper name for the verity-opened /usr; appears at /dev/mapper/<this>.
const USR_VERITY_VOLUME: &str = "archetype-usr";

/// `systemd-cryptsetup` is not on `PATH`; it ships at this fixed location.
const CRYPTSETUP_BIN: &str = "/usr/lib/systemd/systemd-cryptsetup";

/// The device-mapper name for the unlocked dm-integrity home volume; it appears
/// at `/dev/mapper/<HOME_VOLUME>`. The same name is used as the integritytab
/// volume name so the boot-time generator re-creates the identical mapper.
const HOME_VOLUME: &str = "home";

/// HMAC-SHA256 integrity key length, in bytes. integritysetup(8) caps key files
/// at 4096 bytes; 32 bytes (256 bits) matches the HMAC-SHA256 output size.
const HOME_KEY_SIZE: usize = 32;

/// dm-integrity tag size, in bytes. integritysetup(8)'s HMAC-SHA256 example uses
/// `--tag-size 32` (the full SHA-256 digest).
const HOME_TAG_SIZE: usize = 32;

/// Boot-time path of the home integrity key, relative to the installed root.
/// integritytab(5) references this absolute path; at install time the same file
/// lives under `<target>` (see [`home_key_install_path`]).
const HOME_KEY_BOOT_PATH: &str = "/etc/integritysetup-keys.d/home.key";

/// GPT partition label of the home partition (set `Label=HOME` by repart). The
/// integritytab line references it as `PARTLABEL=HOME` so the volume is found
/// independent of the device node.
const HOME_PARTLABEL: &str = "HOME";

/// A message from the install worker to the Progress screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// A new discrete step began.
    Step(String),
    /// A log line (repart output, command output, or a note).
    Line(String),
    /// The recovery key enrolled on the encrypted root. Surfaced so the UI can
    /// display it (with a QR code) and block reboot until the user saves it.
    RecoveryKey(String),
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
    /// Whether the user requested a home partition (Sizing.home is Some). The
    /// dm-integrity home setup runs only when this is true AND a HOME partition
    /// is actually found on the target.
    home_requested: bool,
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
        home_requested: bool,
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
            home_requested,
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
    // The worker sends Progress over `work_tx`. A forwarder thread tees every
    // message to a persistent log (so the record survives the TUI teardown and
    // can be read on a failed/incomplete install), then forwards to the UI
    // receiver. This keeps the 17 worker fns unchanged -- they still just send
    // Progress -- while making the install fully debuggable. NOTE: we do NOT
    // also write stderr: the installer always runs as the ratatui TUI inside
    // kmscon on tty1 (archetype-install.service), so stderr shares that terminal
    // and any write corrupts the rendered display. The log file is the debug
    // channel; read /run/archetype-install/install.log.
    let (work_tx, work_rx) = mpsc::channel::<Progress>();
    let (ui_tx, ui_rx) = mpsc::channel::<Progress>();

    thread::spawn(move || tee_progress(work_rx, ui_tx));
    let handle = thread::spawn(move || run(plan, work_tx));

    Install {
        progress: ui_rx,
        handle: Some(handle),
    }
}

/// Path of the persistent install log (under /run, cleared on reboot but
/// readable for the lifetime of a live session -- the whole point on a
/// passwordless live image where the TUI output would otherwise vanish).
const LOG_PATH: &str = "/run/archetype-install/install.log";

/// Forward every Progress message from the worker to the UI, teeing each to the
/// install log file first. Best-effort: a logging failure never blocks the
/// install or the UI. Deliberately does NOT write stderr -- see [`spawn`]: the
/// TUI owns this terminal, so stderr would corrupt the display.
fn tee_progress(work_rx: Receiver<Progress>, ui_tx: Sender<Progress>) {
    use std::io::Write;
    let mut logfile = std::fs::create_dir_all("/run/archetype-install")
        .ok()
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(LOG_PATH)
                .ok()
        });
    while let Ok(msg) = work_rx.recv() {
        if let Some(line) = render_log_line(&msg) {
            if let Some(f) = logfile.as_mut() {
                let _ = writeln!(f, "{line}");
                let _ = f.flush();
            }
        }
        // Forward to the UI; if the UI is gone, keep draining so the worker
        // doesn't block on a full channel, but logging above still records it.
        let _ = ui_tx.send(msg);
    }
}

/// One log line for a Progress message, or None for messages with no useful
/// textual form. Recovery keys are NEVER logged (secret).
fn render_log_line(msg: &Progress) -> Option<String> {
    match msg {
        Progress::Step(s) => Some(format!("==> {s}")),
        Progress::Line(l) => Some(l.clone()),
        Progress::RecoveryKey(_) => None,
        Progress::Done(outcome) => Some(format!("DONE: {outcome:?}")),
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
        .and_then(|()| write_machine_info(&plan.firstboot, root_mount, tx))
        .and_then(|()| integrity_home(plan, &parts, root_mount, tx))
        .and_then(|()| populate_esp(&parts, tx))
        .and_then(|()| {
            let root = parts
                .root
                .as_ref()
                .ok_or_else(|| "root partition vanished before recovery enrollment".to_string())?;
            enroll_recovery_key(root, tx)
        });
    if let Err(detail) = rest {
        detach_root(tx);
        return Err(detail);
    }
    Ok(())
}

/// Set up the home partition as a keyed (HMAC-SHA256) dm-integrity volume, lay
/// btrfs on the resulting mapper, and record it in `/etc/integritytab` +
/// `/etc/fstab` on the target so it is reassembled at every boot. Runs only when
/// the user requested home AND a HOME partition was actually found; a missing
/// partition when home was requested is a required-step failure. Any failure
/// past `integritysetup open` closes the mapper before returning so no open
/// integrity device is leaked onto the recovery console.
fn integrity_home(
    plan: &InstallPlan,
    parts: &TargetPartitions,
    root_mount: &Path,
    tx: &Sender<Progress>,
) -> Result<(), String> {
    if !plan.home_requested {
        return Ok(());
    }
    let _ = tx.send(Progress::Step(
        "Setting up encrypted home (dm-integrity)".to_string(),
    ));
    let home = parts.home.as_ref().ok_or_else(|| {
        "home was requested but no HOME partition was found on the target".to_string()
    })?;
    log(tx, &format!("home partition: {home}"));

    let key = generate_integrity_key()?;
    let key_path = home_key_install_path(root_mount);
    write_integrity_key(&key_path, &key)?;
    log(
        tx,
        &format!("wrote integrity key {} (0600)", key_path.display()),
    );

    let format_args = integritysetup_format_args(home, &key_path, HOME_KEY_SIZE, HOME_TAG_SIZE);
    let format_refs: Vec<&str> = format_args.iter().map(String::as_str).collect();
    if !run_cmd(tx, "integritysetup", &format_refs) {
        return Err(format!(
            "integritysetup could not format the home partition {home}"
        ));
    }

    let open_args = integritysetup_open_args(home, &key_path, HOME_KEY_SIZE, HOME_VOLUME);
    let open_refs: Vec<&str> = open_args.iter().map(String::as_str).collect();
    if !run_cmd(tx, "integritysetup", &open_refs) {
        return Err(format!(
            "integritysetup could not open the home integrity volume on {home}"
        ));
    }
    // The integrity mapper is now OPEN. Any failure below must close it.
    let mapper = format!("/dev/mapper/{HOME_VOLUME}");
    if !run_cmd(tx, "mkfs.btrfs", &[&mapper]) {
        let _ = detach_home(tx);
        return Err(format!("could not create a btrfs filesystem on {mapper}"));
    }

    if let Err(detail) = append_line(&root_mount.join("etc/integritytab"), &integritytab_line()) {
        let _ = detach_home(tx);
        return Err(detail);
    }
    if let Err(detail) = append_line(&root_mount.join("etc/fstab"), &home_fstab_line()) {
        let _ = detach_home(tx);
        return Err(detail);
    }
    // Leave the mapper closed for the rest of the install; boot re-opens it from
    // integritytab. Closing also avoids a stale mapper colliding at next boot.
    // On the success path a failed close is NOT best-effort: a left-open mapper
    // would collide at next boot, so report Incomplete.
    if !detach_home(tx) {
        return Err(format!(
            "set up home but could not close /dev/mapper/{HOME_VOLUME}; \
             close it before rebooting (recoverable, no reboot)"
        ));
    }
    Ok(())
}

/// Close the opened home integrity mapper. Returns true on success. On error
/// cleanup paths the boolean is ignored (best-effort); on the success path the
/// caller treats a failure as [`Outcome::Incomplete`].
fn detach_home(tx: &Sender<Progress>) -> bool {
    run_cmd(tx, "integritysetup", &["close", HOME_VOLUME])
}

/// Enroll a recovery key on the encrypted root via `systemd-cryptenroll
/// --recovery-key`, unlocking with the already-enrolled TPM2 keyslot. The
/// recovery key string is parsed from stdout and emitted as
/// [`Progress::RecoveryKey`] so the UI can display it (with a QR code) and block
/// reboot until the user saves it. The root is LUKS2-encrypted, so this always
/// runs; a failure leaves the system installed + encrypted but without a
/// recovery key, which is a required-step failure.
fn enroll_recovery_key(root_device: &str, tx: &Sender<Progress>) -> Result<(), String> {
    let _ = tx.send(Progress::Step(
        "Enrolling recovery key on the root".to_string(),
    ));
    let args = cryptenroll_recovery_args(root_device);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    log(tx, &format!("$ systemd-cryptenroll {}", arg_refs.join(" ")));
    let output = Command::new("systemd-cryptenroll")
        .args(&args)
        .output()
        .map_err(|err| format!("failed to run systemd-cryptenroll: {err}"))?;
    // Do NOT forward stdout (the recovery key) OR stderr (the chrome includes a
    // QR encoding of the key) to the progress log -- both would leak the key
    // into ProgressState.log, outside the dedicated Recovery screen. The key is
    // surfaced only via Progress::RecoveryKey. On failure, report the exit code
    // only (no secret), since the QR is only printed on the success path anyway.
    if !output.status.success() {
        return Err(format!(
            "systemd-cryptenroll exited {}; the root is encrypted but no recovery key was enrolled",
            output
                .status
                .code()
                .map_or_else(|| "by signal".to_string(), |c| c.to_string())
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let key = crate::recovery::parse_recovery_key(&stdout).ok_or_else(|| {
        "systemd-cryptenroll succeeded but no recovery key could be parsed from its output"
            .to_string()
    })?;
    let _ = tx.send(Progress::RecoveryKey(key));
    Ok(())
}

/// Best-effort teardown of the opened+mounted target root after a post-open
/// failure: unmount /usr and root, then detach the LUKS2 mapper so no unlocked
/// encrypted volume is left dangling. Never fatal; logged only.
fn detach_root(tx: &Sender<Progress>) {
    let _ = tx.send(Progress::Step("Tearing down target root".to_string()));
    let root_mount = Path::new(TARGET_MOUNT);
    // Order: unmount /usr, close its verity mapper, unmount root, detach LUKS.
    run_cmd(
        tx,
        "umount",
        &[&root_mount.join("usr").display().to_string()],
    );
    run_cmd(tx, "veritysetup", &["close", USR_VERITY_VOLUME]);
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

/// Build the `integritysetup format` argv for a keyed HMAC-SHA256 dm-integrity
/// volume, per integritysetup(8): the HMAC integrity algorithm requires a key
/// file and its size, plus a tag size (full SHA-256 digest). `--batch-mode`
/// suppresses the interactive wipe confirmation.
fn integritysetup_format_args(
    device: &str,
    key_path: &Path,
    key_size: usize,
    tag_size: usize,
) -> Vec<String> {
    vec![
        "format".to_string(),
        "--batch-mode".to_string(),
        "--integrity".to_string(),
        "hmac-sha256".to_string(),
        "--integrity-key-file".to_string(),
        key_path.display().to_string(),
        "--integrity-key-size".to_string(),
        key_size.to_string(),
        "--tag-size".to_string(),
        tag_size.to_string(),
        device.to_string(),
    ]
}

/// Build the `integritysetup open` argv. integritysetup(8): a non-default
/// integrity algorithm is NOT detected from the device, so `--integrity` plus
/// the same key file and size must be repeated. Maps the device to
/// `/dev/mapper/<volume>`.
fn integritysetup_open_args(
    device: &str,
    key_path: &Path,
    key_size: usize,
    volume: &str,
) -> Vec<String> {
    vec![
        "open".to_string(),
        "--integrity".to_string(),
        "hmac-sha256".to_string(),
        "--integrity-key-file".to_string(),
        key_path.display().to_string(),
        "--integrity-key-size".to_string(),
        key_size.to_string(),
        device.to_string(),
        volume.to_string(),
    ]
}

/// The `/etc/integritytab` line for the home volume. integritytab(5): with a key
/// file present the algorithm defaults to hmac-sha256; the options select bitmap
/// mode and discards. Newline-terminated. Fields are whitespace-delimited.
fn integritytab_line() -> String {
    format!("{HOME_VOLUME}\tPARTLABEL={HOME_PARTLABEL}\t{HOME_KEY_BOOT_PATH}\tallow-discards,mode=bitmap\n")
}

/// The `/etc/fstab` line that mounts the home integrity mapper at `/home`. The
/// integritysetup-generator(8) sets up `/dev/mapper/home` from integritytab
/// before `local-fs.target`, so a plain fstab line suffices; `x-systemd.requires`
/// is added defensively to make the ordering dependency explicit. Newline-
/// terminated.
fn home_fstab_line() -> String {
    format!(
        "/dev/mapper/{HOME_VOLUME}\t/home\tbtrfs\tdefaults,x-systemd.requires=/dev/mapper/{HOME_VOLUME}\t0\t0\n"
    )
}

/// Build the `systemd-cryptenroll --recovery-key` argv. The root already carries
/// a TPM2 keyslot; `--unlock-tpm2-device=auto` (systemd-cryptenroll(1)) unlocks
/// via the local TPM2 non-interactively so no passphrase prompt is needed.
fn cryptenroll_recovery_args(root_device: &str) -> Vec<String> {
    vec![
        "--recovery-key".to_string(),
        "--unlock-tpm2-device=auto".to_string(),
        root_device.to_string(),
    ]
}

/// The install-time path of the home integrity key under the mounted target,
/// i.e. `<target>/etc/integritysetup-keys.d/home.key`. The boot-time path
/// ([`HOME_KEY_BOOT_PATH`]) is the same file relative to the installed root.
fn home_key_install_path(root_mount: &Path) -> PathBuf {
    root_mount.join(HOME_KEY_BOOT_PATH.trim_start_matches('/'))
}

/// Generate a cryptographically secure integrity key by reading [`HOME_KEY_SIZE`]
/// bytes from `/dev/urandom`. No `rand` dependency: `/dev/urandom` is the kernel
/// CSPRNG and is always present on the installer image.
fn generate_integrity_key() -> Result<Vec<u8>, String> {
    let mut key = vec![0u8; HOME_KEY_SIZE];
    let mut file = std::fs::File::open("/dev/urandom")
        .map_err(|err| format!("could not open /dev/urandom: {err}"))?;
    std::io::Read::read_exact(&mut file, &mut key)
        .map_err(|err| format!("could not read random bytes: {err}"))?;
    Ok(key)
}

/// Write the integrity key to `path` with mode 0600, creating its parent
/// directory. Root-owned by virtue of the installer running as root.
fn write_integrity_key(path: &Path, key: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("could not create {}: {err}", parent.display()))?;

    // Write to a fresh 0600 temp file, fsync it, then rename over the target and
    // fsync the directory. create_new guarantees the secret never lands in a
    // pre-existing file with wider permissions, and the fsyncs make the key
    // durable BEFORE integritysetup formats home with it (a crash must not leave
    // home formatted against a key that was only in page cache).
    let tmp = path.with_extension("key.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|err| format!("could not create {}: {err}", tmp.display()))?;
    file.write_all(key)
        .and_then(|()| file.sync_all())
        .map_err(|err| format!("could not write {}: {err}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, path)
        .map_err(|err| format!("could not install {}: {err}", path.display()))?;
    // Best-effort directory fsync so the rename itself is durable.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Append a line to a file, creating it (and its parent directory) if absent.
fn append_line(path: &Path, line: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
    }
    // If the file exists and does not end in a newline (a seeded /etc/fstab or
    // /etc/integritytab might not), prepend one so the new record can't join the
    // previous line and be mis-parsed at boot.
    let needs_separator = match std::fs::read(path) {
        Ok(bytes) => !bytes.is_empty() && bytes.last() != Some(&b'\n'),
        Err(_) => false,
    };
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|err| format!("could not open {}: {err}", path.display()))?;
    let payload = if needs_separator {
        format!("\n{line}")
    } else {
        line.to_string()
    };
    std::io::Write::write_all(&mut file, payload.as_bytes())
        .map_err(|err| format!("could not append to {}: {err}", path.display()))
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

    // Create a per-version root subvolume @archetype_<version> and seed EVERYTHING
    // into it (not the btrfs top-level). The version is the running image's
    // IMAGE_VERSION == the version whose UKI we install, and that UKI is built
    // with rootflags=subvol=@archetype_<version> (mkosi build), so gpt-auto mounts
    // this exact subvolume as / at boot. `miz -Iu` later snapshots it to
    // @archetype_<newversion> for the new UKI, so each UKI is pinned to a matching
    // root snapshot -- a /usr rollback brings the layered packages that match its
    // deps. We do NOT btrfs-set-default: rootflags= on each UKI's cmdline is the
    // pinning (a global default would fight a rollback).
    let version = read_image_version().ok_or_else(|| {
        "could not read IMAGE_VERSION from /usr/lib/os-release; cannot name the \
         root subvolume to match the UKI's rootflags=subvol="
            .to_string()
    })?;
    let subvol = root_subvol_name(&version);

    // Mount the btrfs top-level to create the subvolume, then remount the
    // subvolume itself at root_mount as the seed target.
    if !try_mount(tx, &opened, root_mount) {
        detach_root(tx);
        return Err(format!(
            "unlocked root {opened} but could not mount it at {TARGET_MOUNT} \
             (recoverable, no reboot)"
        ));
    }
    let subvol_path = root_mount.join(&subvol);
    if !run_cmd(
        tx,
        "btrfs",
        &["subvolume", "create", &subvol_path.display().to_string()],
    ) {
        run_cmd(tx, "umount", &[&root_mount.display().to_string()]);
        run_cmd(tx, CRYPTSETUP_BIN, &["detach", ROOT_VOLUME]);
        return Err(format!("could not create root subvolume {subvol}"));
    }
    log(tx, &format!("created root subvolume {subvol}"));
    run_cmd(tx, "umount", &[&root_mount.display().to_string()]);
    if !try_mount_opts(tx, &opened, root_mount, &format!("subvol={subvol}")) {
        detach_root(tx);
        return Err(format!(
            "created subvolume {subvol} but could not mount it at {TARGET_MOUNT} \
             (recoverable, no reboot)"
        ));
    }

    // The target root is a freshly-formatted, EMPTY btrfs (70-root.conf has no
    // CopyFiles). systemd's switch-root moves the API filesystems (/dev /proc
    // /sys /run) onto the new root and requires those mountpoints to already
    // exist there (switch-root.c chase()s each without CHASE_NONEXISTENT) -- an
    // empty root makes switch-root fail with "Failed to resolve /sysroot/dev"
    // and the machine reboot-loops. The live image sidesteps this because its
    // root is root=tmpfs, built by PID1. A persistent disk root must carry the
    // skeleton, so create it here before seeding /etc.
    if let Err(detail) = create_root_skeleton(root_mount) {
        detach_root(tx);
        return Err(detail);
    }

    let usr = match parts.usr.as_ref() {
        Some(usr) => usr,
        None => {
            detach_root(tx);
            return Err("no /usr partition found on the target".to_string());
        }
    };
    let _ = tx.send(Progress::Step(
        "Opening cloned /usr (dm-verity)".to_string(),
    ));
    log(tx, &format!("/usr data partition: {usr}"));

    // Open /usr through dm-verity (integrity-checked) rather than bare-mounting
    // the data partition. veritysetup open <data> <name> <hash> <roothash>
    // verifies every block against the Merkle tree rooted at <roothash>. The
    // cloned /usr is a block-for-block CopyBlocks=auto of the RUNNING /usr, so
    // its root hash is the live system's usrhash= (on /proc/cmdline) and its
    // hash tree is the cloned usr-verity partition.
    let hash_dev = match parts.usr_verity.as_ref() {
        Some(h) => h,
        None => {
            detach_root(tx);
            return Err("no /usr verity (hash) partition found on the target".to_string());
        }
    };
    let roothash = match read_usrhash() {
        Some(h) => h,
        None => {
            detach_root(tx);
            return Err(
                "could not read usrhash= from /proc/cmdline; cannot verity-open /usr".to_string(),
            );
        }
    };
    log(tx, &format!("/usr verity hash partition: {hash_dev}"));
    // Clear a stale mapper from an earlier aborted run (would block open).
    if Path::new(&format!("/dev/mapper/{USR_VERITY_VOLUME}")).exists() {
        warn(
            tx,
            &format!("stale /dev/mapper/{USR_VERITY_VOLUME}; closing first"),
        );
        run_cmd(tx, "veritysetup", &["close", USR_VERITY_VOLUME]);
    }
    if !run_cmd(
        tx,
        "veritysetup",
        &["open", usr, USR_VERITY_VOLUME, hash_dev, &roothash],
    ) {
        detach_root(tx);
        return Err(format!(
            "veritysetup open failed for /usr ({usr}); the root hash may not \
             match the cloned hash partition (recoverable, no reboot)"
        ));
    }
    // /usr verity is now OPEN; teardown must close it (detach_root does).
    let usr_mapper = format!("/dev/mapper/{USR_VERITY_VOLUME}");
    let usr_mount = root_mount.join("usr");
    if let Err(err) = std::fs::create_dir_all(&usr_mount) {
        detach_root(tx);
        return Err(format!("could not create {}: {err}", usr_mount.display()));
    }
    if !try_mount(tx, &usr_mapper, &usr_mount) {
        detach_root(tx);
        return Err(format!(
            "verity-opened /usr {usr_mapper} but could not mount it; /etc seeding skipped"
        ));
    }

    seed_etc(tx, root_mount).inspect_err(|_| detach_root(tx))
}

/// The running system's `usrhash=` (verity root hash for /usr) from
/// `/proc/cmdline`. The cloned /usr shares it (block-for-block clone of the
/// running /usr). Returns the hex string, or None if absent/unreadable.
fn read_usrhash() -> Option<String> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").ok()?;
    parse_usrhash(&cmdline)
}

/// Extract `usrhash=<hex>` from a kernel cmdline string. Pure, for testing.
fn parse_usrhash(cmdline: &str) -> Option<String> {
    cmdline
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("usrhash=").map(str::to_string))
        .filter(|h| !h.is_empty())
}

/// Top-level root entries the installer never replicates onto the target:
/// `usr` is mounted separately (dm-verity); `etc` is seeded from the factory
/// tree by [`seed_etc`]; `home` is created by [`integrity_home`]. Everything
/// else at `/` (the `filesystem` package's dirs `dev proc sys run tmp var` and
/// the usr-merge symlinks `bin lib lib64 sbin`) must be replicated.
const ROOT_SKELETON_SKIP: [&str; 3] = ["usr", "etc", "home"];

/// Replicate the live root's top-level skeleton onto the freshly-formatted,
/// EMPTY target btrfs so switch-root can move the API filesystems (/dev /proc
/// /sys /run) onto it -- systemd's switch-root requires those mountpoints to
/// pre-exist (switch-root.c chase()s each without CHASE_NONEXISTENT), and an
/// empty root reboot-loops with "Failed to resolve /sysroot/dev". It also needs
/// the usr-merge symlinks (/bin -> usr/bin, /lib, /lib64, /sbin) or PID1 on the
/// new root can't resolve interpreter/binary paths.
///
/// The running installer IS on the exact root layout the installed system
/// needs, so we mirror `/` directly (ground truth, no drift from the
/// `filesystem` package): symlinks copied verbatim, directories created empty
/// (they are mountpoints or tmpfs-populated at runtime). `usr`/`etc`/`home` are
/// skipped (handled elsewhere).
fn create_root_skeleton(root_mount: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir("/")
        .map_err(|err| format!("could not read the live root skeleton at /: {err}"))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("could not read a / entry: {err}"))?;
        let name = entry.file_name();
        if ROOT_SKELETON_SKIP
            .iter()
            .any(|skip| name.as_os_str() == *skip)
        {
            continue;
        }
        let meta = entry
            .path()
            .symlink_metadata()
            .map_err(|err| format!("could not stat /{}: {err}", name.to_string_lossy()))?;
        let dest = root_mount.join(&name);
        if meta.is_symlink() {
            let target = std::fs::read_link(entry.path()).map_err(|err| {
                format!("could not read symlink /{}: {err}", name.to_string_lossy())
            })?;
            // Recreate the symlink verbatim; ignore if it already exists.
            if !dest.exists() {
                std::os::unix::fs::symlink(&target, &dest)
                    .map_err(|err| format!("could not create symlink {}: {err}", dest.display()))?;
            }
        } else if meta.is_dir() {
            std::fs::create_dir_all(&dest)
                .map_err(|err| format!("could not create root dir {}: {err}", dest.display()))?;
        }
    }
    Ok(())
}

/// `systemd-tmpfiles --root=<target> --boot --create`: seed persistent `/etc`
/// from `/usr/share/factory/etc` (per 70-root.conf).
fn seed_etc(tx: &Sender<Progress>, root_mount: &Path) -> Result<(), String> {
    let _ = tx.send(Progress::Step("Seeding /etc from factory".to_string()));
    let code = run_cmd_code(
        tx,
        "systemd-tmpfiles",
        &[
            &format!("--root={}", root_mount.display()),
            "--boot",
            "--create",
        ],
    );
    interpret_tmpfiles_exit(code)
}

/// Interpret a `systemd-tmpfiles` exit code for the offline /etc seed.
///
/// Exit 65 (EX_DATAERR) = "some lines had to be ignored, but no other errors
/// occurred" (systemd-tmpfiles(8)). On an offline --root seed this is normal and
/// benign: tmpfiles.d rules reference users/groups not yet present in the target
/// (audio, kvm, systemd-network, utmp, tss, ...) and factory sources for foreign
/// C! rules (mtab, ssl/cert.pem, ca-*) we don't ship. Our own /etc defaults are
/// still created, and systemd's own systemd-tmpfiles-setup.service does not fail
/// the boot on 65 -- so neither do we. Only a hard failure (73 = EX_CANTCREAT,
/// or 1) or death by signal is fatal. Pure for unit-testing.
fn interpret_tmpfiles_exit(code: Option<i32>) -> Result<(), String> {
    match code {
        Some(0) | Some(65) => Ok(()),
        Some(c) => Err(format!(
            "systemd-tmpfiles failed to seed /etc from the factory tree (exit {c})"
        )),
        None => Err("systemd-tmpfiles was killed by a signal while seeding /etc".to_string()),
    }
}

/// Where the target ESP is mounted while we populate it. Under /run.
const TARGET_ESP_MOUNT: &str = "/run/archetype-install/esp";

/// Discover where the LIVE system's ESP is mounted. systemd doesn't fix this to
/// one path: `bootctl --print-esp-path` reports the real mount, and the
/// conventional locations are /efi, /boot, /boot/efi (bootctl's own search
/// order). Prefer bootctl; fall back to the first of those that has an EFI/ dir.
fn find_live_esp() -> Option<String> {
    if let Ok(out) = Command::new("bootctl").arg("--print-esp-path").output() {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() && Path::new(&p).join("EFI").is_dir() {
                return Some(p);
            }
        }
    }
    for cand in ["/efi", "/boot", "/boot/efi"] {
        if Path::new(cand).join("EFI").is_dir() {
            return Some(cand.to_string());
        }
    }
    None
}

/// Populate the target ESP so the installed system is bootable.
///
/// The installer's repart ESP template (repart.sysinstall.d/00-efi.conf) creates
/// an EMPTY vfat ESP -- unlike the live image's build-time ESP, it has no
/// CopyFiles. So nothing put the bootloader/UKI on the target, and the firmware
/// found no boot device (the spike's PLAN section 13 Q4, now resolved: yes, the
/// installer must populate the ESP). Copy the live ESP's contents
/// (systemd-boot under EFI/, loader/, the UKI under EFI/Linux/) to the target,
/// EXCLUDING installer.addon.efi -- that addon forces systemd.unit=
/// system-install.target, so an installed system carrying it would boot back
/// into the installer. Then bootctl install lays down the EFI/BOOT/BOOTX64.EFI
/// removable fallback + a boot entry. Required step: a failure here means an
/// unbootable install, so it returns Err (-> Outcome::Incomplete).
fn populate_esp(parts: &TargetPartitions, tx: &Sender<Progress>) -> Result<(), String> {
    let _ = tx.send(Progress::Step(
        "Populating ESP (bootloader + UKI)".to_string(),
    ));
    let esp = parts
        .esp
        .as_ref()
        .ok_or_else(|| "no ESP partition found on the target".to_string())?;

    let live_esp = find_live_esp()
        .ok_or_else(|| "could not locate the live ESP (bootctl/efi/boot)".to_string())?;
    log(tx, &format!("live ESP: {live_esp}"));

    let mnt = Path::new(TARGET_ESP_MOUNT);
    std::fs::create_dir_all(mnt)
        .map_err(|e| format!("could not create {TARGET_ESP_MOUNT}: {e}"))?;
    if !try_mount(tx, esp, mnt) {
        return Err(format!("could not mount the target ESP {esp}"));
    }

    // Copy the live ESP -> target ESP. cp -a --no-target-directory lands the
    // live ESP's CONTENTS (EFI/, loader/) at the target root (verified), then we
    // delete the installer addon (cp has no --exclude) so the installed system
    // doesn't re-enter the installer.
    let dst = mnt.display().to_string();
    let copied = run_cmd(tx, "cp", &["-a", "--no-target-directory", &live_esp, &dst]);
    // Remove the addon if it rode along (cp has no --exclude; delete after).
    let addon = mnt.join("loader/addons/installer.addon.efi");
    if addon.exists() {
        let _ = std::fs::remove_file(&addon);
        log(tx, "removed installer.addon.efi from target ESP");
    }
    if !copied {
        let _ = umount(tx, mnt);
        return Err("failed to copy the bootloader/UKI to the target ESP".to_string());
    }

    // bootctl install: write the removable fallback EFI/BOOT/BOOTX64.EFI + a
    // systemd-boot entry into the target ESP. --no-variables: don't touch the
    // live firmware's NVRAM boot order (the target boots via the fallback path).
    if !run_cmd(
        tx,
        "bootctl",
        &["install", "--esp-path", &dst, "--no-variables"],
    ) {
        warn(
            tx,
            "bootctl install reported an error; the copied EFI/ tree may still \
             boot via the removable fallback, but verify on the target.",
        );
    }

    // Install the per-version rootflags addon so the installed UKI pins its
    // matching @archetype_<version> root subvolume. rootflags=subvol= is NOT in
    // the base UKI cmdline (it would break the live boot's root=tmpfs, see
    // archetype-build mkosi.conf); systemd-stub applies addons in
    // EFI/Linux/<uki>.efi.extra.d/ ONLY to that UKI. The signed addon is baked
    // into /usr (mounted at TARGET_MOUNT/usr); copy it beside the UKI we just
    // wrote. Non-fatal-but-loud: without it, gpt-auto mounts the btrfs default
    // (top-level, not a seeded @archetype_<v>) -> broken boot, so warn clearly.
    if let Err(detail) = install_rootflags_addon(tx, mnt) {
        warn(
            tx,
            &format!("could not install the rootflags addon: {detail}"),
        );
    }

    let _ = umount(tx, mnt);

    // Mount the ESP at /efi on the INSTALLED system. The ESP is populated above,
    // but nothing mounts it at runtime: gpt-auto's ESP automount is conditional
    // (LoaderDevicePartUUID must match + /efi|/boot empty) and does not fire on
    // our layout, so without an explicit fstab entry the ESP stays unmounted.
    // systemd-sysupdate's UKI transfer (90-uki.transfer, PathRelativeTo=esp)
    // then can't resolve the ESP -> "Required key not available" -> no host
    // target -> `miz -I` "no such component: host". An fstab mount by the ESP's
    // stable PARTLABEL (00-efi.conf Label=archetype-esp) fixes it, and gpt-auto
    // defers to fstab. The mountpoint /efi doesn't exist on the target yet (the
    // live root has none to mirror), so create it; systemd would also create it,
    // but being explicit is harmless.
    let root_mount = Path::new(TARGET_MOUNT);
    if let Err(err) = std::fs::create_dir_all(root_mount.join("efi")) {
        warn(tx, &format!("could not create /efi mountpoint: {err}"));
    }
    if let Err(detail) = append_line(&root_mount.join("etc/fstab"), &esp_fstab_line()) {
        // Non-fatal: the install is otherwise complete and bootable; only image
        // updates (miz -Iu) need the ESP mounted. Surface it, don't abort.
        warn(
            tx,
            &format!("could not write the ESP fstab entry: {detail}"),
        );
    }
    Ok(())
}

/// GPT PARTLABEL assigned to the ESP by the installer's repart config
/// (00-efi.conf `Label=archetype-esp`). Used to mount it stably via fstab.
const ESP_PARTLABEL: &str = "archetype-esp";

/// The rootflags addon baked into /usr (archetype-build scripts/rootflags_addon.sh),
/// relative to the mounted target root. Copied onto the target ESP so the
/// installed UKI pins its @archetype_<version> root subvolume.
const ROOTFLAGS_ADDON_SRC: &str = "usr/lib/archetype/rootflags.addon.efi";

/// Copy the per-version rootflags addon into the target ESP's
/// `EFI/Linux/archetype_<version>.efi.extra.d/`. `esp_mnt` is the mounted target
/// ESP. The source is the addon baked into the (mounted) target /usr; the
/// version names the extra.d dir to match the UKI written beside it (systemd-stub
/// resolves `<uki>.efi.extra.d/` for `archetype_<version>.efi`).
fn install_rootflags_addon(tx: &Sender<Progress>, esp_mnt: &Path) -> Result<(), String> {
    let version = read_image_version().ok_or_else(|| "could not read IMAGE_VERSION".to_string())?;
    let src = Path::new(TARGET_MOUNT).join(ROOTFLAGS_ADDON_SRC);
    if !src.exists() {
        return Err(format!("addon not found at {}", src.display()));
    }
    let extra_d = esp_mnt.join(format!("EFI/Linux/archetype_{version}.efi.extra.d"));
    std::fs::create_dir_all(&extra_d)
        .map_err(|e| format!("could not create {}: {e}", extra_d.display()))?;
    let dst = extra_d.join("rootflags.addon.efi");
    std::fs::copy(&src, &dst)
        .map_err(|e| format!("could not copy addon to {}: {e}", dst.display()))?;
    log(
        tx,
        &format!("installed rootflags addon -> {}", dst.display()),
    );
    Ok(())
}

/// The `/etc/fstab` line mounting the ESP at `/efi` on the installed system, by
/// its stable PARTLABEL. `umask=0077` keeps the vfat ESP root-only (it holds
/// boot artifacts). `nofail` so a missing/again-unmounted ESP never blocks boot;
/// `x-systemd.automount` mounts it lazily on first access (e.g. by sysupdate).
///
/// PARTLABEL (not PARTUUID) is deterministic and needs no runtime lookup, at the
/// cost of ambiguity if two Archetype disks are attached at once -- acceptable
/// for the single-install target; revisit with the target ESP's PARTUUID if
/// multi-disk installs become a concern.
fn esp_fstab_line() -> String {
    format!("PARTLABEL={ESP_PARTLABEL}\t/efi\tvfat\tumask=0077,nofail,x-systemd.automount\t0\t2\n")
}

/// `umount <path>`, logged. Returns true on success.
fn umount(tx: &Sender<Progress>, path: &Path) -> bool {
    run_cmd(tx, "umount", &[&path.display().to_string()])
}

/// Attempt `mount <source> <target>`, logging the result. Returns true on a
/// clean mount.
fn try_mount(tx: &Sender<Progress>, source: &str, target: &Path) -> bool {
    run_cmd(tx, "mount", &[source, &target.display().to_string()])
}

/// `mount -o <opts> <source> <target>`. Used to mount a specific btrfs
/// subvolume (`subvol=...`) as the target root.
fn try_mount_opts(tx: &Sender<Progress>, source: &str, target: &Path, opts: &str) -> bool {
    run_cmd(
        tx,
        "mount",
        &["-o", opts, source, &target.display().to_string()],
    )
}

/// The running image's `IMAGE_VERSION` from `/usr/lib/os-release` -- the version
/// being installed (the installer clones the running /usr). Names the root
/// subvolume to match the installed UKI's baked-in rootflags=subvol=.
fn read_image_version() -> Option<String> {
    let text = std::fs::read_to_string("/usr/lib/os-release")
        .or_else(|_| std::fs::read_to_string("/etc/os-release"))
        .ok()?;
    parse_image_version(&text)
}

/// Extract `IMAGE_VERSION=<v>` from os-release text (optionally quoted). Pure,
/// for testing.
fn parse_image_version(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("IMAGE_VERSION="))
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

/// The root subvolume name for `version`: `@archetype_<version>`. MUST match the
/// UKI's baked-in `rootflags=subvol=` (built in archetype-build) and miz -Iu's
/// snapshot naming. Pure, for testing.
fn root_subvol_name(version: &str) -> String {
    format!("@archetype_{version}")
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
    run_cmd_code(tx, program, args) == Some(0)
}

/// Like [`run_cmd`] but returns the process exit code: `Some(code)`, or `None`
/// if it was killed by a signal or could not be spawned. Lets a caller accept
/// specific non-zero codes (e.g. systemd-tmpfiles' benign 65). Logs the argv,
/// captured output, and a warning on any non-zero/abnormal exit.
fn run_cmd_code(tx: &Sender<Progress>, program: &str, args: &[&str]) -> Option<i32> {
    let shown: Vec<String> = args.iter().map(|a| redact_arg(a)).collect();
    log(tx, &format!("$ {program} {}", shown.join(" ")));
    match Command::new(program).args(args).output() {
        Ok(output) => {
            for stream in [&output.stdout, &output.stderr] {
                for line in String::from_utf8_lossy(stream).lines() {
                    log(tx, &format!("  {line}"));
                }
            }
            if !output.status.success() {
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
            }
            output.status.code()
        }
        Err(err) => {
            warn(tx, &format!("failed to run {program}: {err}"));
            None
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
    usr_verity: Option<String>,
    home: Option<String>,
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
            // Verity hash partition first (more specific) so the data arm's
            // !verity guard isn't needed to disambiguate, but keep both clear.
            k if k.starts_with("Linux /usr")
                && k.contains("verity")
                && !k.contains("signature") =>
            {
                &mut parts.usr_verity
            }
            k if k.starts_with("Linux /usr") && !k.contains("verity") => &mut parts.usr,
            "Linux home" => &mut parts.home,
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
        assert!(InstallPlan::authorize(true, "/dev/sdb", "/dev/sdb", fb(), true).is_none());
        // Dry-run is rejected regardless of the home flag, so the integrity and
        // recovery steps are structurally unreachable in dry-run.
        assert!(InstallPlan::authorize(true, "/dev/sdb", "/dev/sdb", fb(), false).is_none());
    }

    #[test]
    fn parse_image_version_and_subvol_name() {
        assert_eq!(
            parse_image_version("NAME=x\nIMAGE_VERSION=2026.07.01-7\nID=archetype\n").as_deref(),
            Some("2026.07.01-7")
        );
        assert_eq!(
            parse_image_version("IMAGE_VERSION=\"2026.07.01-7\"\n").as_deref(),
            Some("2026.07.01-7")
        );
        assert_eq!(parse_image_version("NAME=x\nID=archetype\n"), None);
        assert_eq!(parse_image_version("IMAGE_VERSION=\n"), None);
        assert_eq!(root_subvol_name("2026.07.01-7"), "@archetype_2026.07.01-7");
    }

    #[test]
    fn parse_usrhash_extracts_the_token() {
        assert_eq!(
            parse_usrhash("root=tmpfs usrhash=4aa6a2af5d1e lsm=apparmor").as_deref(),
            Some("4aa6a2af5d1e")
        );
        assert_eq!(parse_usrhash("root=tmpfs lsm=apparmor"), None);
        assert_eq!(parse_usrhash("usrhash="), None);
    }

    #[test]
    fn tmpfiles_exit_65_is_tolerated() {
        // 0 and 65 (some lines ignored, no other errors) => seed OK.
        assert!(interpret_tmpfiles_exit(Some(0)).is_ok());
        assert!(interpret_tmpfiles_exit(Some(65)).is_ok());
        // 73 (EX_CANTCREAT), 1, and signal death => fatal.
        assert!(interpret_tmpfiles_exit(Some(73)).is_err());
        assert!(interpret_tmpfiles_exit(Some(1)).is_err());
        assert!(interpret_tmpfiles_exit(None).is_err());
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
        assert!(InstallPlan::authorize(false, "/dev/sdb", "/dev/sda", fb(), true).is_none());
        assert!(InstallPlan::authorize(false, "/dev/sdb", "", fb(), true).is_none());
        assert!(InstallPlan::authorize(false, "/dev/sdb", "/dev/sdb ", fb(), true).is_none());
        assert!(InstallPlan::authorize(false, "/dev/sdb", "sdb", fb(), true).is_none());
    }

    #[test]
    fn authorize_rejects_empty_chosen_device() {
        assert!(InstallPlan::authorize(false, "", "", fb(), true).is_none());
    }

    #[test]
    fn authorize_accepts_exact_match_when_not_dry_run() {
        let plan = InstallPlan::authorize(false, "/dev/sdb", "/dev/sdb", fb(), true)
            .expect("should authorize");
        assert_eq!(plan.device, "/dev/sdb");
        assert_eq!(plan.definitions_dir, PathBuf::from(OUTPUT_DIR));
        assert!(plan.home_requested);
    }

    #[test]
    fn authorize_records_home_requested_flag() {
        let plan = InstallPlan::authorize(false, "/dev/sdb", "/dev/sdb", fb(), false)
            .expect("should authorize");
        assert!(!plan.home_requested);
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
        // The verity partition must not be picked as the /usr data partition;
        // it is captured separately as the hash device for veritysetup open.
        assert_eq!(parts.usr.as_deref(), Some("/dev/sdb2"));
        assert_eq!(parts.usr_verity.as_deref(), Some("/dev/sdb3"));
        // No home partition in this layout (home omitted by the user).
        assert_eq!(parts.home, None);
    }

    #[test]
    fn parse_partitions_classifies_home() {
        let json = r#"{
           "blockdevices": [
              {
                 "path": "/dev/sdb", "parttypename": null,
                 "children": [
                    {"path": "/dev/sdb1", "parttypename": "EFI System"},
                    {"path": "/dev/sdb4", "parttypename": "Linux root (x86-64)"},
                    {"path": "/dev/sdb6", "parttypename": "Linux home"}
                 ]
              }
           ]
        }"#;
        let parts = parse_partitions(json).unwrap();
        assert_eq!(parts.home.as_deref(), Some("/dev/sdb6"));
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

    #[test]
    fn integritysetup_format_args_keyed_hmac_sha256() {
        let args = integritysetup_format_args(
            "/dev/sdb6",
            Path::new("/run/k/home.key"),
            HOME_KEY_SIZE,
            HOME_TAG_SIZE,
        );
        assert_eq!(
            args,
            [
                "format",
                "--batch-mode",
                "--integrity",
                "hmac-sha256",
                "--integrity-key-file",
                "/run/k/home.key",
                "--integrity-key-size",
                "32",
                "--tag-size",
                "32",
                "/dev/sdb6",
            ]
        );
    }

    #[test]
    fn integritysetup_open_args_repeat_integrity_and_key() {
        let args = integritysetup_open_args(
            "/dev/sdb6",
            Path::new("/run/k/home.key"),
            HOME_KEY_SIZE,
            HOME_VOLUME,
        );
        assert_eq!(
            args,
            [
                "open",
                "--integrity",
                "hmac-sha256",
                "--integrity-key-file",
                "/run/k/home.key",
                "--integrity-key-size",
                "32",
                "/dev/sdb6",
                "home",
            ]
        );
    }

    #[test]
    fn integritytab_line_matches_target_format() {
        assert_eq!(
            integritytab_line(),
            "home\tPARTLABEL=HOME\t/etc/integritysetup-keys.d/home.key\tallow-discards,mode=bitmap\n"
        );
    }

    #[test]
    fn home_fstab_line_mounts_mapper_at_home() {
        assert_eq!(
            home_fstab_line(),
            "/dev/mapper/home\t/home\tbtrfs\tdefaults,x-systemd.requires=/dev/mapper/home\t0\t0\n"
        );
    }

    #[test]
    fn esp_fstab_line_mounts_esp_at_efi_by_partlabel() {
        assert_eq!(
            esp_fstab_line(),
            "PARTLABEL=archetype-esp\t/efi\tvfat\tumask=0077,nofail,x-systemd.automount\t0\t2\n"
        );
    }

    #[test]
    fn cryptenroll_recovery_args_unlock_via_tpm2() {
        assert_eq!(
            cryptenroll_recovery_args("/dev/sdb4"),
            ["--recovery-key", "--unlock-tpm2-device=auto", "/dev/sdb4",]
        );
    }

    #[test]
    fn home_key_install_path_joins_under_target() {
        assert_eq!(
            home_key_install_path(Path::new("/run/archetype-install/target")),
            PathBuf::from("/run/archetype-install/target/etc/integritysetup-keys.d/home.key")
        );
    }

    #[test]
    fn generate_integrity_key_is_right_size_and_varies() {
        let a = generate_integrity_key().expect("/dev/urandom should be readable");
        let b = generate_integrity_key().expect("/dev/urandom should be readable");
        assert_eq!(a.len(), HOME_KEY_SIZE);
        assert_eq!(b.len(), HOME_KEY_SIZE);
        assert_ne!(a, b, "two CSPRNG draws should differ");
    }

    #[test]
    fn write_integrity_key_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("archetype-key-test-{}", std::process::id()));
        let path = dir.join("etc/integritysetup-keys.d/home.key");
        write_integrity_key(&path, &[0u8; HOME_KEY_SIZE]).expect("should write key");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_line_creates_and_appends() {
        let dir =
            std::env::temp_dir().join(format!("archetype-append-test-{}", std::process::id()));
        let path = dir.join("etc/integritytab");
        append_line(&path, "one\n").unwrap();
        append_line(&path, "two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_line_inserts_separator_when_file_lacks_trailing_newline() {
        let dir =
            std::env::temp_dir().join(format!("archetype-append-nl-test-{}", std::process::id()));
        let path = dir.join("etc/fstab");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A seeded file with no trailing newline.
        std::fs::write(&path, "existing line").unwrap();
        append_line(&path, "new line\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "existing line\nnew line\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        let plan = InstallPlan::authorize(false, &device, &device, fb(), true)
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
