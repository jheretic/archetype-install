//! First-boot configuration: the data the wizard collects for a later phase to
//! feed `systemd-firstboot --root=<target>` and to write `/etc/machine-info`.
//!
//! This module owns the [`FirstbootConfig`] accumulator, the [`Chassis`]
//! choice, field validation, and password hashing. A later phase consumes
//! these values; nothing here runs systemd or touches the target.
//!
//! The root password is never stored as plaintext: the screen hashes it with
//! [`hash_root_password`] into a crypt(3) `$6$` (SHA-512) string, which is what
//! `systemd-firstboot --root-password-hashed=` expects. No hashing tool is
//! installed in the image, so we hash in-process.

use anyhow::{anyhow, Result};
use sha_crypt::{sha512_simple, Sha512Params, ROUNDS_DEFAULT};

/// The machine chassis, mapped to `CHASSIS=` in `/etc/machine-info`. Exactly
/// these three are offered; the stored value is the lowercase string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Chassis {
    Desktop,
    Laptop,
    Server,
}

impl Chassis {
    /// The three choices, in cycle order.
    pub const ALL: [Chassis; 3] = [Chassis::Desktop, Chassis::Laptop, Chassis::Server];

    /// The `CHASSIS=` value, e.g. `desktop`.
    pub fn as_str(self) -> &'static str {
        match self {
            Chassis::Desktop => "desktop",
            Chassis::Laptop => "laptop",
            Chassis::Server => "server",
        }
    }

    /// Cycle to the next choice, wrapping; stays within [`Chassis::ALL`].
    pub fn next(self) -> Chassis {
        let index = Self::ALL.iter().position(|&c| c == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// Cycle to the previous choice, wrapping; stays within [`Chassis::ALL`].
    pub fn prev(self) -> Chassis {
        let index = Self::ALL.iter().position(|&c| c == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// First-boot fields accumulated on the Config screen. The text fields start at
/// sensible defaults; `root_password_hash` is `None` until the password is
/// entered, confirmed, and hashed.
pub struct FirstbootConfig {
    pub keymap: String,
    pub locale: String,
    pub timezone: String,
    pub hostname: String,
    pub chassis: Chassis,
    /// crypt(3) `$6$` hash of the root password, never the plaintext.
    pub root_password_hash: Option<String>,
}

impl Default for FirstbootConfig {
    fn default() -> Self {
        Self {
            keymap: "us".to_string(),
            locale: "en_US.UTF-8".to_string(),
            timezone: "UTC".to_string(),
            hostname: "archetype".to_string(),
            chassis: Chassis::Desktop,
            root_password_hash: None,
        }
    }
}

impl FirstbootConfig {
    /// Whether every required text field is filled and the hostname is valid.
    /// Independent of the password, which the screen gates separately.
    pub fn fields_complete(&self) -> bool {
        !self.keymap.trim().is_empty()
            && !self.locale.trim().is_empty()
            && !self.timezone.trim().is_empty()
            && valid_hostname(&self.hostname)
    }
}

/// A DNS-ish hostname check: 1..=63 chars of ASCII alphanumerics or hyphens,
/// not starting or ending with a hyphen. Deliberately light; a full picker is a
/// follow-up.
pub fn valid_hostname(hostname: &str) -> bool {
    let len = hostname.len();
    if !(1..=63).contains(&len) {
        return false;
    }
    if hostname.starts_with('-') || hostname.ends_with('-') {
        return false;
    }
    hostname
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Hash `plaintext` into a crypt(3) `$6$` SHA-512 string for
/// `systemd-firstboot --root-password-hashed=`. A random salt is generated per
/// call.
pub fn hash_root_password(plaintext: &str) -> Result<String> {
    let params = Sha512Params::new(ROUNDS_DEFAULT)
        .map_err(|err| anyhow!("invalid sha512 params: {err:?}"))?;
    sha512_simple(plaintext, &params).map_err(|err| anyhow!("failed to hash password: {err:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha_crypt::sha512_check;

    #[test]
    fn hash_is_sha512_crypt_and_round_trips() {
        let hash = hash_root_password("correct horse battery staple").unwrap();
        assert!(
            hash.starts_with("$6$"),
            "hash must be a $6$ SHA-512 crypt string: {hash}"
        );
        assert!(sha512_check("correct horse battery staple", &hash).is_ok());
        assert!(sha512_check("wrong password", &hash).is_err());
    }

    #[test]
    fn distinct_salts_yield_distinct_hashes() {
        let a = hash_root_password("same").unwrap();
        let b = hash_root_password("same").unwrap();
        assert_ne!(a, b, "a random salt per call should make hashes differ");
    }

    #[test]
    fn valid_hostname_accepts_dns_ish_names() {
        assert!(valid_hostname("archetype"));
        assert!(valid_hostname("host-01"));
        assert!(valid_hostname("a"));
    }

    #[test]
    fn valid_hostname_rejects_bad_names() {
        assert!(!valid_hostname(""));
        assert!(!valid_hostname("-leading"));
        assert!(!valid_hostname("trailing-"));
        assert!(!valid_hostname("has space"));
        assert!(!valid_hostname("under_score"));
        assert!(!valid_hostname("dotted.name"));
        assert!(!valid_hostname(&"x".repeat(64)));
    }

    #[test]
    fn fields_complete_requires_all_text_fields() {
        let mut config = FirstbootConfig::default();
        assert!(config.fields_complete());
        config.keymap.clear();
        assert!(!config.fields_complete());
    }

    #[test]
    fn chassis_cycles_within_the_three_choices() {
        let mut chassis = Chassis::Desktop;
        let mut seen = Vec::new();
        for _ in 0..6 {
            seen.push(chassis.as_str());
            assert!(Chassis::ALL.contains(&chassis));
            chassis = chassis.next();
        }
        assert_eq!(chassis, Chassis::Desktop, "next must wrap");
        assert_eq!(Chassis::Desktop.prev(), Chassis::Server, "prev must wrap");
        assert!(seen
            .iter()
            .all(|s| ["desktop", "laptop", "server"].contains(s)));
    }
}
