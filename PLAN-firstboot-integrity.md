# Plan: preflight, firstboot config, optional home, dm-integrity, recovery key

Six interlocking changes spanning the installer wizard, the install execute
path, and the archetype-build image. Verified against systemd man pages
(firstboot, machine-info, integritytab, cryptenroll, repart.d) on 2026-06-29.

## 0. Verified facts (don't re-guess)

- `systemd-firstboot.service` runs iff `ConditionFirstBoot=yes`, i.e. `/etc` is
  unpopulated (machine-id unset). The live image's `/etc` is tmpfs with no
  machine-id, so it fires on every live boot today.
- "If a setting is already initialized, firstboot will not overwrite it and will
  not prompt." => seeding the TARGET's /etc makes the installed system's
  firstboot a no-op for what we set.
- firstboot writes to a mounted image via `--root=`. It sets: locale, keymap,
  timezone, hostname, root password (`--root-password-hashed=` preferred),
  machine-id. It does NOT set chassis.
- Desktop/laptop/server is `CHASSIS=` in `/etc/machine-info` (machine-info(5),
  since v197), NOT firstboot `--machine-tags=` (v261, different concept). The
  installer writes /etc/machine-info on the target directly.
- TPM preflight: `systemd-creds has-tpm2` — exit 0 = usable TPM2; prints
  firmware/driver/libraries lines. Authoritative systemd check.
- integritytab: `name  device  keyfile|-  options|-`. A keyfile present =>
  integrity-algorithm defaults to hmac-sha256; `integritysetup format` MUST use
  the matching algorithm + key. Max keyfile 4096 bytes.
- cryptenroll `--recovery-key` generates a high-entropy recovery key AND prints
  a QR code to the terminal. To enroll it must first unlock the volume with an
  existing key — the TPM2 keyslot repart created serves as that unlock path.

## 1. TPM preflight (first thing, before any screen acts)

- New `preflight` module: run `systemd-creds has-tpm2`; exit 0 => ok.
- New `Screen::Preflight` shown FIRST (before/instead of Welcome advancing).
  On failure: a clear afterglow error screen — "Archetype requires a TPM2.
  None was detected. <reason from has-tpm2 output>. The installer cannot
  continue." Only action: quit.
- In `--dry-run`, still run the check but treat absence as a loud warning, not a
  hard stop (so dry-runs work on dev boxes without a TPM). Decide: warn-and-
  continue in dry-run; hard-stop otherwise.

## 2. firstboot config screens (replace the live firstboot)

- archetype-build: MASK `systemd-firstboot.service` on the live image so it
  never runs on the live medium (the installer collects this instead). Ship a
  mask symlink in mkosi.extra/usr/lib/systemd/system/ OR a drop-in. Decide
  mechanism in the build-repo task.
- New screens collecting: keymap, locale, timezone, hostname, root password
  (typed twice, masked), chassis (desktop/laptop/server). Store in InstallConfig.
  - keymap/locale/timezone: free-text entry first cut (validated lightly);
    a picker is a follow-up. Provide sensible defaults (us / en_US.UTF-8 / UTC).
  - root password: confirm-match gate; hash with a known crypt method. NOTE:
    need a hashing approach (openssl passwd -6, or mkpasswd) available in-image
    — VERIFY which is present; firstboot wants `--root-password-hashed=`.
- Execute path: after /usr+/etc are laid down on the target, run
  `systemd-firstboot --root=<target> --locale= --keymap= --timezone=
  --hostname= --root-password-hashed= --setup-machine-id` and write
  /etc/machine-info with CHASSIS=<choice> on the target.

## 3. Optional home partition

- Sizing screen: allow home to be omitted entirely (toggle / set-to-zero).
- layout.rs: `home: Option<SizeChoice>` (currently non-optional). When None,
  generate.rs omits 90-home.conf — mirror the existing swap-omission path.
- Tests: home-None omits the file (parallel to omits_swap_file_when_swap_is_none).

## 4. dm-integrity home (only when home present)

repart side:
- 90-home.conf: `Format=empty` (create the partition empty; we format it
  ourselves with integritysetup). Keep a stable PARTLABEL=HOME for integritytab.
