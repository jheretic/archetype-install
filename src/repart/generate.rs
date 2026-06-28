//! Generate the full `repart.d` definition set under `/run`.
//!
//! The seven fixed partitions are emitted verbatim from the canonical
//! templates, loaded at runtime from [`RUNTIME_TEMPLATE_DIR`] (owned by the
//! image) and falling back to copies embedded at build time so the tool works
//! even when that directory is absent. The three configurable partitions
//! (`root`, `swap`, `home`) are rendered from the user's [`Sizing`] via
//! [`crate::layout`].

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::layout::{self, ConfigurablePartitions, Sizing};

/// Canonical fixed templates, owned by the image. Preferred source at runtime.
pub const RUNTIME_TEMPLATE_DIR: &str = "/usr/lib/repart.sysinstall.d";

/// Volatile output directory; deleting it is a full undo (tmpfs, cleared on
/// reboot).
pub const OUTPUT_DIR: &str = "/run/archetype-install/repart.d";

/// A fixed template: its `repart.d` filename and the build-time embedded copy
/// used when [`RUNTIME_TEMPLATE_DIR`] is unavailable.
struct FixedTemplate {
    filename: &'static str,
    embedded: &'static str,
}

/// The seven fixed partitions, in `repart.d` ordering. The configurable
/// `70`/`80`/`90` files are generated, not listed here.
const FIXED_TEMPLATES: [FixedTemplate; 7] = [
    FixedTemplate {
        filename: "00-efi.conf",
        embedded: include_str!("templates/00-efi.conf"),
    },
    FixedTemplate {
        filename: "10-usr.conf",
        embedded: include_str!("templates/10-usr.conf"),
    },
    FixedTemplate {
        filename: "20-usr-verity.conf",
        embedded: include_str!("templates/20-usr-verity.conf"),
    },
    FixedTemplate {
        filename: "30-usr-verity-sig.conf",
        embedded: include_str!("templates/30-usr-verity-sig.conf"),
    },
    FixedTemplate {
        filename: "40-usr-b.conf",
        embedded: include_str!("templates/40-usr-b.conf"),
    },
    FixedTemplate {
        filename: "50-usr-verity-b.conf",
        embedded: include_str!("templates/50-usr-verity-b.conf"),
    },
    FixedTemplate {
        filename: "60-usr-verity-sig-b.conf",
        embedded: include_str!("templates/60-usr-verity-sig-b.conf"),
    },
];

/// One generated `repart.d` file: its name and final text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedFile {
    pub filename: String,
    pub contents: String,
}

/// Render the complete ten-file definition set for `sizing` on a `disk_bytes`
/// target, without touching the filesystem. Fixed files come from
/// `template_dir` (falling back to embedded copies); configurable files are
/// computed and validated by [`crate::layout::plan`].
pub fn render_set(
    sizing: &Sizing,
    disk_bytes: u64,
    template_dir: &Path,
) -> Result<Vec<RenderedFile>> {
    let mut files = Vec::with_capacity(FIXED_TEMPLATES.len() + 3);

    for template in &FIXED_TEMPLATES {
        files.push(RenderedFile {
            filename: template.filename.to_string(),
            contents: load_fixed(template, template_dir),
        });
    }

    let ConfigurablePartitions { root, swap, home } = layout::plan(sizing, disk_bytes)?;
    files.push(RenderedFile {
        filename: "70-root.conf".to_string(),
        contents: root.render(),
    });
    if let Some(swap) = swap {
        files.push(RenderedFile {
            filename: "80-swap.conf".to_string(),
            contents: swap.render(),
        });
    }
    files.push(RenderedFile {
        filename: "90-home.conf".to_string(),
        contents: home.render(),
    });

    Ok(files)
}

/// Render the set and write it to [`OUTPUT_DIR`], returning the output path and
/// the rendered files (the same bytes a review screen displays).
pub fn generate(sizing: &Sizing, disk_bytes: u64) -> Result<(PathBuf, Vec<RenderedFile>)> {
    let files = render_set(sizing, disk_bytes, Path::new(RUNTIME_TEMPLATE_DIR))?;
    let dir = PathBuf::from(OUTPUT_DIR);
    write_set(&dir, &files)?;
    Ok((dir, files))
}

/// Read a fixed template from `template_dir`, falling back to its embedded copy
/// when the runtime file is missing or unreadable.
fn load_fixed(template: &FixedTemplate, template_dir: &Path) -> String {
    let path = template_dir.join(template.filename);
    fs::read_to_string(&path).unwrap_or_else(|_| template.embedded.to_string())
}

