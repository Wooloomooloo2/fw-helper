use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A filesystem root for hardware access.
///
/// All reads and writes are relative to `root`, which makes the entire layer
/// testable against a fixture directory instead of real hardware.
#[derive(Debug, Clone)]
pub struct Sysfs {
    root: PathBuf,
}

impl Default for Sysfs {
    fn default() -> Self {
        Self::new("/")
    }
}

impl Sysfs {
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// Resolve a relative sysfs path. Leading slashes are tolerated so callers may
    /// write either `/sys/...` or `sys/...`.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel.trim_start_matches('/'))
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.path(rel).exists()
    }

    pub fn read_string(&self, rel: &str) -> io::Result<String> {
        Ok(fs::read_to_string(self.path(rel))?.trim().to_string())
    }

    pub fn read_u64(&self, rel: &str) -> io::Result<u64> {
        let raw = self.read_string(rel)?;
        raw.parse().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{rel}: cannot parse {raw:?} as u64: {e}"),
            )
        })
    }

    pub fn read_i64(&self, rel: &str) -> io::Result<i64> {
        let raw = self.read_string(rel)?;
        raw.parse().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{rel}: cannot parse {raw:?} as i64: {e}"),
            )
        })
    }

    pub fn write_string(&self, rel: &str, value: &str) -> io::Result<()> {
        fs::write(self.path(rel), value)
    }

    /// Locate a hwmon node by its `name` file.
    ///
    /// hwmon indices are assigned in probe order and are **not** stable across boots —
    /// on the reference machine `cros_ec` was `hwmon11`, but nothing guarantees that.
    /// Returns a path relative to the root, suitable for the other methods here.
    pub fn find_hwmon(&self, name: &str) -> Option<String> {
        let dir = self.path("sys/class/hwmon");
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let base = entry.file_name().to_str()?.to_string();
            let rel = format!("sys/class/hwmon/{base}");
            if self.read_string(&format!("{rel}/name")).ok().as_deref() == Some(name) {
                return Some(rel);
            }
        }
        None
    }

    /// Resolve a RAPL zone by its `name`, e.g. `core`, `uncore`, `package-0`.
    ///
    /// Same reasoning as [`Self::find_hwmon`]: the numbering is positional
    /// (`intel-rapl:0:1`) and describes where a zone sits in the tree, not what it
    /// measures. On the reference board `intel-rapl:0:1` is `uncore` — the iGPU rail —
    /// but nothing guarantees that ordering on another part, and reading the wrong
    /// subzone yields a plausible number for the wrong thing.
    ///
    /// Searches subzones as well as top-level zones, since `core` and `uncore` are
    /// always children of a package zone.
    pub fn find_powercap(&self, name: &str) -> Option<String> {
        let base = "sys/class/powercap";
        for entry in fs::read_dir(self.path(base)).ok()?.flatten() {
            let zone = entry.file_name().to_str()?.to_string();
            let rel = format!("{base}/{zone}");
            if self.read_string(&format!("{rel}/name")).ok().as_deref() == Some(name) {
                return Some(rel);
            }
            // Subzones live inside their parent and are named with the parent's prefix.
            let Ok(children) = fs::read_dir(self.path(&rel)) else {
                continue;
            };
            for child in children.flatten() {
                let Some(sub) = child.file_name().to_str().map(String::from) else {
                    continue;
                };
                if !sub.starts_with(&zone) {
                    continue;
                }
                let sub_rel = format!("{rel}/{sub}");
                if self.read_string(&format!("{sub_rel}/name")).ok().as_deref() == Some(name) {
                    return Some(sub_rel);
                }
            }
        }
        None
    }
}
