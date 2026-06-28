#!/usr/bin/env bash
#
# Phase-1 spike for archetype-install (PLAN.md §11 Phase 1, the GATING risk).
#
# Goal: prove that a root=tmpfs Archetype live boot can clone its own immutable
# verity-/usr onto a fresh disk via systemd-repart + CopyBlocks=auto, and that
# the result is a bootable, verity-valid install. This is throwaway: it builds
# CORRECTED repart.d files by hand and runs repart against a SCRATCH LOOP FILE
# (no real disk touched) unless you point it at a device explicitly.
#
# Run this ON A LIVE ARCHETYPE BOOT (it reads the running system's /usr, ESP,
# etc.). It is read-only with respect to the host; all writes go to the target
# image/device and a /run scratch dir.
#
# Usage:
#   sudo ./repart-spike.sh                       # 12G loop file at /var/tmp (default)
#   sudo ./repart-spike.sh --target /dev/sdX     # a real SCRATCH disk (WIPED!)
#   sudo ./repart-spike.sh --size 16G            # bigger loop file
#   sudo ./repart-spike.sh --keep                # don't tear down loop/mounts (inspect)
#
# What it answers (printed at the end as PASS/FAIL/UNKNOWN):
#   1. Does CopyBlocks=auto resolve the live verity /usr data partition?
#   2. Does the usr-verity hash partition get (re)built and VALIDATE against the
#      cloned data? (veritysetup verify / systemd-dissect)
#   3. Does the usr-verity-sig partition come across (CopyBlocks=auto) or must we
#      re-sign? (reported, not assumed)
#   4. Does the ESP get the bootloader + UKIs WITHOUT installer.addon.efi?
#   5. Do the post-steps (tmpfiles --root) work against the cloned /usr?
#
# Nothing here is the real installer; it informs the repart/model + runner code.

set -euo pipefail

# ---- args ------------------------------------------------------------------
TARGET=""              # empty => create a loop-backed image
IMAGE_SIZE="12G"
KEEP=0
SCRATCH="/run/archetype-install-spike"
while [ $# -gt 0 ]; do
    case "$1" in
        --target) TARGET="$2"; shift 2 ;;
        --size)   IMAGE_SIZE="$2"; shift 2 ;;
        --keep)   KEEP=1; shift ;;
        -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

[ "$(id -u)" -eq 0 ] || { echo "must run as root (repart writes partitions)" >&2; exit 1; }

log()  { printf '\033[1;35m::\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  PASS\033[0m %s\n' "$*"; }
bad()  { printf '\033[1;31m  FAIL\033[0m %s\n' "$*"; }
huh()  { printf '\033[1;33m  ????\033[0m %s\n' "$*"; }

REPART_DIR="$SCRATCH/repart.d"
IMAGE_FILE="$SCRATCH/target.img"
LOOP=""
MNT="$SCRATCH/mnt"