/// Write a rendered set into `dir`, creating it if absent.
fn write_set(dir: &Path, files: &[RenderedFile]) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    for file in files {
        let path = dir.join(&file.filename);
        fs::write(&path, &file.contents)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The canonical templates as they must appear byte-for-byte. Embedded here
    /// independently of the source tree so a drift in either copy is caught.
    fn expected_fixed() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            (
                "00-efi.conf",
                "[Partition]\nType=esp\nSizeMinBytes=550M\nSizeMaxBytes=550M\nFormat=vfat\n",
            ),
            (
                "10-usr.conf",
                "[Partition]\nType=usr\nSizeMinBytes=512M\nSizeMaxBytes=512M\nVerity=data\nVerityMatchKey=usr\n# Clone the running system's verity /usr block-for-block (self-install), NOT\n# Format= an empty squashfs. CopyBlocks and Format are mutually exclusive.\nCopyBlocks=auto\n",
            ),
            (
                "20-usr-verity.conf",
                "[Partition]\nType=usr-verity\nSizeMinBytes=64M\nSizeMaxBytes=64M\nVerity=hash\nVerityMatchKey=usr\n# Clone the running verity hash partition block-for-block. repart will not\n# recompute the hash tree during a self-install; the live hash must be copied.\nCopyBlocks=auto\n",
            ),
            (
                "30-usr-verity-sig.conf",
                "[Partition]\nType=usr-verity-sig\nVerity=signature\nVerityMatchKey=usr\n# Clone the running signature partition (no signing key needed in the live\n# installer; spike confirmed CopyBlocks=auto resolves it).\nCopyBlocks=auto\n",
            ),
            (
                "40-usr-b.conf",
                "[Partition]\nType=usr\nSizeMinBytes=512M\nSizeMaxBytes=512M\nVerity=data\nVerityMatchKey=usr-b\n# Mirror of the A slot (10-usr): clone the running /usr block-for-block so the\n# installed system can boot from either slot immediately. The A/B updater\n# overwrites this on the first image update.\nCopyBlocks=auto\n",
            ),
            (
                "50-usr-verity-b.conf",
                "[Partition]\nType=usr-verity\nSizeMinBytes=64M\nSizeMaxBytes=64M\nVerity=hash\nVerityMatchKey=usr-b\n# Mirror of the A slot (20-usr-verity): clone the running verity hash partition\n# block-for-block. The A/B updater replaces it on the first image update.\nCopyBlocks=auto\n",
            ),
            (
                "60-usr-verity-sig-b.conf",
                "[Partition]\nType=usr-verity-sig\nVerity=signature\nVerityMatchKey=usr-b\n# Mirror of the A slot (30-usr-verity-sig): clone the running signature\n# partition. The A/B updater replaces it on the first image update.\nCopyBlocks=auto\n",
            ),
        ])
    }

    /// Render with a nonexistent template dir so the embedded fallback is used.
    fn render_embedded() -> Vec<RenderedFile> {
        render_set(
            &Sizing::default(),
            512 * 1024 * 1024 * 1024,
            Path::new("/nonexistent/repart.sysinstall.d"),
        )
        .unwrap()
    }

    #[test]
    fn embedded_fixed_files_match_canonical_templates_byte_for_byte() {
        let expected = expected_fixed();
        let rendered = render_embedded();
        for file in &rendered {
            if let Some(want) = expected.get(file.filename.as_str()) {
                assert_eq!(&file.contents, want, "{} drifted", file.filename);
            }
        }
        for name in expected.keys() {
            assert!(
                rendered.iter().any(|f| &f.filename == name),
                "missing fixed file {name}"
            );
        }
    }

    #[test]
    fn renders_full_ten_file_set_in_order() {
        let files = render_embedded();
        let names: Vec<&str> = files.iter().map(|f| f.filename.as_str()).collect();
        assert_eq!(
            names,
            [
                "00-efi.conf",
                "10-usr.conf",
                "20-usr-verity.conf",
                "30-usr-verity-sig.conf",
                "40-usr-b.conf",
                "50-usr-verity-b.conf",
                "60-usr-verity-sig-b.conf",
                "70-root.conf",
                "80-swap.conf",
                "90-home.conf",
            ]
        );
    }

    #[test]
    fn omits_swap_file_when_swap_is_none() {
        let sizing = Sizing {
            swap: None,
            ..Sizing::default()
        };
        let files =
            render_set(&sizing, 512 * 1024 * 1024 * 1024, Path::new("/nonexistent")).unwrap();
        assert!(files.iter().all(|f| f.filename != "80-swap.conf"));
        assert_eq!(files.len(), 9);
    }

    #[test]
    fn prefers_runtime_template_over_embedded() {
        let dir = std::env::temp_dir().join(format!("repart-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let custom = "[Partition]\nType=esp\nSizeMinBytes=1M\n";
        fs::write(dir.join("00-efi.conf"), custom).unwrap();

        let files = render_set(&Sizing::default(), 512 * 1024 * 1024 * 1024, &dir).unwrap();
        let efi = files.iter().find(|f| f.filename == "00-efi.conf").unwrap();
        assert_eq!(efi.contents, custom);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_set_creates_dir_and_files() {
        let dir = std::env::temp_dir().join(format!("repart-write-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let files = render_embedded();
        write_set(&dir, &files).unwrap();

        for file in &files {
            let written = fs::read_to_string(dir.join(&file.filename)).unwrap();
            assert_eq!(written, file.contents);
        }
        assert_eq!(fs::read_dir(&dir).unwrap().count(), files.len());

        fs::remove_dir_all(&dir).unwrap();
    }
}
