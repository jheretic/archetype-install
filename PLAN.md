# archetype-install — Implementation Plan

A single Rust + ratatui TUI binary that gathers install parameters, generates
`systemd-repart` partition-definition files under `/run`, and invokes
`systemd-repart` to install Archetype Linux onto a chosen disk. Replaces the
abandoned `systemd-sysinstall` integration.

Repo: `/home/n0n/src/archetype/archetype-install` (currently empty bar
`LICENSE` (AGPL-3.0), `README.md`, `.gitignore` tuned for a Cargo binary).

---

## 0. Verification log (grill me on these)

| Fact | How verified |
|---|---|
| Canonical repart set lives at `archetype-build/mkosi.extra/usr/lib/repart.sysinstall.d/` (00-efi … 90-home), 10 files. | Read all 10 verbatim. |
| The sysinstall `usr`/`usr-b` files use `Format=squashfs` + `Verity=data` but carry **no `CopyBlocks=` and no `CopyFiles=`**. As written they format **empty** squashfs partitions — no OS data lands on the target. | Read 10-usr.conf / 40-usr-b.conf. |
| Build-time repart (`archetype-build/mkosi.repart/10-usr.conf`) DOES populate `/usr`: `Format=squashfs` + `CopyFiles=/usr:/` + `Minimize=best`. That is image *creation*, run by mkosi, not the installer. | Read mkosi.repart/*.conf. |
| `CopyBlocks=` (repart.d, v246+) block-replicates a source; `CopyBlocks=auto` auto-selects the running system's matching partition by `Type=`, and "is capable of automatically tracking down the backing partitions for encrypted and Verity-enabled volumes… useful for implementing self-replicating systems." | `repart.d.5` lines 363-401. |
| `CopyBlocks=` **cannot** be combined with `Format=` or `CopyFiles=`. | `repart.d.5` lines 396-399, 425-428. |
| For a `Verity=hash` partition, repart **recomputes** hashes from the matching `Verity=data` partition (matched by `VerityMatchKey=`); `Verity=signature` writes a JSON signature of the verity root hash, which requires a signing key. | `repart.d.5` lines 724-758. |
| `systemd-repart` execute invocation: `--dry-run=no --definitions=DIR --empty=… --key-file=… --private-key=… --certificate=… DEVICE`; `--dry-run=yes` is the implied default (safe). Also has `--copy-source=`, `--root=`, `--seed=`, `--factory-reset=`, `--json=`. | `systemd-repart.8` options list + lines 246-261, 412-476. |
| `system-install.target` + `systemd-sysinstall` are systemd **v261** features. The build's tools-tree systemd is 260.2, but the image installs systemd from the pinned Arch repo (`mkosi.version` 2026.06.17-2), which is ≥261. | mkosi.tools pacman db = 260.2; kernel_addon.sh comment cites "systemd v261"; image uses Arch repo systemd. |
| Live image boots `root=tmpfs systemd.unit=system-install.target` via a signed UKI **addon** (`installer.addon.efi`), placed on the ESP at `/loader/addons/`. | `scripts/kernel_addon.sh`; `mkosi.repart/00-efi.conf` CopyFiles. |
| `70-root.conf` documents the required post-step: after laying down `/usr`, run `systemd-tmpfiles --root=<target> --boot --create` to seed persistent `/etc` from `/usr/share/factory/etc`. `/etc` is **not** a CopyFiles target. | `70-root.conf` comment; `mkosi.postinst` write_tmpfiles docstring. |
| afterglow palette (16 hex slots) is defined identically in `archetype-logo.sh` and `vt-palette.sh`: bg `1c1430`, fg `f4e3ea`, accents red `ff6a5e`, yellow `ffaa52`, green `9ed17b`, blue `7d8fd6`, etc. The chevron ribbon is 4 Powerline `U+E0B0` bands (warm→cool). | Read both scripts. |
| **Guess (unverified):** `CopyBlocks=auto` on the `usr-verity-sig` partition can clone the running system's signature partition, avoiding the need to ship the verity signing private key into the live installer. Not confirmed that `auto` resolves sig partitions. | Inferred from "tracking down backing partitions for Verity-enabled volumes"; sig handling not explicitly stated. Phase 1 spike must confirm. |
| **Guess (unverified):** the installed ESP must exclude `installer.addon.efi`, else the installed system re-enters the installer. | Logical from the addon cmdline; not yet tested. |

---

## 1. Goal (restated)

Build a single, self-contained Rust/ratatui binary that interactively installs
Archetype Linux by generating `repart.d` files into `/run/archetype-install/`
and driving `systemd-repart` to clone the running A/B immutable-`/usr` layout
onto a user-chosen disk, with a dry-run review path and afterglow-themed UI.

---

## 2. Context found

- `archetype-build/mkosi.extra/usr/lib/repart.sysinstall.d/*.conf` — the fixed
  partition template set the tool starts from; **must be patched** to clone
  data (see Phase 1 / risk).
- `archetype-build/mkosi.repart/*.conf` — build-time analogue; shows the
  `CopyFiles`/`Format`/`Minimize` idiom used to *create* the image.
- `archetype-build/scripts/kernel_addon.sh` — proves the live boot isolates
  `system-install.target` with `root=tmpfs`; the installer runs as a unit there.
- `archetype-build/mkosi.postinst` (`write_tmpfiles`, `use_kmscon_on_vts`) —
  documents the `/etc`-from-factory tmpfiles seeding the installer must trigger,
  and that the console is kmscon with a Nerd/Powerline font.
- `archetype-logo.sh`, `vt-palette.sh` — the afterglow palette + chevron-ribbon
  branding to mirror in a Rust theme module.
- No `system-install.target` unit and **no installer service unit** currently
  ship in `archetype-build` (only the repart.d data + the addon). The service
  wiring is greenfield — Phase 7 recommends where it lives.

---

## 3. Crate / module layout

Single binary crate. Modules kept small and single-purpose; no premature trait
abstraction (there is exactly one disk-enumeration strategy, one repart runner).

```
archetype-install/
  Cargo.toml
  src/
    main.rs          # arg parse (--dry-run), TTY guard, App::new().run(), exit/reboot handoff
    app.rs           # App state machine: Screen enum, wizard transitions, shared InstallConfig
    theme.rs         # afterglow palette -> ratatui Color consts; chevron-ribbon header widget
    event.rs         # crossterm event loop + tick; input -> AppEvent
    tui.rs           # terminal init/restore (raw mode, alt screen), panic hook restore
    disk/
      mod.rs         # Disk struct {name, path, size_bytes, model, removable, is_live}
      enumerate.rs   # lsblk --json parse -> Vec<Disk>, filter live/loop/zram
    repart/
      mod.rs
      model.rs       # PartitionDef + RepartSet: fixed templates + configurable root/swap/home
      generate.rs    # RepartSet -> ordered NN-name.conf text, written under /run
      runner.rs      # build + spawn systemd-repart argv; dry-run vs execute; capture output
    layout.rs        # free-space math: disk_size - fixed_total -> allocatable; validation
    screens/
      welcome.rs     # splash: chevron ribbon + version
      disk_select.rs # list widget of Disk; size/model columns
      sizing.rs      # root/swap/home allocation inputs + live remaining gauge
      review.rs      # render generated .conf files (dry-run preview)
      confirm.rs     # destructive-action confirmation (type disk name / hold)
      progress.rs    # stream repart progress + post-steps
      result.rs      # success/failure summary; reboot or exit
  README.md, LICENSE, .gitignore (exist)
```

### Dependencies (minimal, justified)

| Crate | Why | Alternative rejected |
|---|---|---|
| `ratatui` | required TUI framework | — |
| `crossterm` | ratatui's default backend; raw mode, alt screen, truecolor, key events on a bare VT | `termion` (weaker cross-term support; crossterm is the ratatui-canonical backend) |
| `anyhow` | ergonomic error propagation in a binary (not a lib) — context chains for repart failures | `thiserror` alone (overkill: no typed error API exposed to callers) |
| `serde` + `serde_json` | parse `lsblk --json` output | hand-rolled parse (fragile); `libudev`/`sysinfo` (see Disk section) |

Deliberately **not** adding: `nix`/`rustix` (no direct syscalls needed —
privileged work is delegated to `systemd-repart`). Add `libc` only if a real
`geteuid()` root guard is wanted; otherwise skip and let repart surface the
permission failure.

Pin an MSRV + `rust-toolchain.toml` matching the build sandbox toolchain
(coordinate — see Phase 7).

---

## 4. Wizard flow & app-state design

State machine, one `Screen` enum, a single shared `InstallConfig` accumulator.
Classic ratatui loop: `terminal.draw(|f| ui(f, &app))` then block on
`event::read()` (with a tick for the progress screen). No async runtime — the
only long op (`systemd-repart`) runs on a worker thread feeding a channel so the
progress screen stays responsive.

```
Welcome ─▶ DiskSelect ─▶ Sizing ─▶ Review ─┬─(dry-run flag)─▶ Result(printed)
                            ▲               │
                            └───(back)──────┤
                                            └─▶ Confirm ─▶ Progress ─▶ Result
```

- `App { screen, config: InstallConfig, disks: Vec<Disk>, error: Option<String> }`
- `InstallConfig { target: Option<Disk>, root: SizeChoice, swap: SizeChoice, home: SizeChoice }`
- `SizeChoice { Fixed(u64) | Grow(weight) }` (see §6).
- Navigation: each screen returns a `Transition { Next | Back | Quit | Stay }`.
- Dry-run is a global flag (CLI `--dry-run` OR a Review toggle); when set,
  Review → Result without ever reaching Confirm/Progress.
- Progress screen consumes a `mpsc::Receiver<RepartProgress>` from the runner
  thread; Result reports `Ok`/`Err` and offers reboot (`systemctl reboot`) or
  exit (so the `system-install.target` service hand-off completes).

Rejected: an async (`tokio`) event loop — unjustified for one subprocess and a
console UI; adds runtime weight and complicates the TTY/signal story.

---

## 5. Disk enumeration

**Approach: shell out to `lsblk --json -b -o NAME,PATH,SIZE,MODEL,TYPE,RM,RO,MOUNTPOINTS`
and parse with serde_json.**

Justification:
- `lsblk` is guaranteed present (util-linux is in the image) and already
  encodes the size/model/type semantics we need, including loop/rom exclusion
  via `TYPE`.
- A native `libudev` binding pulls a C dep and more code for no gain here;
  `sysinfo` does not model block topology well. The tool already shells out to
  `systemd-repart`, so a second well-defined subprocess is consistent.

Filtering rules (`disk/enumerate.rs`):
- Keep only `type == "disk"` (drops `loop`, `rom`, `part`).
- Drop `zram*` and read-only devices (`ro == true`).
- Identify and exclude the **live boot medium**: resolve the backing device of
  the mounted ESP / the device carrying the running `/usr` verity volume, and
  exclude that whole parent disk (cross-check `findmnt` / the `MOUNTPOINTS`
  column). Mark it `is_live` and show it greyed as "(install medium)" rather
  than hiding it.

Weakest point: correctly identifying the live medium across USB/ISO/loop boots —
validated by the Phase 1 spike on a real live boot.

---

## 6. repart-config model & generation

### Data model (`repart/model.rs`)

Two partition classes:

- **Fixed** (verbatim from templates, never user-editable): `esp`, `usr`,
  `usr-verity`, `usr-verity-sig`, `usr-b`, `usr-verity-b`, `usr-verity-sig-b`.
  Emitted unchanged (the corrected versions — see Phase 1).
- **Configurable**: `root`, `swap`, `home`. Each gets a `SizeChoice`.

```
enum SizeChoice { Fixed(u64 bytes), Grow { weight: u32 } }

struct PartitionDef {            // generic emitter
  index: u8,                     // NN ordering prefix
  filename: String,              // e.g. "70-root.conf"
  type_: String,
  body: Vec<(String, String)>,   // ordered key=value lines under [Partition]
}
```

### Free-space allocation → repart directives (`layout.rs`)

`allocatable = disk_size - sum(fixed partition max sizes) - gpt/alignment slack`.
Compute exactly from the template `SizeMaxBytes` values (ESP 550M + usr 512M +
usr-verity 64M + sig (~tiny constant reserve, e.g. 16M) across the A and B
slots) plus ~16–32 MiB GPT/alignment slack.

Mapping rules:
- **`SizeChoice::Fixed(n)`** → emit `SizeMinBytes=n` **and** `SizeMaxBytes=n`
  (pins the partition to exactly `n`).
- **`SizeChoice::Grow{weight}`** → emit `SizeMinBytes=<floor>` + `Weight=<weight>`,
  **omit `SizeMaxBytes`**. repart distributes leftover free space across all
  `Weight`-bearing, max-less partitions in proportion to `Weight`. (Exactly the
  stock template behaviour: `home` has no size → soaks remainder; `swap` is
  `64M..1G Weight=333`.)
- Default config mirrors templates: `root` Fixed 1G, `swap` Grow (min 64M,
  weight), `home` Grow (remainder). User may override each.
- `PaddingWeight=` is **not** needed (it reserves *unused* gaps); omit. Noted in
  follow-ups if reserved headroom is ever wanted.

Validation (before leaving Sizing):
- Sum of all `Fixed` choices ≤ `allocatable`.
- At least one `Grow` partition OR fixed sizes exactly fill space (else warn
  about wasted space — non-fatal).
- Per-partition minimums (root recommended ≥ 2G); reject sizes exceeding
  `allocatable`.

### Generation (`repart/generate.rs`)

- Create `/run/archetype-install/repart.d/` (tmpfs, ephemeral — correct place;
  cleaned on reboot).
- Write fixed templates verbatim, then the three configurable files with
  computed size/weight lines.
- Return the directory path + the rendered text (the same bytes the
  Review/dry-run screen displays — single source of truth, no separate preview
  renderer).

Reversibility: generation writes only to `/run` (volatile). Deleting the dir is
a full undo. Nothing on disk changes until the runner executes.

---

## 7. systemd-repart invocation & the /usr data question (TOP RISK)

### The invocation (`repart/runner.rs`)

Dry-run (Review screen / `--dry-run`):
```
systemd-repart --dry-run=yes --definitions=/run/archetype-install/repart.d \
               --json=pretty --no-pager <DEVICE>
```
Execute (after Confirm):
```
systemd-repart --dry-run=no --definitions=/run/archetype-install/repart.d \
               --empty=force          # destroy + repartition the target disk
               --key-file=<luks key>  # for root Encrypt=key-file+tpm2
               [--private-key=… --certificate=…]   # ONLY if sig partitions are regenerated
               --seed=random          # or fixed for reproducible UUIDs
               <DEVICE>
```
`--empty=` choice (`require`/`allow`/`force`/`create`) decides wipe semantics —
for "install onto this disk, destroying it" use `--empty=force`. The Confirm
screen must make that destruction explicit.

### The hard question: where does new `/usr` (squashfs + verity) data come from?

The running live image has the `usr`/`usr-verity`/`usr-verity-sig` partitions
mounted (verity-backed `/usr`). The installer must reproduce them on the target.
The stock sysinstall templates **do not** — they `Format=squashfs` empty. The
fix, to be confirmed by a Phase-1 spike on a real boot:

**Plan of record:** rewrite the `usr*` data partitions to **clone the running
volumes block-for-block** instead of reformatting:

- `usr` / `usr-b` data partitions: replace `Format=squashfs` with
  `CopyBlocks=auto` + keep `Verity=data` + `VerityMatchKey=`. `auto` resolves
  the running system's `Type=usr` backing partition (verity-aware) and copies
  the already-built squashfs blocks. (`CopyBlocks` and `Format` are mutually
  exclusive — `Format` must go.)
- `usr-verity` / `-b`: keep `Verity=hash`; repart **recomputes** the hash tree
  from the cloned data partition. No copy needed.
- `usr-verity-sig` / `-b`: two candidate mechanisms —
  1. `CopyBlocks=auto` to clone the running signature partition (no key needed
     in the live env). **Preferred if `auto` resolves sig partitions.**
  2. `Verity=signature` + pass `--private-key`/`--certificate` so repart
     re-signs the recomputed root hash. Requires shipping signing material into
     the live installer — undesirable.
- A/B: install populates **only the A slot** (`usr`, `usr-verity`,
  `usr-verity-sig`); the B slot partitions are created empty (sized, no
  CopyBlocks) and filled later by the A/B updater. Confirm the B-slot templates
  should drop `Format=squashfs` too (empty, unformatted is fine for a future
  update target).

**This is the weakest assumption in the whole plan** and must be proven before
any UI work is trusted: that `CopyBlocks=auto` correctly resolves the
verity-backed `/usr` (and ideally the sig) of a `root=tmpfs` live system, and
that the resulting target boots. Phase 1 is a throwaway shell spike doing
exactly this by hand (hand-edited repart.d + `systemd-repart` against a scratch
disk/loop file), *before* the Rust binary encodes the conclusion.

### Required post-repart steps (Phase 1 to confirm, Phase 6 to implement)

After partitions are written, the installer likely must:
1. Unlock/mount the new root (LUKS via the key-file just enrolled) and mount the
   cloned `/usr`.
2. `systemd-tmpfiles --root=<target> --boot --create` to seed `/etc` from
   `/usr/share/factory/etc` (per 70-root.conf).
3. ESP/bootloader: ensure the target ESP has the bootloader + UKIs but **not**
   `installer.addon.efi` (else it reboots into the installer). Either
   `CopyFiles` the loader + `EFI/Linux` UKIs explicitly (excluding the addon),
   or copy then delete the addon. Confirm whether `bootctl install --root=` /
   `kernel-install` is needed or repart's ESP `CopyFiles` suffices.

These steps may belong partly in repart `CopyFiles=` directives and partly in
post-exec Rust code; Phase 1 decides the split.

### ESP generation

The stock `00-efi.conf` is just `Type=esp Format=vfat 550M`. The installer's ESP
file must add `CopyFiles=` for the bootloader + UKIs (mirroring
`mkosi.repart/00-efi.conf` but pointing at the *running* `/efi`/`/boot`, and
**excluding** the installer addon).

---

## 8. Dry-run design

- CLI `--dry-run` and a Review-screen toggle both set `App.dry_run`.
- The Review screen renders the exact generated `.conf` text (scrollable), so
  dry-run review and the on-disk files are the same bytes.
- Optionally also run `systemd-repart --dry-run=yes … --json=pretty` and show
  repart's own plan (sizes/UUIDs it would assign) — the real allocation, not
  just our intent. Recommended.
- In dry-run, the flow ends at Result with "no changes made"; Confirm/Progress
  are skipped. Nothing outside `/run` is touched.

---

## 9. Theme & branding module (`theme.rs`)

- Mirror the **afterglow** 16-slot palette as `ratatui::style::Color::Rgb`
  consts: `BG 1c1430`, `FG f4e3ea`, `RED ff6a5e`, `YELLOW ffaa52`,
  `GREEN 9ed17b`, `BLUE 7d8fd6`, `MAGENTA d579c2`, `CYAN 5fc6c4`, plus the 8
  bright variants. Keep the hex in one place; document it as a copy of the
  `vt-palette.sh` afterglow row (note the duplication rather than trying to
  share across the shell/Rust boundary).
- Chevron-ribbon header widget: four Powerline `U+E0B0` () segments in the
  warm→cool accent order (red, yellow, green, blue) with the "archetype"
  wordmark, reusing the logo's segment semantics. The console is kmscon with
  DepartureMono Nerd Font, so `U+E0B0` and truecolor render. Provide a plain
  fallback (solid blocks) guarded by a capability check.
- Header appears on every screen; a larger splash variant on Welcome.

Rejected: re-deriving the ASCII block banner from `archetype-logo.sh` at runtime
— the ribbon header is simpler as native ratatui spans; the block banner is
shell-specific and unneeded in a truecolor TUI.

---

## 10. Packaging & service wiring

- Ship as a Cargo binary; add an Arch `PKGBUILD` (consistent with
  archetype-proxy's `packaging/arch/` convention) producing `archetype-install`
  in `/usr/bin/`.
- **Service unit + target:** `system-install.target` is a systemd-261 upstream
  unit; the live boot already isolates it via the kernel addon. The installer
  needs an `archetype-install.service` (analogue of the old
  `systemd-sysinstall.service`) that:
  - `Type=idle`, `StandardInput=tty`, `StandardOutput=tty`, `TTYPath=/dev/tty1`,
    `TTYReset=yes`, `WantedBy=system-install.target`, runs as root.
  - `ExecStart=/usr/bin/archetype-install`; on success reboot (or
    `SuccessAction`/`FailureAction` to drop to an emergency shell).
  - Conflicts with kmscon/getty on tty1 so the TUI owns the console.
  - **Recommendation:** the **service unit lives in `archetype-build`** (next to
    the repart.sysinstall.d data and the kmscon tty wiring — all image-composition
    concerns), while the **binary + PKGBUILD live in this repo**. archetype-build
    adds `archetype-install` to its package list and ships the unit + the
    corrected repart.d files. Mirrors how the image already owns getty/kmscon unit
    placement. Coordinate via a note in archetype-build (out of scope here).
- The corrected `repart.sysinstall.d` templates (Phase 1) live in archetype-build
  (baked into `/usr/lib`). **Recommendation:** this binary reads them from
  `/usr/lib/repart.sysinstall.d/` at runtime (single source of truth, image owns
  it), falling back to embedded copies for dev/testing.

---

## 11. Phasing

Each phase is independently implementable and verifiable. Phase 1 is a
non-Rust spike and **gates everything** — do not build UI on an unproven
install mechanism.

### Phase 1 — Spike: prove the self-replicating repart install (GATING)
- **What:** by hand (shell), construct a corrected `repart.d` set using
  `CopyBlocks=auto` for `usr`/`usr-verity-sig` (drop `Format=squashfs`), and run
  `systemd-repart --dry-run=no` against a scratch loop file / spare disk on a
  real `root=tmpfs` live boot. Confirm `/usr` data + verity + signature land and
  the target boots. Confirm post-steps (`systemd-tmpfiles --root`, ESP
  population minus the addon).
- **Why:** the entire tool encodes this mechanism; it is the top risk.
- **Verify:** target `/usr` verity validates; system boots in a VM pointed at
  the installed disk.
- **Deliverable:** corrected repart.d templates + a written note of the exact
  `systemd-repart` argv and post-steps. Updates land in **archetype-build**.
- Blocks: Phases 4, 5.

### Phase 2 — Skeleton: crate, theme, TUI loop, Welcome screen
- Cargo.toml + deps; `tui.rs` raw-mode/alt-screen init + panic-restore;
  `event.rs` loop; `theme.rs` afterglow palette + chevron header; `app.rs`
  Screen enum with just Welcome→Quit.
- Verify: `cargo run` shows the themed Welcome splash on a real terminal and
  restores cleanly on quit/panic.
- Blocks: Phases 3, 6.

### Phase 3 — Disk enumeration + DiskSelect screen
- `disk/enumerate.rs` (lsblk json + filters + live-medium exclusion);
  `screens/disk_select.rs`.
- Verify: lists real disks with size/model, excludes the live medium, selection
  stored in `InstallConfig`.
- BlockedBy: Phase 2.

### Phase 4 — repart model + generation + layout math
- `repart/model.rs`, `repart/generate.rs`, `layout.rs`. Fixed templates loaded
  from `/usr/lib/repart.sysinstall.d` (fallback embedded); configurable
  root/swap/home; free-space + validation; write to `/run/...`.
- Verify: unit tests for free-space math + size→directive mapping; generated
  fixed-set files match the corrected templates byte-for-byte; round-trip a
  sample config and diff.
- BlockedBy: Phase 1.

### Phase 5 — Sizing + Review + runner (dry-run path)
- `screens/sizing.rs` (allocation UI + live remaining gauge + validation);
  `screens/review.rs` (render generated text + optional `repart --dry-run=yes`
  json); `repart/runner.rs` dry-run invocation.
- Verify: end-to-end dry-run on a real machine produces correct files and a sane
  repart plan; no disk mutation.
- BlockedBy: Phases 3, 4.

### Phase 6 — Confirm + Progress + execute + Result + reboot handoff
- Destructive Confirm (type disk name); runner execute on a worker thread
  feeding a progress channel; post-exec steps (tmpfiles, ESP/bootloader);
  Result + reboot/exit.
- Verify: full install onto a scratch disk in a VM; installed system boots;
  failure paths surface errors and leave a recoverable console.
- BlockedBy: Phase 5.

### Phase 7 — Packaging + service unit + archetype-build wiring
- PKGBUILD in this repo; `archetype-install.service` recommendation written for
  archetype-build; document the `/usr/lib/repart.sysinstall.d` runtime contract.
- Verify: package builds; service runs the binary on tty1 under
  `system-install.target` in a VM boot.
- BlockedBy: Phase 6.

---

## 12. Rejected alternatives (global)

- **Native repart reimplementation in Rust** (libfdisk/gpt crate): rejected —
  repart's verity/CopyBlocks/LUKS/TPM logic is enormous and security-sensitive;
  the whole design premise is to drive the canonical tool.
- **`tokio` async event loop**: rejected — one subprocess + a console UI doesn't
  justify a runtime; a worker thread + `mpsc` is simpler and signal-friendlier.
- **`libudev`/`sysinfo` for disks**: rejected — `lsblk --json` is present,
  higher-level, and consistent with already shelling to systemd-repart.
- **Embedding repart templates only (no runtime read)**: rejected as primary
  source — the image owns the canonical set; embed only as dev fallback.

---

## 13. Weakest assumption — RESOLVED (provisionally, by Phase 1 spike)

`CopyBlocks=auto` correctly resolves and block-clones the verity-backed `/usr`
partitions of a `root=tmpfs` live system onto the target, producing a bootable
install. Confirmed against the spike's 5 questions (spike/repart-spike.sh):
  1. CopyBlocks=auto resolves the live verity /usr data partition — YES
  2. usr-verity hash rebuilt and validates — YES
  3. usr-verity-sig — CLONES via CopyBlocks=auto (no re-signing / shipped keys
     needed)
  4. ESP excludes installer.addon.efi — YES
  5. Installed image boots in a VM — YES

So the corrected templates (Format=squashfs -> CopyBlocks=auto for the usr data
+ sig partitions) are the design of record, and Phases 4+ proceed on this basis.
CAVEAT: confirmed via the user's reading, not a captured spike run in-image
(getting the script into the image was the blocker). Re-validate end-to-end once
the installer is built into the image and can run on a real boot (PLAN owner's
note). Until then, treat the encrypted-root (TPM2) path and a true bootable
install as still needing an in-image confirmation pass.

---

## 14. Follow-ups (out of scope)

- A/B B-slot population strategy (the updater's job) — confirm B-slot templates
  should be empty/unformatted.
- Reserved free space via `PaddingWeight=` if a future feature wants headroom.
- Network/locale/hostname/user configuration screens (this installer only does
  disk + partitioning; firstboot handles the rest via factory `/etc`).
- TPM2 PCR policy details for the encrypted root (`Encrypt=key-file+tpm2`) —
  enrollment specifics and recovery-key UX.
- A capability probe for Powerline glyph / truecolor support with graceful
  fallback on a non-kmscon console.
- The `archetype-install.service` unit and repart.d template corrections actually
  landing in archetype-build (this plan only recommends; that repo's change is a
  separate task).

---

## 15. Critical files for implementation

- `/home/n0n/src/archetype/archetype-build/mkosi.extra/usr/lib/repart.sysinstall.d/10-usr.conf`
  — the file that must change from `Format=squashfs` to `CopyBlocks=auto`; the
  crux of the install mechanism.
- `/home/n0n/src/archetype/archetype-build/mkosi.repart/10-usr.conf` &
  `00-efi.conf` — the build-time idiom to mirror for cloning/ESP population.
- `/home/n0n/src/archetype/archetype-build/scripts/kernel_addon.sh` — defines the
  `system-install.target` / `root=tmpfs` runtime context the service must fit.
- `/home/n0n/src/archetype/archetype-build/mkosi.postinst` — the `/etc`-from-
  factory tmpfiles seeding (`write_tmpfiles`) and kmscon tty wiring the installer
  must respect/trigger.
- `/home/n0n/src/archetype/archetype-logo.sh` & `/home/n0n/src/archetype/vt-palette.sh`
  — afterglow palette + chevron-ribbon source for `theme.rs`.
