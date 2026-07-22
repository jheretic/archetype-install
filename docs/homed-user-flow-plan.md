# Plan: replace root-password install flow with a systemd-homed first user

Status: planned 2026-07 (v1 = PASSWORD ONLY). Changes the installer's first-boot
identity model: LOCK root, create an initial **systemd-homed** user (username +
full name/GECOS) in the **wheel** group with **sudo**, authenticated by a
**password**. FIDO2 is explicitly out of v1 — the user can enroll a token later
(`homectl update <user> --fido2-device=auto`), and password-only is the right
default for a headless server with no token present.

## Why password-only is simple (and FIDO2 was not)

A password user is FULLY PRE-BAKEABLE OFFLINE. FIDO2/recovery-key enrollment
required a live homed + physical token at create time (a first-boot interactive
step) — dropping it removes all of that. There is NO homed recovery key in v1
(the user has a password); the existing root-PARTITION recovery key is unchanged.

## Mechanism (verified against systemd 261 in mkosi.tools)

- `systemd-homed-firstboot.service` ships in the systemd package with
  `ConditionFirstBoot=yes`, `ImportCredential=home.*`,
  `ExecStart=homectl firstboot --prompt-new-user ...`, `WantedBy=systemd-homed.service`.
- On first boot it imports credentials named `home.create.<username>` (a JSON
  user record) and creates that homed user NON-interactively. (`--prompt-new-user`
  only prompts if NO such credential exists.)
- Credential store search path includes **`/etc/credstore/`** (systemd.exec(5)).
  So the INSTALLER writes `/etc/credstore/home.create.<username>` (mode 0600) on
  the mounted target; first boot consumes it.
- `systemd-homed.service` is enabled by preset (`90-systemd.preset:
  enable systemd-homed.service`) and the `systemd-homed` binary ships in Arch's
  base `systemd` package (already in the image). Phase 4 verifies the image
  build actually applies the preset (not masked).
- Password: the user record needs BOTH `privileged.hashedPassword` (crypt(3)
  `$6$`, for auth) AND `secret.password` (PLAINTEXT array) — homed requires the
  plaintext at CREATE time to derive the LUKS home encryption key
  (USER_RECORD.md: the `secret` section carries the plaintext password "as
  passwords... need to be provided to encrypt the home directory with").
  hashedPassword alone cannot encrypt the home.
- SECURITY NOTE (decided: plaintext-on-encrypted-root): the staged
  `/etc/credstore/home.create.<user>` therefore contains the PLAINTEXT password
  until first boot consumes it. This is acceptable because the target's
  `/etc/credstore/` lives on the LUKS+TPM-encrypted root partition (protected at
  rest), the credential is consumed once (`ConditionFirstBoot=yes`), and this is
  the standard systemd first-boot-user-creation pattern. Write it 0600. (The
  encrypted-credstore alternative would need the target's TPM sealed at install
  time — more complexity for marginal gain since root is already encrypted.)
  The record's `regular`/`privileged` sections carry userName/realName/
  memberOf=[wheel]/storage=luks + hashedPassword; `secret.password` carries the
  plaintext. No signature section (homed signs locally on create; an unsigned
  create credential is accepted from the trusted local credstore).

## Storage backend: nested LUKS-file (DECIDED)

The installer already builds an integrity-protected `/home` partition
(HOME_PARTLABEL: integritysetup + btrfs, mounted at /home — install.rs
integrity_home). DECISION (user, 2026-07): use homed's DEFAULT `luks` storage —
a per-user LUKS home file under /home — i.e. NESTED encryption on the integrity
btrfs /home. Rationale: defense-in-depth — the home is independently encrypted
with the USER's password, so someone holding the root/integrity key still cannot
read the home without the user password. The nesting is a minor space/perf cost,
not a correctness issue. So the user record uses `storage=luks` (the default; set
it explicitly for clarity). Do NOT use subvolume/directory.

## Current flow being replaced (archetype-install)

- `firstboot.rs::FirstbootConfig` holds `root_password_hash` + `tpm_pin`;
  `firstboot_args()` builds `systemd-firstboot --root --root-password-hashed=`.
- `install.rs`: `apply_firstboot` (offline `--root`), `enroll_recovery_key`
  (root-partition recovery, KEEP), optional TPM2-PIN enroll (KEEP).
- `recovery.rs`: recovery-key parse + QR (unchanged; still for the root disk).
- Screens: `screens/firstboot.rs` (config screen).

## Target flow

1. **Lock root** instead of setting a root password. Offline: set root's shadow
   entry to a locked hash (`!*`). Verify the cleanest offline mechanism
   (`systemd-firstboot --root-password-hashed='!*'`, or writing the shadow entry
   directly). No root login; wheel+sudo is the admin path.
2. **Collect on the config screen:** username (valid UNIX name), full name/GECOS,
   password (+ confirm). Drop the root-password field.
3. **Add `sudo`** to archetype-build Packages= + ship a wheel dropin
   (`%wheel ALL=(ALL:ALL) ALL` under /etc/sudoers.d, or the /usr equivalent).
   User is a member of `wheel`.
4. **Stage the credential:** build the JSON user record (userName, realName,
   memberOf=["wheel"], hashed password, `storage=luks`) and write it to
   `/etc/credstore/home.create.<username>` (0600) on the target. First boot
   creates the user (nested LUKS home on the integrity /home).

## Phasing

Destructive/first-boot paths are VM-validation-only (as with the rest of
archetype-install); unit-test the pure pieces (user-record JSON, username/GECOS
validation, root-lock arg, credential file contents).

- **Phase 1 — data model + firstboot.rs.** Replace `root_password_hash` with
  `UserConfig { username, realname, password_hash }`; add root-lock; add the
  user-record JSON builder + `credstore` path/filename + the wheel/storage
  fields. Username + GECOS validation. Unit tests.
- **Phase 2 — config screen** (`screens/firstboot.rs`): username + GECOS +
  password/confirm fields, validation gating (mirror the existing field-nav +
  conditional-gating pattern already in the screen).
- **Phase 3 — install.rs wiring:** replace the root-password step with root-lock
  + write `/etc/credstore/home.create.<user>`. Keep root-partition recovery +
  TPM-PIN steps unchanged.
- **Phase 4 — image (archetype-build):** add `sudo` to Packages=; ship the wheel
  sudoers dropin; VERIFY systemd-homed.service is enabled in the built image
  (preset applied, not masked) so first boot consumes the credential.
- **Phase 5 — docs + validation notes.** Document the flow; enumerate the
  VM-validation items (first boot creates the user, wheel/sudo works, root is
  locked, login with the password).

## Risks

- **homed not actually enabled in the built image** → the credential is never
  consumed, first boot has no login user, root is locked = lockout. Phase 4 must
  verify homed is enabled in the IMAGE (not just the tools tree). Consider a
  safety net (see below).
- **Lockout failure story (DECIDED — accept the risk):** locked root + a homed
  user that fails to materialize at first boot = no way in. DECISION (user,
  2026-07): ACCEPT this — no bootstrap escape hatch; rely on rigorous Phase-4
  verification that homed is enabled in the BUILT image + the recovery console.
  This matches image-based-distro norms; a fallback getty / staged-lock adds
  real complexity we're not taking on.
- **Storage backend** (above) — pick subvolume/directory over nested LUKS.
- **VM-only validation** for the whole first-boot/homed path.
