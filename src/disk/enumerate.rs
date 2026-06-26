//! Enumerate candidate install-target disks from `lsblk` JSON.
//!
//! Live-medium exclusion: the disk backing the running system must never be
//! offered as a wipe target. `lsblk`'s tree nests partitions and their
//! device-mapper holders (e.g. the dm-verity `/usr`) under the physical disk,
//! so a disk is treated as the live medium when any device in its subtree is
//! mounted at a system path (`/`, `/usr`, `/boot*`, `/efi*`). Detection is
//! fail-safe: anything carrying a system mount is excluded rather than risk
//! offering the running image for destruction.

use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// A whole block device that may be selected as the install target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    pub name: String,
    pub size_bytes: u64,
    pub model: Option<String>,
}

impl Disk {
    /// Model name with a stable fallback for devices that report none.
    pub fn display_model(&self) -> &str {
        self.model.as_deref().unwrap_or("Unknown device")
    }

    /// Size rendered in binary units, e.g. `465.8 GiB`.
    pub fn human_size(&self) -> String {
        const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
        let mut size = self.size_bytes as f64;
        let mut unit = 0;
        while size >= 1024.0 && unit < UNITS.len() - 1 {
            size /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{} {}", self.size_bytes, UNITS[unit])
        } else {
            format!("{size:.1} {}", UNITS[unit])
        }
    }
}

#[derive(Debug, Deserialize)]
struct LsblkOutput {
    blockdevices: Vec<BlockDevice>,
}

#[derive(Debug, Deserialize)]
struct BlockDevice {
    name: String,
    size: u64,
    model: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    ro: bool,
    #[serde(default)]
    mountpoints: Vec<Option<String>>,
    #[serde(default)]
    children: Vec<BlockDevice>,
}

/// Run `lsblk` and return the disks usable as install targets.
pub fn enumerate_disks() -> Result<Vec<Disk>> {
    let output = Command::new("lsblk")
        .args([
            "--json",
            "-b",
            "-o",
            "NAME,SIZE,MODEL,TYPE,RO,RM,MOUNTPOINTS",
        ])
        .output()
        .context("failed to run lsblk")?;

    if !output.status.success() {
        bail!(
            "lsblk exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    parse_lsblk(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `lsblk --json` output and keep only valid install targets.
fn parse_lsblk(json: &str) -> Result<Vec<Disk>> {
    let output: LsblkOutput = serde_json::from_str(json).context("failed to parse lsblk JSON")?;
    Ok(output
        .blockdevices
        .iter()
        .filter(|device| is_install_target(device))
        .map(|device| Disk {
            name: format!("/dev/{}", device.name),
            size_bytes: device.size,
            model: device.model.clone(),
        })
        .collect())
}

/// A device is a target when it is a real, writable disk that is not the
/// pseudo-device family (loop/zram/rom) and is not the live medium.
fn is_install_target(device: &BlockDevice) -> bool {
    device.kind == "disk"
        && !device.ro
        && !is_pseudo_device(&device.name)
        && !carries_system_mount(device)
}

fn is_pseudo_device(name: &str) -> bool {
    name.starts_with("loop") || name.starts_with("zram") || name.starts_with("sr")
}

/// True when this device or any descendant is mounted at a system path.
fn carries_system_mount(device: &BlockDevice) -> bool {
    device
        .mountpoints
        .iter()
        .flatten()
        .any(|mount| is_system_mount(mount))
        || device.children.iter().any(carries_system_mount)
}

fn is_system_mount(mount: &str) -> bool {
    mount == "/"
        || mount == "/usr"
        || mount.starts_with("/usr/")
        || mount.starts_with("/boot")
        || mount.starts_with("/efi")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured `lsblk --json -b -o NAME,SIZE,MODEL,TYPE,RO,RM,MOUNTPOINTS`
    /// modelled on a live Archetype boot:
    /// - `sda`: the live USB medium (verity `/usr` + ESP) -> excluded
    /// - `sdb`: a clean target disk with stale partitions -> kept
    /// - `sdc`: a read-only disk -> excluded
    /// - `loop0`: squashfs loop device -> excluded
    /// - `zram0`: compressed swap -> excluded
    const SAMPLE: &str = r#"{
       "blockdevices": [
          {
             "name": "sda", "size": 16000000000, "model": "Live USB",
             "type": "disk", "ro": false, "rm": true, "mountpoints": [null],
             "children": [
                {
                   "name": "sda1", "size": 536870912, "model": null,
                   "type": "part", "ro": false, "rm": true,
                   "mountpoints": ["/efi"]
                },{
                   "name": "sda2", "size": 1073741824, "model": null,
                   "type": "part", "ro": false, "rm": true,
                   "mountpoints": [null],
                   "children": [
                      {
                         "name": "usr", "size": 1073741824, "model": null,
                         "type": "crypt", "ro": true, "rm": false,
                         "mountpoints": ["/usr"]
                      }
                   ]
                }
             ]
          },{
             "name": "sdb", "size": 512110190592, "model": "Samsung SSD 860",
             "type": "disk", "ro": false, "rm": false, "mountpoints": [null],
             "children": [
                {
                   "name": "sdb1", "size": 512110190592, "model": null,
                   "type": "part", "ro": false, "rm": false,
                   "mountpoints": [null]
                }
             ]
          },{
             "name": "sdc", "size": 256060514304, "model": "Read Only Disk",
             "type": "disk", "ro": true, "rm": false, "mountpoints": [null]
          },{
             "name": "loop0", "size": 1610612736, "model": null,
             "type": "loop", "ro": true, "rm": false, "mountpoints": ["/run/archetype/usr.img"]
          },{
             "name": "zram0", "size": 8589934592, "model": null,
             "type": "disk", "ro": false, "rm": false, "mountpoints": ["[SWAP]"]
          }
       ]
    }"#;

    #[test]
    fn keeps_only_the_clean_target_disk() {
        let disks = parse_lsblk(SAMPLE).unwrap();
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].name, "/dev/sdb");
        assert_eq!(disks[0].size_bytes, 512110190592);
        assert_eq!(disks[0].model.as_deref(), Some("Samsung SSD 860"));
    }

    #[test]
    fn excludes_live_medium_via_nested_usr_and_esp_mounts() {
        let disks = parse_lsblk(SAMPLE).unwrap();
        assert!(disks.iter().all(|disk| disk.name != "/dev/sda"));
    }

    #[test]
    fn excludes_read_only_loop_and_zram() {
        let disks = parse_lsblk(SAMPLE).unwrap();
        let names: Vec<&str> = disks.iter().map(|disk| disk.name.as_str()).collect();
        assert!(!names.contains(&"/dev/sdc"));
        assert!(!names.contains(&"/dev/loop0"));
        assert!(!names.contains(&"/dev/zram0"));
    }

    #[test]
    fn empty_blockdevices_yield_no_disks() {
        let disks = parse_lsblk(r#"{"blockdevices": []}"#).unwrap();
        assert!(disks.is_empty());
    }

    #[test]
    fn human_size_uses_binary_units() {
        let disk = Disk {
            name: "/dev/sdb".into(),
            size_bytes: 512110190592,
            model: None,
        };
        assert_eq!(disk.human_size(), "476.9 GiB");
        assert_eq!(disk.display_model(), "Unknown device");
    }

    #[test]
    fn human_size_keeps_small_sizes_in_bytes() {
        let disk = Disk {
            name: "/dev/sdz".into(),
            size_bytes: 512,
            model: None,
        };
        assert_eq!(disk.human_size(), "512 B");
    }
}
