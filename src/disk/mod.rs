//! Block-device enumeration for choosing an install target.

mod enumerate;

pub use enumerate::{enumerate_disks, Disk};
