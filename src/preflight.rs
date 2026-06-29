//! Startup TPM2 preflight. Archetype enrolls the root partition's LUKS keyslot
//! against the local TPM2 (no key-file fallback), so a usable TPM2 is mandatory
//! before the wizard touches anything.
//!
//! `systemd-creds has-tpm2` is the authoritative check: exit 0 means full TPM2
//! support; any non-zero exit means missing or partial support. The command
//! also prints a short summary (firmware/driver/system lines, or a `+firmware
//! +driver ...` style on older systemd) which we capture verbatim for display.
//!
//! The Command run is split from the interpretation so the latter is unit
//! testable without a real TPM (see [`interpret`]).

use std::process::Command;

const CREDS_BIN: &str = "systemd-creds";

/// Outcome of the TPM2 preflight check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreflightResult {
    /// True when a fully usable TPM2 is present (the check exited 0).
    pub ok: bool,
    /// Trimmed `systemd-creds` output (stdout + stderr), or the spawn error,
    /// shown to the operator on failure.
    pub detail: String,
}

/// The argv passed to [`CREDS_BIN`].
fn args() -> [&'static str; 1] {
    ["has-tpm2"]
}

/// Run `systemd-creds has-tpm2` and interpret the result. A spawn failure (e.g.
/// the binary is absent) is reported as not-ok with the error as detail.
pub fn check() -> PreflightResult {
    match Command::new(CREDS_BIN).args(args()).output() {
        Ok(output) => interpret(
            output.status.success(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        ),
        Err(err) => PreflightResult {
            ok: false,
            detail: format!("could not run `{CREDS_BIN} has-tpm2`: {err}"),
        },
    }
}

/// Map an exit success flag plus captured output to a [`PreflightResult`].
/// `success` keys the verdict; the combined output is the display detail.
fn interpret(success: bool, stdout: &str, stderr: &str) -> PreflightResult {
    let detail = [stdout, stderr]
        .iter()
        .map(|stream| stream.trim())
        .filter(|stream| !stream.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    PreflightResult {
        ok: success,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_is_has_tpm2() {
        assert_eq!(args(), ["has-tpm2"]);
    }

    #[test]
    fn exit_zero_is_ok() {
        let result = interpret(true, "firmware: yes\ndriver: yes\nsystem: yes\n", "");
        assert!(result.ok);
        assert_eq!(result.detail, "firmware: yes\ndriver: yes\nsystem: yes");
    }

    #[test]
    fn non_zero_exit_is_not_ok_with_detail() {
        let result = interpret(false, "", "firmware: no\ndriver: no\nsystem: no\n");
        assert!(!result.ok);
        assert_eq!(result.detail, "firmware: no\ndriver: no\nsystem: no");
    }

    #[test]
    fn merges_and_trims_both_streams() {
        let result = interpret(false, "  +firmware +driver  ", "  partial support  ");
        assert!(!result.ok);
        assert_eq!(result.detail, "+firmware +driver\npartial support");
    }
}
