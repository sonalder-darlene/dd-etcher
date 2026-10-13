mod drives;
mod flash;
mod image;

/// Linux privileged helper, run through pkexec: open the device and hand the
/// descriptor back over stdout, which the parent made a Unix socket. Writing,
/// verifying and wiping all happen back in the unprivileged app. macOS needs
/// no helper of ours — authopen(1) does this job there.
#[cfg(target_os = "linux")]
pub fn privileged_open(device: &str) {
    use std::os::fd::AsRawFd;

    if !is_safe_device(device) {
        eprintln!("dd-Etcher: rejected unsafe device path: {device}");
        std::process::exit(1);
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("dd-Etcher: cannot open {device}: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = flash::send_fd(1, file.as_raw_fd()) {
        eprintln!("dd-Etcher: cannot pass {device} back: {e}");
        std::process::exit(1);
    }
}

fn is_safe_device(device: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        device
            .strip_prefix("/dev/disk")
            .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
    }
    #[cfg(target_os = "linux")]
    {
        let name = device.strip_prefix("/dev/").unwrap_or("");
        !name.is_empty()
            && (name.starts_with("sd")
                || name.starts_with("nvme")
                || name.starts_with("mmcblk")
                || name.starts_with("vd"))
            && name.chars().all(|c| c.is_ascii_alphanumeric())
    }
}

#[tauri::command]
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The number of bytes this image will occupy once written — for a `.xz` that
/// is the uncompressed size, not the file size.
#[tauri::command]
fn get_file_size(path: String) -> Result<u64, String> {
    image::image_size(&path).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn open_repo() {
    let url = env!("CARGO_PKG_REPOSITORY");
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(flash::FlashState::default())
        .invoke_handler(tauri::generate_handler![
            drives::list_drives,
            flash::flash,
            flash::cancel_flash,
            flash::eject_drive,
            app_version,
            open_repo,
            get_file_size,
        ])
        .setup(|_app| {
            #[cfg(debug_assertions)]
            {
                use tauri::Manager as _;
                if let Some(w) = _app.get_webview_window("main") {
                    w.open_devtools();
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running dd-Etcher");
}

#[cfg(test)]
mod tests {
    use super::is_safe_device;

    /// The allowlist checked before a drive is opened for writing. A false
    /// positive here writes to the wrong device, so the interesting cases are
    /// the near misses, not the happy path.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_accepts_only_whole_disks() {
        for ok in ["/dev/disk0", "/dev/disk2", "/dev/disk10"] {
            assert!(is_safe_device(ok), "should accept {ok}");
        }
        for bad in [
            "",
            "/dev/disk",          // no number
            "/dev/disk2s1",       // a partition, not the whole disk
            "/dev/rdisk2",        // raw node — derived only after this check
            "/dev/disk2 ",        // trailing space
            "/dev/disk2;reboot",  // shell metacharacters
            "/dev/../etc/passwd", // traversal
            "/dev/sda",           // Linux name on macOS
            "disk2",              // not absolute
        ] {
            assert!(!is_safe_device(bad), "should reject {bad:?}");
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_accepts_known_block_device_names() {
        for ok in [
            "/dev/sda",
            "/dev/sdb1",
            "/dev/nvme0n1",
            "/dev/mmcblk0",
            "/dev/vda",
        ] {
            assert!(is_safe_device(ok), "should accept {ok}");
        }
        for bad in [
            "",
            "/dev/",
            "/dev/loop0", // not a removable target
            "/dev/sda;reboot",
            "/dev/../etc/passwd",
            "/dev/disk2", // macOS name on Linux
            "sda",
        ] {
            assert!(!is_safe_device(bad), "should reject {bad:?}");
        }
    }
}
