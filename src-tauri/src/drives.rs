use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drive {
    pub id: String,
    pub name: String,
    pub size_bytes: u64,
    pub removable: bool,
    pub device_path: String,
}

#[tauri::command]
pub async fn list_drives() -> Result<Vec<Drive>, String> {
    #[cfg(target_os = "macos")]
    {
        macos::list().map_err(|e| e.to_string())
    }
    #[cfg(target_os = "linux")]
    {
        linux::list().map_err(|e| e.to_string())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err("Unsupported platform".into())
    }
}

/// Query the size of a single drive by ID. Used by the flash backend to
/// validate the image size independently of the frontend-supplied value.
pub fn query_drive_size(drive_id: &str) -> anyhow::Result<u64> {
    #[cfg(target_os = "macos")]
    {
        macos::size(drive_id)
    }
    #[cfg(target_os = "linux")]
    {
        linux::size(drive_id)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        anyhow::bail!("Unsupported platform")
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::Drive;
    use anyhow::{anyhow, Context, Result};
    use std::process::Command;

    /// Enumerate external disks via `diskutil list -plist external`, then
    /// fetch per-disk details via `diskutil info -plist <id>`.
    ///
    /// Using `external` filters out the boot drive automatically — this
    /// is macOS's own notion of "removable / user-facing" media.
    pub fn list() -> Result<Vec<Drive>> {
        let output = Command::new("diskutil")
            .args(["list", "-plist", "external"])
            .output()
            .context("failed to run diskutil list")?;

        if !output.status.success() {
            return Err(anyhow!(
                "diskutil list failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let plist: plist::Value =
            plist::from_bytes(&output.stdout).context("failed to parse diskutil list plist")?;

        let disk_ids = plist
            .as_dictionary()
            .and_then(|d| d.get("WholeDisks"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_string().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let mut drives = Vec::with_capacity(disk_ids.len());
        for id in disk_ids {
            match info(&id) {
                Ok(drive) => drives.push(drive),
                Err(e) => eprintln!("skipping {id}: {e}"),
            }
        }
        Ok(drives)
    }

    pub fn size(id: &str) -> Result<u64> {
        Ok(info(id)?.size_bytes)
    }

    /// A mounted `.dmg` is external, ejectable, and `RemovableMedia: true` by
    /// diskutil's reckoning, so it reaches this point alongside USB sticks.
    /// `VirtualOrPhysical` is what separates them. Absent key means physical —
    /// hiding a real drive is worse than listing a disk image.
    pub(super) fn is_virtual(dict: &plist::Dictionary) -> bool {
        dict.get("VirtualOrPhysical")
            .and_then(|v| v.as_string())
            .is_some_and(|s| s.eq_ignore_ascii_case("virtual"))
    }

    fn info(id: &str) -> Result<Drive> {
        let output = Command::new("diskutil")
            .args(["info", "-plist", id])
            .output()
            .context("failed to run diskutil info")?;

        if !output.status.success() {
            return Err(anyhow!(
                "diskutil info {id} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let plist: plist::Value = plist::from_bytes(&output.stdout)?;
        let dict = plist
            .as_dictionary()
            .ok_or_else(|| anyhow!("diskutil info: not a dict"))?;

        if is_virtual(dict) {
            return Err(anyhow!("{id} is a disk image, not physical media"));
        }

        let size_bytes = dict
            .get("TotalSize")
            .and_then(|v| v.as_unsigned_integer())
            .unwrap_or(0);

        let name = dict
            .get("MediaName")
            .or_else(|| dict.get("IORegistryEntryName"))
            .and_then(|v| v.as_string())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| id.to_string());

        let removable = dict
            .get("RemovableMedia")
            .and_then(|v| v.as_boolean())
            .unwrap_or(true);

        let device_path = format!("/dev/{id}");

        Ok(Drive {
            id: id.to_string(),
            name,
            size_bytes,
            removable,
            device_path,
        })
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::macos::is_virtual;
    use plist::{Dictionary, Value};

    fn dict(entries: &[(&str, &str)]) -> Dictionary {
        let mut d = Dictionary::new();
        for (k, v) in entries {
            d.insert(k.to_string(), Value::String(v.to_string()));
        }
        d
    }

    #[test]
    fn disk_images_are_virtual() {
        assert!(is_virtual(&dict(&[("VirtualOrPhysical", "Virtual")])));
        assert!(is_virtual(&dict(&[("VirtualOrPhysical", "virtual")])));
    }

    #[test]
    fn real_media_is_not() {
        assert!(!is_virtual(&dict(&[("VirtualOrPhysical", "Physical")])));
        // Absent key: show the drive rather than hide it.
        assert!(!is_virtual(&dict(&[("RemovableMedia", "true")])));
        assert!(!is_virtual(&Dictionary::new()));
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::Drive;
    use anyhow::{anyhow, Context, Result};
    use std::process::Command;

    /// Enumerate block devices via `lsblk --json -o NAME,SIZE,TYPE,RM,MODEL,PATH`.
    /// Filter to removable disks (RM=true) and exclude loop/rom types.
    pub fn list() -> Result<Vec<Drive>> {
        let output = Command::new("lsblk")
            .args(["--json", "-b", "-o", "NAME,SIZE,TYPE,RM,MODEL,PATH"])
            .output()
            .context("failed to run lsblk")?;

        if !output.status.success() {
            return Err(anyhow!(
                "lsblk failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let blockdevs = json
            .get("blockdevices")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("lsblk: missing blockdevices"))?;

        let mut drives = Vec::new();
        for dev in blockdevs {
            let kind = dev.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if kind != "disk" {
                continue;
            }
            let removable = dev.get("rm").and_then(|v| v.as_bool()).unwrap_or(false);
            if !removable {
                continue;
            }
            let name = dev
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let path = dev
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("/dev/{name}"));
            let size_bytes = dev.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
            let model = dev
                .get("model")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| name.clone());

            drives.push(Drive {
                id: name,
                name: model,
                size_bytes,
                removable,
                device_path: path,
            });
        }
        Ok(drives)
    }

    pub fn size(drive_id: &str) -> Result<u64> {
        let drives = list()?;
        drives
            .into_iter()
            .find(|d| d.id == drive_id)
            .map(|d| d.size_bytes)
            .ok_or_else(|| anyhow::anyhow!("drive {drive_id} not found"))
    }
}
