//! First-boot configuration: the data the wizard collects for a later phase to
//! feed `systemd-firstboot --root=<target>`, write `/etc/machine-info`, and
//! stage the systemd-homed first-user credential.
//!
//! This module owns the [`FirstbootConfig`] accumulator, the [`Chassis`]
//! choice, the [`UserConfig`] first-user record, field validation, and
//! password hashing. A later phase consumes these values; nothing here runs
//! systemd or touches the target.
//!
//! Identity model: root is LOCKED (no login); the admin path is an initial
//! systemd-homed user in the `wheel` group. The user password is hashed with
//! [`hash_password`] into a crypt(3) `$6$` (SHA-512) string for auth, but the
//! plaintext is also held in [`UserConfig`] because homed needs it at create
//! time to derive the LUKS home key (see docs/homed-user-flow-plan.md).

use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::json;
use sha_crypt::{sha512_simple, Sha512Params, ROUNDS_DEFAULT};

/// The locked-account shadow/hash convention: an invalid crypt string that no
/// password can produce, so root has no working login. Passed to
/// `systemd-firstboot --root-password-hashed=` to lock root on the target.
pub const LOCKED_PASSWORD_HASH: &str = "!*";

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

/// The initial systemd-homed user, collected on the Config screen. Holds both
/// the crypt(3) `$6$` password hash (for auth) and the plaintext password
/// (needed by homed at create time to derive the LUKS home key). The plaintext
/// lives only in-memory here; a later phase writes it into the 0600 credstore
/// credential on the LUKS-encrypted target.
#[derive(Clone)]
pub struct UserConfig {
    /// UNIX username; validated by [`valid_username`].
    pub username: String,
    /// Full name / GECOS; may be empty (then `realName` is omitted).
    pub realname: String,
    /// crypt(3) `$6$` SHA-512 hash of the password, for `privileged.hashedPassword`.
    pub password_hash: String,
    /// Plaintext password, for the `secret.password` array homed needs at create.
    pub password_plain: String,
}

// Consumed by the Phase-2 screen and Phase-3 install wiring; unit-tested now.
#[allow(dead_code)]
impl UserConfig {
    /// The compact JSON systemd user record for the `home.create.<username>`
    /// credential. Follows the systemd JSON User Records spec: top-level
    /// identity fields, a `privileged.hashedPassword` array, and a
    /// `secret.password` plaintext array (required so homed can derive the LUKS
    /// home key at create). `storage: "luks"` selects a per-user LUKS home file.
    /// No signature section: homed signs locally on create and accepts an
    /// unsigned create credential from the trusted local credstore.
    pub fn home_create_record(&self) -> String {
        let mut record = json!({
            "userName": self.username,
            "memberOf": ["wheel"],
            "storage": "luks",
            "disposition": "regular",
            // Disable homed's password-quality check at create time: the screen
            // already accepts any non-empty password, and without this homed may
            // REJECT a "weak" one and fall back to an interactive prompt, leaving
            // root locked and no user on a headless install. Matches what
            // systemd's own `homectl firstboot` sets.
            "enforcePasswordPolicy": false,
            "privileged": { "hashedPassword": [self.password_hash] },
            "secret": { "password": [self.password_plain] },
        });
        if !self.realname.is_empty() {
            record["realName"] = json!(self.realname);
        }
        record.to_string()
    }

    /// Relative path, under the target root, of the first-user credential file:
    /// `etc/credstore/home.create.<username>`. A later phase writes
    /// [`home_create_record`](Self::home_create_record) there (mode 0600);
    /// systemd-homed-firstboot consumes it on first boot.
    pub fn credstore_relpath(&self) -> String {
        format!("etc/credstore/home.create.{}", self.username)
    }

    /// Whether this user is fully specified: valid username, a password hash,
    /// and the plaintext present. GECOS may be empty. The screen gates
    /// password confirmation separately.
    pub fn user_complete(&self) -> bool {
        valid_username(&self.username)
            && valid_gecos(&self.realname)
            && !self.password_hash.is_empty()
            && !self.password_plain.is_empty()
    }
}

/// First-boot fields accumulated on the Config screen. The text fields start at
/// sensible defaults; `user` is `None` until the first user is entered,
/// confirmed, and committed.
#[derive(Clone)]
pub struct FirstbootConfig {
    pub keymap: String,
    pub locale: String,
    pub timezone: String,
    pub hostname: String,
    pub chassis: Chassis,
    /// The initial systemd-homed user; `None` until the screen commits it.
    pub user: Option<UserConfig>,
    /// The TPM2 unlock PIN for the encrypted root, if the user chose PIN mode
    /// (the default). `Some(pin)` -> the installer re-enrolls the root TPM2
    /// keyslot with `--tpm2-with-pin=yes` + a signed PCR-11 policy and wipes the
    /// PIN-less slot, so the root only auto-unlocks with the PIN (defends against
    /// booting live media to decrypt the disk). `None` -> automatic mode: the
    /// PIN-less TPM2 slot repart enrolled is kept as-is (current behaviour).
    /// Plaintext, held only until enrollment; never written to disk or logs.
    pub tpm_pin: Option<String>,
}

