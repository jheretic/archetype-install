//! Free-space math for the configurable `root`/`swap`/`home` partitions.
//!
//! The seven fixed partitions (ESP + the A/B `usr` verity triples) occupy a
//! constant amount of every target disk; the remainder is allocatable to the
//! user's choices. This module computes that remainder, validates a sizing
//! request against it, and maps each choice onto `repart.d` size directives.
//!
//! Consumed by the Sizing screen in a later phase; the math and validation API
//! is complete now ahead of that wiring.
#![allow(dead_code)]

use crate::repart::model::{Encrypt, Format, PartitionDef};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// `SizeMaxBytes` of each fixed partition, mirroring the canonical templates.
/// MUST stay in lockstep with templates/10-usr.conf + 40-usr-b.conf (and the
/// archetype-build shipped copies): this is subtracted from the disk to size
/// the configurable root/swap/home, so an understated value over-allocates them
/// and repart placement can fail on a tight disk.
const ESP_BYTES: u64 = 550 * MIB;
const USR_BYTES: u64 = GIB;
const USR_VERITY_BYTES: u64 = 64 * MIB;
/// The `usr-verity-sig` templates carry no `SizeMaxBytes`; repart sizes them to
/// the (tiny) signature payload. We reserve a fixed amount per slot for the
/// free-space estimate.
const USR_VERITY_SIG_BYTES: u64 = 16 * MIB;
/// GPT primary/backup tables plus 1 MiB partition alignment slack.
const GPT_OVERHEAD_BYTES: u64 = 16 * MIB;

/// One A or B `usr` slot: data + verity hash + verity signature.
const USR_SLOT_BYTES: u64 = USR_BYTES + USR_VERITY_BYTES + USR_VERITY_SIG_BYTES;

/// Total space consumed by all fixed partitions plus GPT overhead.
pub const FIXED_TOTAL_BYTES: u64 = ESP_BYTES + 2 * USR_SLOT_BYTES + GPT_OVERHEAD_BYTES;

/// Smallest root partition we accept, fixed or as a grow floor.
pub const ROOT_MIN_BYTES: u64 = GIB;

/// How a configurable partition claims space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeChoice {
    /// Pinned to exactly this many bytes (`SizeMinBytes` == `SizeMaxBytes`).
    Fixed(u64),
    /// Grows to soak leftover free space in proportion to `weight`, never
    /// shrinking below `min_bytes` (emitted as `SizeMinBytes` + `Weight`, no
    /// `SizeMaxBytes`).
    Grow { weight: u32, min_bytes: u64 },
}

impl SizeChoice {
    /// Bytes this choice is guaranteed to occupy regardless of growth.
    fn committed_bytes(self) -> u64 {
        match self {
            SizeChoice::Fixed(bytes) => bytes,
            SizeChoice::Grow { min_bytes, .. } => min_bytes,
        }
    }

    /// Apply this choice's size directives onto a partition definition.
    fn apply(self, def: &mut PartitionDef) {
        match self {
            SizeChoice::Fixed(bytes) => {
                def.size_min_bytes = Some(bytes);
                def.size_max_bytes = Some(bytes);
            }
            SizeChoice::Grow { weight, min_bytes } => {
                if min_bytes > 0 {
                    def.size_min_bytes = Some(min_bytes);
                }
                def.weight = Some(weight);
            }
        }
    }
}

/// A user's size choices for the three configurable partitions. `swap` and
/// `home` are optional (an omitted one leaves its space as free GPT space);
/// `root` is always present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sizing {
    pub root: SizeChoice,
    pub swap: Option<SizeChoice>,
    pub home: Option<SizeChoice>,
}

impl Default for Sizing {
    /// Mirrors the canonical templates: root fixed at 1 GiB, swap growing from
    /// a 64 MiB floor at weight 333, home soaking the remainder.
    fn default() -> Self {
        Self {
            root: SizeChoice::Fixed(GIB),
            swap: Some(SizeChoice::Grow {
                weight: 333,
                min_bytes: 64 * MIB,
            }),
            home: Some(SizeChoice::Grow {
                weight: 1000,
                min_bytes: 0,
            }),
        }
    }
}

/// The configurable partitions rendered as `repart.d` definitions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigurablePartitions {
    pub root: PartitionDef,
    pub swap: Option<PartitionDef>,
    pub home: Option<PartitionDef>,
}