Execute side (after target root mounted, home partition exists):
1. generate a random keyfile, write to <target>/etc/integritysetup-keys.d/home.key
   (mode 0600, root). (Key size: pick per hmac-sha256, <=4096 bytes.)
2. `integritysetup format --integrity hmac-sha256 --integrity-key-file <key>
   --integrity-key-size <n> <home-part>` (VERIFY exact flag names).
3. open it: `integritysetup open --integrity hmac-sha256 --integrity-key-file
   ... <home-part> home` => /dev/mapper/home
4. `mkfs.btrfs /dev/mapper/home`
5. append to <target>/etc/integritytab:
   `home  PARTLABEL=HOME  /etc/integritysetup-keys.d/home.key  allow-discards,mode=bitmap`
6. (fstab/mount of /home: does the image mount /home from /dev/mapper/home? The
   integritysetup-generator sets up the mapper device from integritytab at boot;
   a corresponding /home mount entry is still needed. VERIFY how the image
   mounts /home — gpt-auto won't find btrfs-on-mapper by PARTLABEL. Likely need
   an fstab entry on the target too. OPEN QUESTION — resolve before building.)

## 5. Root recovery key (cryptenroll)

- After the root LUKS2 volume exists (repart enrolled TPM2), run
  `systemd-cryptenroll --recovery-key <root-part>`; it unlocks via the TPM2 and
  prints the recovery key + QR to the terminal.
- The installer must capture that output and DISPLAY it on a dedicated screen
  (recovery key text large + the QR). Since the install runs on a worker thread
  feeding the Progress channel, the recovery key/QR must be surfaced through a
  new Progress variant and held on a Result-like screen until the user
  acknowledges ("I have saved this — press Enter"). Block reboot until ack.
  - QR rendering in ratatui: render to unicode half-blocks. cryptenroll already
    prints a QR to TTY; capturing its ANSI and replaying may be simplest, OR
    re-generate from the key string with a qr crate. Decide in the task.

## 6. Ordering in the execute path (revised post_steps)

1. repart write (TPM2-enrolled root, empty home, etc.) — existing
2. mount target root (TODO: TPM2 unlock still unwired — see existing Incomplete)
3. mount cloned /usr
4. seed /etc (systemd-tmpfiles) — existing
5. firstboot --root + write machine-info (NEW §2)
6. dm-integrity home setup + integritytab (NEW §4, only if home present)
7. recovery-key enroll + display/ack (NEW §5)
8. bootloader note — existing

## Resolved decisions (user, 2026-06-29)

- Q1 /home mount: write an **fstab entry on the target** for /dev/mapper/home ->
  /home (btrfs). integritytab sets up the mapper at boot; fstab mounts it.
- Q2 wire the TPM2 unlock: YES, in scope for P-E. This finishes the previously
  TODO'd unlock path; firstboot/machine-info/keyfile/integritytab all need the
  target root mounted.
- Q3 NO password-hashing tool in the image. => hash IN the installer binary:
  emit a $6$ SHA-512 crypt hash via a Rust crypt crate (e.g. sha-crypt), pass to
  firstboot --root-password-hashed=. Keeps the single-binary, no new image dep.
- Q4 integritysetup exact flags: STILL VERIFY at P-E time (integritysetup(8) /
  --integrity, --integrity-key-file, --integrity-key-size; open vs format).
- Q5 dry-run: CONFIRMED — §2/§4/§5 are no-ops under --dry-run (log intended
  commands only, touch nothing outside /run).
- CHASSIS confirmed (not firstboot tags). firstboot live-mask confirmed.
- Recovery key + QR display screen confirmed (blocks reboot until ack).

## Phasing

- P-A (build repo): mask firstboot on live; 90-home Format=empty + PARTLABEL.
- P-B (installer): TPM preflight screen + module.
- P-C (installer): optional-home in layout/generate/sizing (+ tests).
- P-D (installer): firstboot config screens + InstallConfig fields.
- P-E (installer): execute-path wiring — firstboot --root, machine-info,
  dm-integrity home, integritytab, recovery-key enroll + display/ack screen.
  (Largest; depends on resolving Q1–Q5. Destructive — reviewer before commit.)

P-A/P-B/P-C are independent and safe; P-D feeds P-E. P-E is the hard one.