impl Default for FirstbootConfig {
    fn default() -> Self {
        Self {
            keymap: "us".to_string(),
            locale: "en_US.UTF-8".to_string(),
            timezone: "UTC".to_string(),
            hostname: "archetype".to_string(),
            chassis: Chassis::Desktop,
            user: None,
            tpm_pin: None,
        }
    }
}

impl FirstbootConfig {
    /// Build the `systemd-firstboot --root=<root>` argument vector for an
    /// offline target tree. Empty text fields are omitted (firstboot leaves an
    /// unconfigured setting unprompted under `--root`). root is LOCKED: we
    /// always pass `--root-password-hashed=!*` (the locked-account convention),
    /// so root has no working login -- the admin path is the wheel homed user.
    /// We deliberately do NOT pass `--setup-machine-id`: committing a real
    /// machine-id makes the installed system's first boot NOT a first boot
    /// (machine-id(5) First Boot Semantics), which silently skips every
    /// `ConditionFirstBoot=yes` unit -- including systemd-homed-firstboot, so
    /// the wheel user would never be created and locked root = brick. Instead
    /// the target `/etc/machine-id` is left in the `uninitialized` first-boot
    /// state (see `write_machine_id` in install.rs); PID1 generates the real id
    /// during that first boot. `--force` is
    /// required: the target /etc was just seeded from the factory tree
    /// (/usr/share/factory/etc ships locale.conf, vconsole.conf, passwd, shadow,
    /// ...), and without --force firstboot silently skips any file that already
    /// exists -- which would drop the locked-root shadow entry while still
    /// reporting success.
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
        args.push(format!("--root-password-hashed={LOCKED_PASSWORD_HASH}"));
        args
    }

    /// The `/etc/machine-info` contents carrying `CHASSIS=`. Newline-terminated.
    /// `CHASSIS=` is a machine-info(5) field, not a firstboot flag, so it is
    /// written as a discrete step.
    pub fn machine_info(&self) -> String {
        format!("CHASSIS={}\n", self.chassis.as_str())
    }

    /// Whether every required text field is filled and the hostname is valid.
    /// Independent of the user, which the screen gates separately.
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

/// Hash `plaintext` into a crypt(3) `$6$` SHA-512 string (for a user record's
/// `privileged.hashedPassword`). A random salt is generated per call.
pub fn hash_password(plaintext: &str) -> Result<String> {
    let params = Sha512Params::new(ROUNDS_DEFAULT)
        .map_err(|err| anyhow!("invalid sha512 params: {err:?}"))?;
    sha512_simple(plaintext, &params).map_err(|err| anyhow!("failed to hash password: {err:?}"))
}

