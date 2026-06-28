//! Invoke `systemd-repart` against a generated definition set.
//!
//! Phase 5 implements only the dry-run path: it asks repart to compute the
//! partition plan without writing anything. The argv is built by [`build_args`]
//! from a [`Mode`]; Phase 6 adds a `Mode::Execute` variant (and its extra
//! flags) and a sibling `execute` entry point without reworking the rest.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// The `systemd-repart` binary name; resolved via `PATH`.
const REPART_BIN: &str = "systemd-repart";

/// What `systemd-repart` should do. Dry-run computes the plan without touching
/// the device; the execute path arrives in Phase 6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `--dry-run=yes`: compute and print the plan, write nothing.
    DryRun,
}

impl Mode {
    fn dry_run_flag(self) -> &'static str {
        match self {
            Mode::DryRun => "--dry-run=yes",
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
/// using the definitions in `definitions_dir`. `--json=pretty` is always
/// requested so callers can show repart's own computed plan.
pub fn build_args(mode: Mode, definitions_dir: &Path, device: &str) -> Vec<String> {
    vec![
        mode.dry_run_flag().to_string(),
        "--empty=force".to_string(),
        format!("--definitions={}", definitions_dir.display()),
        "--json=pretty".to_string(),
        device.to_string(),
    ]
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
    fn device_is_always_the_final_argument() {
        let args = build_args(Mode::DryRun, Path::new("/tmp/defs"), "/dev/nvme0n1");
        assert_eq!(args.last().unwrap(), "/dev/nvme0n1");
    }
}
