//! Typed model of a single `repart.d` partition definition and its rendering
//! to INI text.
//!
//! Only the directives our templates use are modelled. Fixed partitions are
//! emitted verbatim from the canonical templates (see [`super::generate`]); this
//! model renders the configurable `root`/`swap`/`home` partitions, where only
//! the size directives vary per install.

use std::fmt::Write as _;

/// `Format=` filesystem the partition is created with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Vfat,
    Btrfs,
    Swap,
    Squashfs,
}

impl Format {
    fn as_str(self) -> &'static str {
        match self {
            Format::Vfat => "vfat",
            Format::Btrfs => "btrfs",
            Format::Swap => "swap",
            Format::Squashfs => "squashfs",
        }
    }
}

/// `Verity=` role of the partition within a verity triple.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verity {
    Data,
    Hash,
    Signature,
}

impl Verity {
    fn as_str(self) -> &'static str {
        match self {
            Verity::Data => "data",
            Verity::Hash => "hash",
            Verity::Signature => "signature",
        }
    }
}

/// `CopyBlocks=` source. `auto` block-clones the running system's matching
/// partition; mutually exclusive with [`Format`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyBlocks {
    Auto,
}

impl CopyBlocks {
    fn as_str(self) -> &'static str {
        match self {
            CopyBlocks::Auto => "auto",
        }
    }
}

/// `Encrypt=` mode for the partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encrypt {
    Tpm2,
}

impl Encrypt {
    fn as_str(self) -> &'static str {
        match self {
            Encrypt::Tpm2 => "tpm2",
        }
    }
}

/// One `[Partition]` definition. Absent (`None`) fields emit no line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartitionDef {
    pub type_: String,
    pub size_min_bytes: Option<u64>,
    pub size_max_bytes: Option<u64>,
    pub format: Option<Format>,
    pub encrypt: Option<Encrypt>,
    pub verity: Option<Verity>,
    pub verity_match_key: Option<String>,
    pub copy_blocks: Option<CopyBlocks>,
    pub weight: Option<u32>,
    pub padding_weight: Option<u32>,
}

impl PartitionDef {
    /// Start a definition with only its `Type=` set.
    pub fn new(type_: impl Into<String>) -> Self {
        Self {
            type_: type_.into(),
            ..Self::default()
        }
    }

    /// Render to `repart.d` INI text, including the trailing newline. Fields are
    /// emitted in a fixed, readable order; only present fields produce a line.
    pub fn render(&self) -> String {
        let mut out = String::from("[Partition]\n");
        writeln!(out, "Type={}", self.type_).unwrap();
        if let Some(bytes) = self.size_min_bytes {
            writeln!(out, "SizeMinBytes={bytes}").unwrap();
        }
        if let Some(bytes) = self.size_max_bytes {
            writeln!(out, "SizeMaxBytes={bytes}").unwrap();
        }
        if let Some(format) = self.format {
            writeln!(out, "Format={}", format.as_str()).unwrap();
        }
        if let Some(encrypt) = self.encrypt {
            writeln!(out, "Encrypt={}", encrypt.as_str()).unwrap();
        }
        if let Some(verity) = self.verity {
            writeln!(out, "Verity={}", verity.as_str()).unwrap();
        }
        if let Some(key) = &self.verity_match_key {
            writeln!(out, "VerityMatchKey={key}").unwrap();
        }
        if let Some(copy_blocks) = self.copy_blocks {
            writeln!(out, "CopyBlocks={}", copy_blocks.as_str()).unwrap();
        }
        if let Some(weight) = self.weight {
            writeln!(out, "Weight={weight}").unwrap();
        }
        if let Some(padding_weight) = self.padding_weight {
            writeln!(out, "PaddingWeight={padding_weight}").unwrap();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_fixed_size_partition() {
        let def = PartitionDef {
            size_min_bytes: Some(2147483648),
            size_max_bytes: Some(2147483648),
            format: Some(Format::Btrfs),
            encrypt: Some(Encrypt::Tpm2),
            ..PartitionDef::new("root")
        };
        assert_eq!(
            def.render(),
            "[Partition]\n\
             Type=root\n\
             SizeMinBytes=2147483648\n\
             SizeMaxBytes=2147483648\n\
             Format=btrfs\n\
             Encrypt=tpm2\n"
        );
    }

    #[test]
    fn renders_grow_partition_with_weight_and_no_max() {
        let def = PartitionDef {
            weight: Some(333),
            format: Some(Format::Swap),
            ..PartitionDef::new("swap")
        };
        assert_eq!(
            def.render(),
            "[Partition]\nType=swap\nFormat=swap\nWeight=333\n"
        );
    }

    #[test]
    fn omits_absent_fields() {
        assert_eq!(
            PartitionDef::new("home").render(),
            "[Partition]\nType=home\n"
        );
    }
}
