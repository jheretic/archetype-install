//! Recovery-key handling: parse the key string out of `systemd-cryptenroll`
//! stdout and render our own QR code from it.
//!
//! The pure logic (parsing, QR matrix construction) lives here so it can be
//! unit-tested without running cryptenroll. The install worker calls
//! [`parse_recovery_key`]; the Recovery screen calls [`qr_matrix`] to draw the
//! code in the afterglow palette rather than reusing cryptenroll's ANSI QR.

use qrcode::{EcLevel, QrCode};

/// systemd recovery keys are 64 modhex chars in 8 dash-separated groups of 8,
/// e.g. `cbdefghi-jklnrtuv-...` (8*8 + 7 dashes = 71 chars). The modhex alphabet
/// is `cbdefghijklnrtuv` (lowercase); see systemd `src/shared/recovery-key.c`.
const MODHEX: &[u8; 16] = b"cbdefghijklnrtuv";
const GROUPS: usize = 8;
const GROUP_LEN: usize = 8;
/// 8 groups of 8 chars joined by 7 dashes.
const FORMATTED_LEN: usize = GROUPS * GROUP_LEN + (GROUPS - 1);

/// Extract the recovery-key string from `systemd-cryptenroll --recovery-key`
/// stdout. cryptenroll writes only the key itself to stdout (the explanatory
/// chrome and the ANSI QR go to stderr), but we parse defensively: scan every
/// whitespace-delimited token across all lines and return the first that is a
/// well-formed recovery key, so we do not depend on exact surrounding text.
pub fn parse_recovery_key(stdout: &str) -> Option<String> {
    stdout
        .split_whitespace()
        .find(|token| is_recovery_key(token))
        .map(str::to_string)
}

/// Whether `s` is a syntactically valid systemd recovery key: 8 groups of 8
/// modhex characters separated by single dashes.
fn is_recovery_key(s: &str) -> bool {
    if s.len() != FORMATTED_LEN {
        return false;
    }
    for (index, group) in s.split('-').enumerate() {
        if index >= GROUPS || group.len() != GROUP_LEN {
            return false;
        }
        if !group.bytes().all(|b| MODHEX.contains(&b)) {
            return false;
        }
    }
    s.split('-').count() == GROUPS
}

/// Build the QR module matrix for `key` as rows of booleans (true == dark
/// module). Uses error-correction level M, matching cryptenroll's own QR. The
/// caller renders the matrix with Unicode half-blocks in the chosen palette.
pub fn qr_matrix(key: &str) -> Option<Vec<Vec<bool>>> {
    let code = QrCode::with_error_correction_level(key.as_bytes(), EcLevel::M).ok()?;
    let width = code.width();
    let modules = code.to_colors();
    let rows = modules
        .chunks(width)
        .map(|row| row.iter().map(|c| *c == qrcode::Color::Dark).collect())
        .collect();
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample valid recovery key (64 modhex chars, 8 groups of 8).
    const SAMPLE: &str = "cbdefghi-jklnrtuv-cbcbcbcb-dededede-fgfgfgfg-hihihihi-jkjkjkjk-lnlnlnln";

    #[test]
    fn sample_is_well_formed() {
        assert_eq!(SAMPLE.len(), FORMATTED_LEN);
        assert!(is_recovery_key(SAMPLE));
    }

    #[test]
    fn parses_key_from_bare_stdout() {
        let stdout = format!("{SAMPLE}\n");
        assert_eq!(parse_recovery_key(&stdout).as_deref(), Some(SAMPLE));
    }

    #[test]
    fn parses_key_amid_surrounding_text() {
        // Defensive: even if cryptenroll ever framed the key with prose on
        // stdout, we still pick the key token out.
        let stdout = format!(
            "A secret recovery key has been generated:\n\n    {SAMPLE}\n\nPlease save it.\n"
        );
        assert_eq!(parse_recovery_key(&stdout).as_deref(), Some(SAMPLE));
    }

    #[test]
    fn rejects_output_without_a_key() {
        assert_eq!(parse_recovery_key("no key here\njust noise\n"), None);
        // Wrong charset (contains 'a', 'o', 's' which are not modhex).
        assert_eq!(
            parse_recovery_key(
                "aaaaaaaa-oooooooo-ssssssss-aaaaaaaa-oooooooo-ssssssss-aaaaaaaa-oooooooo"
            ),
            None
        );
        // Wrong length (7 groups).
        assert_eq!(
            parse_recovery_key("cbdefghi-jklnrtuv-cbcbcbcb-dededede-fgfgfgfg-hihihihi-jkjkjkjk"),
            None
        );
    }

    #[test]
    fn qr_matrix_is_nonempty_and_square() {
        let matrix = qr_matrix(SAMPLE).expect("a recovery key should encode to a QR");
        assert!(!matrix.is_empty());
        let width = matrix.len();
        assert!(
            matrix.iter().all(|row| row.len() == width),
            "QR must be square"
        );
        // A real QR of this payload has dark modules.
        assert!(matrix.iter().any(|row| row.iter().any(|&m| m)));
    }
}