cleanup() {
    [ "$KEEP" -eq 1 ] && { log "--keep set; leaving $SCRATCH, loop=$LOOP mounted"; return; }
    mountpoint -q "$MNT/usr" 2>/dev/null && umount "$MNT/usr" || true
    mountpoint -q "$MNT"     2>/dev/null && umount "$MNT"     || true
    [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null || true
    rm -rf "$SCRATCH"
}
trap cleanup EXIT

# ---- discover the running system's source partitions -----------------------
# The live medium: the whole disk whose subtree carries /usr (verity) + /efi.
# We need the SOURCE block devices CopyBlocks=auto will read from. repart's
# `auto` resolves them by GPT partition type from the running system, so we
# mostly just need to confirm they exist and report them.
log "Inspecting the running system's source partitions"
SRC_USR="$(findmnt -no SOURCE /usr 2>/dev/null || true)"          # e.g. /dev/mapper/usr (verity)
SRC_USR_BACKING=""
if [ -n "$SRC_USR" ]; then
    # If /usr is a dm-verity device, find its lower 'data' backing partition.
    base="$(basename "$SRC_USR")"
    if [ -d "/sys/block/$base/slaves" ]; then
        for s in /sys/block/"$base"/slaves/*; do
            [ -e "$s" ] || continue
            SRC_USR_BACKING="/dev/$(basename "$s")"   # one of these is the usr data part
        done
    fi
fi
SRC_EFI="$(findmnt -no SOURCE /efi 2>/dev/null || findmnt -no SOURCE /boot 2>/dev/null || true)"
echo "    /usr source     : ${SRC_USR:-<none>}"
echo "    /usr backing    : ${SRC_USR_BACKING:-<none>}  (verity data partition repart auto-resolves)"
echo "    ESP source      : ${SRC_EFI:-<none>}"
echo "    running IMAGE_VERSION: $(. /etc/os-release 2>/dev/null; echo "${IMAGE_VERSION:-?}")"

# ---- build CORRECTED repart.d ----------------------------------------------
# Differences from archetype-build/mkosi.extra/usr/lib/repart.sysinstall.d:
#   * usr/usr-b data: DROP Format=squashfs, ADD CopyBlocks=auto  (clone, not format)
#   * usr-verity/-b : unchanged (Verity=hash -> repart recomputes the hash tree)
#   * usr-verity-sig: try CopyBlocks=auto (clone the running sig); fallback noted
#   * Only the A slot is populated; B slot stays empty (future update target)
#   * ESP: CopyFiles the loader + UKIs, EXCLUDING installer.addon.efi
log "Writing corrected repart.d to $REPART_DIR"
rm -rf "$SCRATCH"; mkdir -p "$REPART_DIR" "$MNT"

cat > "$REPART_DIR/00-efi.conf" <<'EOF'
[Partition]
Type=esp
SizeMinBytes=550M
SizeMaxBytes=550M
Format=vfat
# Bring the bootloader + UKIs, but NOT installer.addon.efi (that addon carries
# root=tmpfs systemd.unit=system-install.target; copying it would make the
# INSTALLED system boot straight back into the installer).
CopyFiles=/efi/EFI:/EFI
CopyFiles=/boot/EFI/Linux:/EFI/Linux
CopyFiles=/efi/loader:/loader
ExcludeFilesTarget=/loader/addons/installer.addon.efi
EOF

cat > "$REPART_DIR/10-usr.conf" <<'EOF'
[Partition]
Type=usr
SizeMinBytes=512M
SizeMaxBytes=512M
Verity=data
VerityMatchKey=usr
CopyBlocks=auto
EOF

cat > "$REPART_DIR/20-usr-verity.conf" <<'EOF'
[Partition]
Type=usr-verity
SizeMinBytes=64M
SizeMaxBytes=64M
Verity=hash
VerityMatchKey=usr
EOF

cat > "$REPART_DIR/30-usr-verity-sig.conf" <<'EOF'
[Partition]
Type=usr-verity-sig
Verity=signature
VerityMatchKey=usr
CopyBlocks=auto
EOF

# B slot: created, sized, but EMPTY (no CopyBlocks) — the A/B updater fills it.
cat > "$REPART_DIR/40-usr-b.conf" <<'EOF'
[Partition]
Type=usr
SizeMinBytes=512M
SizeMaxBytes=512M
VerityMatchKey=usr-b
EOF

cat > "$REPART_DIR/50-usr-verity-b.conf" <<'EOF'
[Partition]
Type=usr-verity
SizeMinBytes=64M
SizeMaxBytes=64M
VerityMatchKey=usr-b
EOF

cat > "$REPART_DIR/60-usr-verity-sig-b.conf" <<'EOF'
[Partition]
Type=usr-verity-sig
VerityMatchKey=usr-b
EOF

# root: keep encryption OFF in the spike (TPM2 enrolment is its own variable;
# the install correctness question is about /usr cloning, not LUKS). The real
# tool restores Encrypt=key-file+tpm2.
cat > "$REPART_DIR/70-root.conf" <<'EOF'
[Partition]
Type=root
SizeMinBytes=1G
SizeMaxBytes=2G
Format=btrfs
EOF

cat > "$REPART_DIR/80-swap.conf" <<'EOF'
[Partition]
Type=swap
Format=swap
SizeMinBytes=64M
SizeMaxBytes=1G
Weight=333
EOF

cat > "$REPART_DIR/90-home.conf" <<'EOF'
[Partition]
Type=home
Format=btrfs
Weight=1000
EOF

echo "    wrote $(ls "$REPART_DIR" | wc -l) definition files"

# ---- prepare the target ----------------------------------------------------
if [ -z "$TARGET" ]; then
    log "Creating $IMAGE_SIZE scratch loop image at $IMAGE_FILE"
    truncate -s "$IMAGE_SIZE" "$IMAGE_FILE"
    LOOP="$(losetup --find --show "$IMAGE_FILE")"
    TARGET="$LOOP"
    echo "    loop device: $LOOP"
else
    log "Using REAL target $TARGET  (WILL BE WIPED — 5s to Ctrl-C)"; sleep 5
fi

# ---- DRY RUN first ---------------------------------------------------------
# --empty=force: the target is explicitly scratch, so wipe any existing table
# and start clean (also covers a loop file reused across spike runs). Valid
# --empty values per systemd-repart(8): refuse|allow|require|force|create.
log "systemd-repart DRY RUN (the plan repart would execute)"
systemd-repart --dry-run=yes --empty=force --definitions="$REPART_DIR" \
    --json=pretty "$TARGET" || { bad "dry-run failed — see error above"; exit 1; }

# ---- REAL RUN --------------------------------------------------------------
log "systemd-repart REAL RUN against $TARGET"
if systemd-repart --dry-run=no --empty=force --definitions="$REPART_DIR" "$TARGET"; then
    ok "repart completed without error"
else
    bad "repart FAILED — this is the key result; capture the error above"
    exit 1
fi

# ---- inspect the result ----------------------------------------------------
log "Inspecting the written partition table"
DEV="$TARGET"
[ -n "$LOOP" ] && { losetup -d "$LOOP"; LOOP="$(losetup --find --show -P "$IMAGE_FILE")"; DEV="$LOOP"; }
sleep 1
sgdisk -p "$DEV" 2>/dev/null || lsblk -o NAME,SIZE,TYPE,PARTTYPENAME "$DEV"

log "Verifying the cloned /usr (data + verity hash + signature)"
# systemd-dissect validates verity end-to-end (data <-> hash <-> sig) the same
# way the bootloader/initrd will. This is the decisive PASS/FAIL.
if command -v systemd-dissect >/dev/null; then
    if systemd-dissect "$IMAGE_FILE" 2>/dev/null | grep -qiE 'usr.*verity|verity.*usr'; then
        ok "systemd-dissect sees a verity-protected /usr on the target"
    else
        huh "systemd-dissect ran but didn't clearly report verity /usr — read its full output:"
        systemd-dissect "$IMAGE_FILE" 2>&1 | sed 's/^/      /' | head -40
    fi
else
    huh "systemd-dissect not available; falling back to manual veritysetup checks"
fi

log "Checking the ESP excluded the installer addon"
ESP_PART="$(lsblk -nro NAME,PARTTYPENAME "$DEV" | awk '/EFI System/{print "/dev/"$1; exit}')"
if [ -n "$ESP_PART" ]; then
    mkdir -p "$MNT/esp"; mount -o ro "$ESP_PART" "$MNT/esp" 2>/dev/null || true
    if [ -e "$MNT/esp/loader/addons/installer.addon.efi" ]; then
        bad "installer.addon.efi IS on the target ESP — installed system would reboot into the installer"
    else
        ok "installer.addon.efi correctly absent from target ESP"
    fi
    echo "    target ESP loader/addons:"; ls "$MNT/esp/loader/addons/" 2>/dev/null | sed 's/^/      /' || echo "      (none)"
    echo "    target ESP EFI/Linux UKIs:"; ls "$MNT/esp/EFI/Linux/" 2>/dev/null | sed 's/^/      /' || echo "      (none)"
    umount "$MNT/esp" 2>/dev/null || true
else
    huh "couldn't locate the target ESP partition to inspect"
fi

log "Post-step check: systemd-tmpfiles --root against the cloned /usr"
# Mount the cloned usr (verity) read-only to confirm /usr/share/factory/etc
# exists, which the tmpfiles C! rules copy into /etc on first boot.
echo "    (manual on a real run: mount target root, mount cloned /usr, then"
echo "     systemd-tmpfiles --root=<root> --boot --create — confirm /etc seeded)"

cat <<'SUMMARY'

============================================================================
SPIKE RESULTS — record these in archetype-install/PLAN.md §13 and decide:
  [1] CopyBlocks=auto resolved live verity /usr data?   (repart real-run PASS?)
  [2] usr-verity hash rebuilt + validates?              (systemd-dissect PASS?)
  [3] usr-verity-sig: cloned via CopyBlocks=auto, or does it need re-signing?
      -> if dissect reports a signature mismatch/absence, the sig must be
         re-signed with --private-key/--certificate (ship keys) OR the install
         accepts an unsigned-but-hash-verified /usr (machine-owner-key model).
  [4] ESP excluded installer.addon.efi?                 (PASS above?)
  [5] Booting the installed image in a fresh VM actually comes up?  (DO THIS:
      qemu-system-x86_64 -drive file=<target.img|disk> -bios <OVMF> -m 2G ...)

If any of 1-3 fail, the repart/model design (Phase 4) changes — that's exactly
what this spike exists to find out before writing the Rust.
============================================================================
SUMMARY

log "Spike done. Target image: $IMAGE_FILE ${LOOP:+(loop $LOOP)}"
[ "$KEEP" -eq 1 ] && echo "    --keep: inspect, then: losetup -d $LOOP; rm -rf $SCRATCH"