/// A reason a [`Sizing`] cannot be realised on a given disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// The disk cannot even hold the fixed partitions.
    DiskTooSmall {
        disk_bytes: u64,
        required_bytes: u64,
    },
    /// The guaranteed sizes exceed the allocatable free space.
    OverAllocated {
        requested_bytes: u64,
        available_bytes: u64,
    },
    /// The root partition's guaranteed size is below [`ROOT_MIN_BYTES`].
    RootTooSmall {
        requested_bytes: u64,
        minimum_bytes: u64,
    },
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutError::DiskTooSmall {
                disk_bytes,
                required_bytes,
            } => write!(
                f,
                "disk is {disk_bytes} bytes but the fixed partitions need {required_bytes} bytes"
            ),
            LayoutError::OverAllocated {
                requested_bytes,
                available_bytes,
            } => write!(
                f,
                "requested {requested_bytes} bytes but only {available_bytes} bytes are available"
            ),
            LayoutError::RootTooSmall {
                requested_bytes,
                minimum_bytes,
            } => write!(
                f,
                "root is {requested_bytes} bytes but the minimum is {minimum_bytes} bytes"
            ),
        }
    }
}

impl std::error::Error for LayoutError {}

/// Bytes left for the configurable partitions after the fixed set and GPT
/// overhead are subtracted from the disk.
pub fn allocatable_bytes(disk_bytes: u64) -> Result<u64, LayoutError> {
    disk_bytes
        .checked_sub(FIXED_TOTAL_BYTES)
        .filter(|&free| free > 0)
        .ok_or(LayoutError::DiskTooSmall {
            disk_bytes,
            required_bytes: FIXED_TOTAL_BYTES,
        })
}

/// Validate a sizing request against a disk and render the configurable
/// partitions. Errors if the disk is too small, the guaranteed sizes
/// over-allocate the free space, or root is under [`ROOT_MIN_BYTES`].
pub fn plan(sizing: &Sizing, disk_bytes: u64) -> Result<ConfigurablePartitions, LayoutError> {
    let available_bytes = allocatable_bytes(disk_bytes)?;

    let root_committed = sizing.root.committed_bytes();
    if root_committed < ROOT_MIN_BYTES {
        return Err(LayoutError::RootTooSmall {
            requested_bytes: root_committed,
            minimum_bytes: ROOT_MIN_BYTES,
        });
    }

    let requested_bytes = root_committed
        + sizing.swap.map_or(0, SizeChoice::committed_bytes)
        + sizing.home.map_or(0, SizeChoice::committed_bytes);
    if requested_bytes > available_bytes {
        return Err(LayoutError::OverAllocated {
            requested_bytes,
            available_bytes,
        });
    }

    Ok(ConfigurablePartitions {
        root: build(
            PartitionDef {
                format: Some(Format::Btrfs),
                encrypt: Some(Encrypt::Tpm2),
                ..PartitionDef::new("root")
            },
            sizing.root,
        ),
        // Format=empty + Label=SWAP: the swap partition is an ENCRYPTED,
        // random-key swap. The installer writes an /etc/crypttab line
        // (PARTLABEL=SWAP, key /dev/urandom, `swap` option) so the installed
        // system opens it in plain dm-crypt and mkswaps the mapper fresh every
        // boot, plus an /etc/fstab `swap` line for the mapper. We deliberately
        // do NOT pre-format it as swap (Format=empty) so no plaintext swap
        // signature is written at install; and the fstab `swap` entry disables
        // gpt-auto's swap auto-activation (systemd #6192), so the raw partition
        // is never auto-swapped on unencrypted. Needs the stable SWAP label for
        // crypttab's PARTLABEL= to find it (repart would otherwise label it
        // "swap", which we could match, but an explicit label mirrors HOME).
        swap: sizing.swap.map(|choice| {
            build(
                PartitionDef {
                    label: Some("SWAP".to_string()),
                    format: Some(Format::Empty),
                    ..PartitionDef::new("swap")
                },
                choice,
            )
        }),
        // Format=empty + Label=HOME: the installer formats this partition into a
        // keyed dm-integrity volume itself and records it in integritytab via
        // PARTLABEL=HOME, so the partition must carry that GPT label (repart
        // would otherwise default the label to "home", and the installed
        // system's integritysetup-generator would never find PARTLABEL=HOME).
        home: sizing.home.map(|choice| {
            build(
                PartitionDef {
                    label: Some("HOME".to_string()),
                    format: Some(Format::Empty),
                    ..PartitionDef::new("home")
                },
                choice,
            )
        }),
    })
}

