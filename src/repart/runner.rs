//! Invoke `systemd-repart` against a generated definition set.
//!
//! Two modes share one argv builder: [`Mode::DryRun`] computes the partition
//! plan without touching the device (Phase 5, Review screen) and
//! [`Mode::Execute`] actually writes it (Phase 6, the destructive path). The
//! execute path streams repart's output line-by-line so the Progress screen can
//! render it live.
//!
//! Safety: [`execute`] simply runs repart against the device it is handed. All
//! gating (not a dry-run, the Confirm type-to-wipe match, the device being the
//! DiskSelect-chosen one) is enforced by the caller in [`crate::install`]; this
//! module never decides *whether* to run.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;

use anyhow::{Context, Result};

/// The `systemd-repart` binary name; resolved via `PATH`.
const REPART_BIN: &str = "systemd-repart";

/// What `systemd-repart` should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `--dry-run=yes`: compute and print the plan, write nothing.
    DryRun,
    /// `--dry-run=no`: write the partition table to the device. Destructive.
    Execute,
}

impl Mode {
    fn dry_run_flag(self) -> &'static str {
        match self {
            Mode::DryRun => "--dry-run=yes",
            Mode::Execute => "--dry-run=no",
        }
    }
}

/// The captured result of a `systemd-repart` run that started successfully.
/// `exit_code` is `None` when the process was terminated by a signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepartOutcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl RepartOutcome {
    /// True when repart exited cleanly (status 0).
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Build the `systemd-repart` argument vector for `mode` against `device`,
/// using the definitions in `definitions_dir`. `--empty=force` is always
/// passed: the target is wiped and repartitioned from scratch. `--json=pretty`
/// is requested only for [`Mode::DryRun`] (callers display the computed plan);
/// the execute path streams human-readable progress instead.
pub fn build_args(mode: Mode, definitions_dir: &Path, device: &str) -> Vec<String> {
    let mut args = vec![
        mode.dry_run_flag().to_string(),
        "--empty=force".to_string(),
        format!("--definitions={}", definitions_dir.display()),
    ];
    if mode == Mode::DryRun {
        args.push("--json=pretty".to_string());
    }
    args.push(device.to_string());
    args
}

/// Run `systemd-repart` in dry-run mode and capture its output. Returns an error
/// only when the process cannot be started (e.g. the binary is missing); a
/// non-zero exit is reported in the returned [`RepartOutcome`].
pub fn dry_run(definitions_dir: &Path, device: &str) -> Result<RepartOutcome> {
    let args = build_args(Mode::DryRun, definitions_dir, device);
    let output = Command::new(REPART_BIN)
        .args(&args)
        .output()
        .with_context(|| format!("failed to run {REPART_BIN}"))?;
    Ok(RepartOutcome {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Run `systemd-repart` in execute mode against `device`, forwarding every
/// output line to `on_line` as it arrives and capturing it for the returned
/// [`RepartOutcome`]. **Destructive**: this writes the partition table. The
/// caller is responsible for every safety gate (see module docs).
///
/// Returns an error only when the process cannot be started; a non-zero exit is
/// reported via [`RepartOutcome::success`].
pub fn execute(
    definitions_dir: &Path,
    device: &str,
    mut on_line: impl FnMut(&str),
) -> Result<RepartOutcome> {
    let args = build_args(Mode::Execute, definitions_dir, device);
    let mut child = Command::new(REPART_BIN)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {REPART_BIN}"))?;

    let stdout = child.stdout.take().context("repart stdout not captured")?;
    let stderr = child.stderr.take().context("repart stderr not captured")?;

    // Drain both pipes on their own threads into one ordered channel, so the
    // callback stays single-threaded and we never deadlock on a full pipe.
    let (tx, rx) = mpsc::channel::<(StreamKind, String)>();
    let out_tx = tx.clone();
    let out_reader = std::thread::spawn(move || drain(stdout, StreamKind::Stdout, &out_tx));
    let err_reader = std::thread::spawn(move || drain(stderr, StreamKind::Stderr, &tx));

    let mut captured_stdout = String::new();
    let mut captured_stderr = String::new();
    for (kind, line) in rx {
        match kind {
            StreamKind::Stdout => {
                captured_stdout.push_str(&line);
                captured_stdout.push('\n');
            }
            StreamKind::Stderr => {
                captured_stderr.push_str(&line);
                captured_stderr.push('\n');
            }
        }
        on_line(&line);
    }
    let _ = out_reader.join();
    let _ = err_reader.join();

    let status = child.wait().context("failed to wait on systemd-repart")?;
    Ok(RepartOutcome {
        exit_code: status.code(),
        stdout: captured_stdout,
        stderr: captured_stderr,
    })
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

/// Read `source` line-by-line, tagging each with `kind` and sending it on `tx`.
/// Stops at EOF; a send failure (receiver gone) ends the drain quietly.
fn drain<R: Read>(source: R, kind: StreamKind, tx: &mpsc::Sender<(StreamKind, String)>) {
    let reader = BufReader::new(source);
    for line in reader.lines() {
        match line {
            Ok(line) => {
                if tx.send((kind, line)).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_args_match_the_spike_invocation() {
        let args = build_args(
            Mode::DryRun,
            Path::new("/run/archetype-install/repart.d"),
            "/dev/sdb",
        );
        assert_eq!(
            args,
            [
                "--dry-run=yes",
                "--empty=force",
                "--definitions=/run/archetype-install/repart.d",
                "--json=pretty",
                "/dev/sdb",
            ]
        );
    }

    #[test]
    fn execute_args_request_a_real_write_without_json() {
        let args = build_args(
            Mode::Execute,
            Path::new("/run/archetype-install/repart.d"),
            "/dev/sdb",
        );
        assert_eq!(
            args,
            [
                "--dry-run=no",
                "--empty=force",
                "--definitions=/run/archetype-install/repart.d",
                "/dev/sdb",
            ]
        );
    }

    #[test]
    fn execute_never_requests_dry_run_yes() {
        let args = build_args(Mode::Execute, Path::new("/tmp/defs"), "/dev/sdb");
        assert!(args.iter().all(|arg| arg != "--dry-run=yes"));
        assert!(args.contains(&"--dry-run=no".to_string()));
    }

    #[test]
    fn device_is_always_the_final_argument() {
        for mode in [Mode::DryRun, Mode::Execute] {
            let args = build_args(mode, Path::new("/tmp/defs"), "/dev/nvme0n1");
            assert_eq!(args.last().unwrap(), "/dev/nvme0n1");
        }
    }
}