/// A valid UNIX username following the useradd convention: first char a
/// lowercase letter or `_`, then lowercase letters, digits, `_`, or `-`;
/// length 1..=32. Rejects uppercase, an all-numeric name, and a trailing `-`.
pub fn valid_username(username: &str) -> bool {
    // Max 31: systemd's strict validator caps at sizeof(utmpx.ut_user)-1 = 31
    // (src/basic/user-util.c). A 32-char name would pass a naive check here but
    // be SKIPPED by homed at first boot -> locked root, no user. So reject it.
    let len = username.len();
    if !(1..=31).contains(&len) {
        return false;
    }
    if username.ends_with('-') {
        return false;
    }
    // Reject names that already exist in the seeded target (homed refuses to
    // create a user/group that collides -> the credential is silently skipped
    // -> lockout). These are the accounts/groups the base image ships.
    if RESERVED_NAMES.contains(&username) {
        return false;
    }
    let mut chars = username.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Names that already exist in the base image's passwd/group and would make
/// systemd-homed refuse to create the user (a name collision skips the
/// credential at first boot). Not exhaustive of every possible NSS entry, but
/// covers root + the standard system accounts/groups an Arch base ships, so the
/// common footguns (root, wheel, nobody, ...) are rejected in the installer
/// rather than silently bricking at first boot.
const RESERVED_NAMES: &[&str] = &[
    "root",
    "wheel",
    "nobody",
    "nobody4",
    "daemon",
    "bin",
    "sys",
    "adm",
    "tty",
    "disk",
    "lp",
    "mem",
    "kmem",
    "mail",
    "news",
    "uucp",
    "man",
    "proxy",
    "kvm",
    "games",
    "ftp",
    "http",
    "dbus",
    "systemd-journal",
    "systemd-network",
    "systemd-resolve",
    "systemd-timesync",
    "systemd-coredump",
    "systemd-oom",
    "systemd-homed",
    "polkitd",
    "audio",
    "video",
    "render",
    "input",
    "users",
    "utmp",
    "storage",
    "optical",
    "network",
    "power",
    "sudo",
];

/// A valid GECOS / full-name field: no control chars, no `:` (the passwd/shadow
/// delimiter), no newline. Empty is allowed (the record omits `realName`).
pub fn valid_gecos(gecos: &str) -> bool {
    gecos
        .chars()
        .all(|c| !c.is_control() && c != ':' && c != '\n')
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha_crypt::sha512_check;

    #[test]
    fn hash_is_sha512_crypt_and_round_trips() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(
            hash.starts_with("$6$"),
            "hash must be a $6$ SHA-512 crypt string: {hash}"
        );
        assert!(sha512_check("correct horse battery staple", &hash).is_ok());
        assert!(sha512_check("wrong password", &hash).is_err());
    }

    #[test]
    fn distinct_salts_yield_distinct_hashes() {
        let a = hash_password("same").unwrap();
        let b = hash_password("same").unwrap();
        assert_ne!(a, b, "a random salt per call should make hashes differ");
    }

    fn sample_user() -> UserConfig {
        UserConfig {
            username: "alice".to_string(),
            realname: "Alice Example".to_string(),
            password_hash: "$6$salt$hash".to_string(),
            password_plain: "hunter2hunter2".to_string(),
        }
    }

    #[test]
    fn home_create_record_matches_user_record_schema() {
        let json: serde_json::Value =
            serde_json::from_str(&sample_user().home_create_record()).unwrap();
        assert_eq!(json["userName"], "alice");
        assert_eq!(json["realName"], "Alice Example");
        assert_eq!(json["disposition"], "regular");
        assert_eq!(json["storage"], "luks");
        assert_eq!(json["memberOf"], serde_json::json!(["wheel"]));
        assert_eq!(json["privileged"]["hashedPassword"][0], "$6$salt$hash");
        assert_eq!(json["secret"]["password"][0], "hunter2hunter2");
    }

    #[test]
    fn home_create_record_omits_empty_realname() {
        let mut user = sample_user();
        user.realname.clear();
        let json: serde_json::Value = serde_json::from_str(&user.home_create_record()).unwrap();
        assert!(json.get("realName").is_none());
    }

    #[test]
    fn credstore_relpath_names_the_credential() {
        assert_eq!(
            sample_user().credstore_relpath(),
            "etc/credstore/home.create.alice"
        );
    }

    #[test]
    fn user_complete_requires_valid_name_and_secrets() {
        let mut user = sample_user();
        assert!(user.user_complete());
        user.password_plain.clear();
        assert!(!user.user_complete());
        let mut user = sample_user();
        user.username = "Bad".to_string();
        assert!(!user.user_complete());
    }

    #[test]
    fn valid_username_accepts_useradd_names() {
        assert!(valid_username("alice"));
        assert!(valid_username("_svc"));
        assert!(valid_username("a"));
        assert!(valid_username("user-01_x"));
        assert!(valid_username(&"a".repeat(31))); // 31 = utmpx max, accepted
    }

    #[test]
    fn valid_username_rejects_over_31_and_reserved() {
        assert!(!valid_username(&"a".repeat(32))); // homed skips >31 -> lockout
        assert!(!valid_username("root")); // collides with seeded accounts
        assert!(!valid_username("wheel"));
        assert!(!valid_username("nobody"));
        assert!(!valid_username("sudo"));
    }

    #[test]
    fn valid_username_rejects_bad_names() {
        assert!(!valid_username(""));
        assert!(!valid_username("Alice"));
        assert!(!valid_username("1abc"));
        assert!(!valid_username("123"));
        assert!(!valid_username("-leading"));
        assert!(!valid_username("trailing-"));
        assert!(!valid_username("has space"));
        assert!(!valid_username("dotted.name"));
        assert!(!valid_username(&"a".repeat(33)));
    }

    #[test]
    fn valid_gecos_accepts_names_and_empty_rejects_delimiters() {
        assert!(valid_gecos("Alice Example"));
        assert!(valid_gecos(""));
        assert!(!valid_gecos("has:colon"));
        assert!(!valid_gecos("has\nnewline"));
        assert!(!valid_gecos("tab\there"));
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
            user: Some(sample_user()),
            tpm_pin: None,
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
                "--root-password-hashed=!*",
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
            user: None,
            tpm_pin: None,
        };
        let args = config.firstboot_args(Path::new("/mnt"));
        assert_eq!(
            args,
            [
                "--root=/mnt",
                "--force",
                "--timezone=UTC",
                "--root-password-hashed=!*",
            ]
        );
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
    fn firstboot_args_always_lock_root() {
        let args = FirstbootConfig::default().firstboot_args(Path::new("/mnt"));
        assert!(args.contains(&"--root-password-hashed=!*".to_string()));
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