fn build(mut def: PartitionDef, choice: SizeChoice) -> PartitionDef {
    choice.apply(&mut def);
    def
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISK_512G: u64 = 512 * GIB;

    #[test]
    fn allocatable_subtracts_fixed_set_and_overhead() {
        let free = allocatable_bytes(DISK_512G).unwrap();
        assert_eq!(free, DISK_512G - FIXED_TOTAL_BYTES);
    }

    #[test]
    fn allocatable_rejects_disk_smaller_than_fixed_set() {
        let err = allocatable_bytes(FIXED_TOTAL_BYTES).unwrap_err();
        assert_eq!(
            err,
            LayoutError::DiskTooSmall {
                disk_bytes: FIXED_TOTAL_BYTES,
                required_bytes: FIXED_TOTAL_BYTES,
            }
        );
    }

    #[test]
    fn plan_accepts_default_sizing() {
        let plan = plan(&Sizing::default(), DISK_512G).unwrap();
        assert_eq!(plan.root.size_min_bytes, Some(GIB));
        assert_eq!(plan.root.size_max_bytes, Some(GIB));
        assert_eq!(plan.swap.as_ref().unwrap().weight, Some(333));
        let home = plan.home.as_ref().unwrap();
        assert_eq!(home.weight, Some(1000));
        assert_eq!(home.size_max_bytes, None);
    }

    #[test]
    fn omits_home_partition_when_home_is_none() {
        let sizing = Sizing {
            home: None,
            ..Sizing::default()
        };
        let plan = plan(&sizing, DISK_512G).unwrap();
        assert!(plan.home.is_none());
    }

    #[test]
    fn home_partition_is_labelled_and_empty_for_integrity() {
        // The installer formats home into a dm-integrity volume and references
        // it via PARTLABEL=HOME, so the generated partition MUST carry Label=HOME
        // and Format=empty (repart would otherwise label it "home").
        let plan = plan(&Sizing::default(), DISK_512G).unwrap();
        let home = plan.home.as_ref().unwrap();
        assert_eq!(home.label.as_deref(), Some("HOME"));
        assert_eq!(home.format, Some(Format::Empty));
        assert!(home.render().contains("Label=HOME"));
        assert!(home.render().contains("Format=empty"));
    }

    #[test]
    fn fixed_choice_pins_min_and_max() {
        let sizing = Sizing {
            root: SizeChoice::Fixed(4 * GIB),
            swap: None,
            home: Some(SizeChoice::Fixed(100 * GIB)),
        };
        let plan = plan(&sizing, DISK_512G).unwrap();
        assert_eq!(plan.root.size_min_bytes, Some(4 * GIB));
        assert_eq!(plan.root.size_max_bytes, Some(4 * GIB));
        assert!(plan.swap.is_none());
        let home = plan.home.as_ref().unwrap();
        assert_eq!(home.size_min_bytes, Some(100 * GIB));
        assert_eq!(home.size_max_bytes, Some(100 * GIB));
    }

    #[test]
    fn grow_choice_emits_weight_and_floor_but_no_max() {
        let sizing = Sizing {
            root: SizeChoice::Fixed(2 * GIB),
            swap: None,
            home: Some(SizeChoice::Grow {
                weight: 500,
                min_bytes: 10 * GIB,
            }),
        };
        let plan = plan(&sizing, DISK_512G).unwrap();
        let home = plan.home.as_ref().unwrap();
        assert_eq!(home.size_min_bytes, Some(10 * GIB));
        assert_eq!(home.size_max_bytes, None);
        assert_eq!(home.weight, Some(500));
    }

    #[test]
    fn rejects_over_allocation() {
        let sizing = Sizing {
            root: SizeChoice::Fixed(400 * GIB),
            swap: Some(SizeChoice::Fixed(100 * GIB)),
            home: Some(SizeChoice::Fixed(100 * GIB)),
        };
        let err = plan(&sizing, DISK_512G).unwrap_err();
        match err {
            LayoutError::OverAllocated {
                requested_bytes,
                available_bytes,
            } => {
                assert_eq!(requested_bytes, 600 * GIB);
                assert_eq!(available_bytes, DISK_512G - FIXED_TOTAL_BYTES);
            }
            other => panic!("expected OverAllocated, got {other:?}"),
        }
    }

    #[test]
    fn rejects_root_below_floor() {
        let sizing = Sizing {
            root: SizeChoice::Fixed(512 * MIB),
            swap: None,
            home: Some(SizeChoice::Grow {
                weight: 1000,
                min_bytes: 0,
            }),
        };
        let err = plan(&sizing, DISK_512G).unwrap_err();
        assert_eq!(
            err,
            LayoutError::RootTooSmall {
                requested_bytes: 512 * MIB,
                minimum_bytes: ROOT_MIN_BYTES,
            }
        );
    }

    #[test]
    fn grow_floor_counts_toward_allocation() {
        let sizing = Sizing {
            root: SizeChoice::Grow {
                weight: 1000,
                min_bytes: ROOT_MIN_BYTES,
            },
            swap: None,
            home: Some(SizeChoice::Grow {
                weight: 1000,
                min_bytes: DISK_512G,
            }),
        };
        let err = plan(&sizing, DISK_512G).unwrap_err();
        assert!(matches!(err, LayoutError::OverAllocated { .. }));
    }
}
