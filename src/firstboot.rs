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

use std::path::Path;

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
#[derive(Clone)]
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
    /// Build the `systemd-firstboot --root=<root>` argument vector for an
    /// offline target tree. Empty text fields and a `None` password hash are
    /// omitted (firstboot leaves an unconfigured setting unprompted under
    /// `--root`). `--setup-machine-id` is always passed so the installed system
    /// gets a fresh machine-id rather than inheriting the installer's. `--force`
    /// is required: the target /etc was just seeded from the factory tree
    /// (/usr/share/factory/etc ships locale.conf, vconsole.conf, passwd, shadow,
    /// ...), and without --force firstboot silently skips any file that already
    /// exists -- which would drop the wizard's root password and lock the
    /// operator out while still reporting success.
    pub fn firstboot_args(&self, root: &Path) -> Vec<String> {
        let mut args = vec![format!("--root={}", root.display()), "--force".to_string()];
        let mut push = |flag: &str, value: &str| {
            if !value.trim().is_empty() {
                args.push(format!("{flag}={value}"));
            }
        };
        push("--locale", &self.locale);
        push("--locale-messages", &self.locale);
        push("--keymap", &self.keymap);
        push("--timezone", &self.timezone);
        push("--hostname", &self.hostname);
        if let Some(hash) = self.root_password_hash.as_deref() {
            if !hash.is_empty() {
                args.push(format!("--root-password-hashed={hash}"));
            }
        }
        args.push("--setup-machine-id".to_string());
        args
    }

    /// The `/etc/machine-info` contents carrying `CHASSIS=`. Newline-terminated.
    /// `CHASSIS=` is a machine-info(5) field, not a firstboot flag, so it is
    /// written as a discrete step.
    pub fn machine_info(&self) -> String {
        format!("CHASSIS={}\n", self.chassis.as_str())
    }

    /// The stdin line for `chpasswd -e` that sets root's password on the LIVE
    /// system to the same hash applied to the target. `None` until a password
    /// has been entered. Fed via stdin (not argv) so the hash never appears in
    /// the process list. Newline-terminated.
    pub fn live_root_chpasswd_entry(&self) -> Option<String> {
        self.root_password_hash
            .as_deref()
            .filter(|hash| !hash.is_empty())
            .map(|hash| format!("root:{hash}\n"))
    }

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
    fn firstboot_args_render_all_set_fields() {
        let config = FirstbootConfig {
            keymap: "us".to_string(),
            locale: "en_US.UTF-8".to_string(),
            timezone: "UTC".to_string(),
            hostname: "archetype".to_string(),
            chassis: Chassis::Desktop,
            root_password_hash: Some("$6$salt$hash".to_string()),
        };
        let args = config.firstboot_args(Path::new("/run/archetype-install/target"));
        assert_eq!(
            args,
            [
                "--root=/run/archetype-install/target",
                "--force",
                "--locale=en_US.UTF-8",
                "--locale-messages=en_US.UTF-8",
                "--keymap=us",
                "--timezone=UTC",
                "--hostname=archetype",
                "--root-password-hashed=$6$salt$hash",
                "--setup-machine-id",
            ]
        );
    }

    #[test]
    fn firstboot_args_omit_empty_and_none_fields() {
        let config = FirstbootConfig {
            keymap: "  ".to_string(),
            locale: String::new(),
            timezone: "UTC".to_string(),
            hostname: String::new(),
            chassis: Chassis::Server,
            root_password_hash: None,
        };
        let args = config.firstboot_args(Path::new("/mnt"));
        assert_eq!(
            args,
            [
                "--root=/mnt",
                "--force",
                "--timezone=UTC",
                "--setup-machine-id"
            ]
        );
        assert!(!args.iter().any(|a| a.starts_with("--root-password-hashed")));
        assert!(!args.iter().any(|a| a.starts_with("--keymap")));
        assert!(!args.iter().any(|a| a.starts_with("--locale")));
        assert!(!args.iter().any(|a| a.starts_with("--hostname")));
    }

    #[test]
    fn machine_info_carries_chassis_newline_terminated() {
        let config = FirstbootConfig {
            chassis: Chassis::Laptop,
            ..FirstbootConfig::default()
        };
        assert_eq!(config.machine_info(), "CHASSIS=laptop\n");
    }

    #[test]
    fn live_chpasswd_entry_present_only_with_a_hash() {
        let mut config = FirstbootConfig::default();
        assert_eq!(config.live_root_chpasswd_entry(), None);
        config.root_password_hash = Some("$6$salt$hash".to_string());
        assert_eq!(
            config.live_root_chpasswd_entry().as_deref(),
            Some("root:$6$salt$hash\n")
        );
        config.root_password_hash = Some(String::new());
        assert_eq!(config.live_root_chpasswd_entry(), None);
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
