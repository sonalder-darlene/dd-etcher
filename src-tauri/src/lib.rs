mod drives;
mod flash;
mod image;

/// Privileged helper: hash the first `limit` bytes of a device, print hex SHA-256 to stdout.
/// Called via `sudo -n` after flashing — reuses the cached sudo credential.
pub fn privileged_sha256(device: &str, limit: u64) {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    if !is_safe_device(device) {
        eprintln!("dd-Etcher: rejected unsafe device path: {device}");
        std::process::exit(1);
    }

    let mut file = match std::fs::File::open(device) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("dd-Etcher: cannot open {device}: {e}");
            std::process::exit(1);
        }
    };

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut remaining = limit;
    while remaining > 0 {
        let to_read = buf.len().min(remaining as usize);
        match file.read(&mut buf[..to_read]) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                remaining -= n as u64;
                // Report bytes read so far to the parent process via stderr.
                eprintln!("{}", limit - remaining);
            }
            Err(e) => {
                eprintln!("dd-Etcher: read error: {e}");
                std::process::exit(1);
            }
        }
    }

    let hash = hasher.finalize();
    println!(
        "{}",
        hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
}

/// Privileged helper entry point — called by main() when invoked via sudo.
/// Pipes stdin directly into dd, which writes to the block device.
/// stdin is the image data streamed from the parent process via the sudo pipe.
/// Only accepts /dev/diskN paths to prevent misuse.
pub fn privileged_flash(device: &str) {
    use std::process::Command;

    if !is_safe_device(device) {
        eprintln!("dd-Etcher: rejected unsafe device path: {device}");
        std::process::exit(1);
    }

    // dd reads from stdin (inherited from our parent's pipe) and writes to
    // the device. bs=4m on macOS BSD dd, bs=4M + conv=fsync on GNU dd.
    #[cfg(target_os = "macos")]
    let result = Command::new("/bin/dd")
        .args([&format!("of={device}"), "bs=4m"])
        .status();

    #[cfg(target_os = "linux")]
    let result = Command::new("/bin/dd")
        .args([&format!("of={device}"), "bs=4M", "conv=fsync"])
        .status();

    match result {
        Ok(s) if s.success() => {}
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("dd-Etcher: failed to run dd: {e}");
            std::process::exit(1);
        }
    }
}

/// Privileged helper: make a partially written device unambiguously blank.
///
/// A cancelled or failed write is the dangerous case, not a harmless one. An
/// image lays down its partition table and bootloader first, so a drive
/// abandoned at 60% still mounts, still looks bootable, and will still start an
/// installer that then fails partway through repartitioning the user's real
/// disk. Zeroing the head takes out the MBR, the primary GPT, and the common
/// filesystem superblocks; zeroing the tail takes out the backup GPT that would
/// otherwise let the OS reconstruct the partition table. What is left is a blank
/// drive the OS offers to initialise — which is what "cancel" should produce.
pub fn privileged_wipe(device: &str) {
    use std::fs::OpenOptions;
    use std::io::{Seek, SeekFrom, Write};

    const HEAD: u64 = 16 * 1024 * 1024;
    const TAIL: u64 = 1024 * 1024;
    const CHUNK: usize = 1024 * 1024;

    if !is_safe_device(device) {
        eprintln!("dd-Etcher: rejected unsafe device path: {device}");
        std::process::exit(1);
    }

    let mut file = match OpenOptions::new().write(true).open(device) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("dd-Etcher: cannot open {device} for wiping: {e}");
            std::process::exit(1);
        }
    };

    // Seeking to the end is how we learn the device size; there is no metadata
    // len for a block device.
    let size = match file.seek(SeekFrom::End(0)) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("dd-Etcher: cannot size {device}: {e}");
            std::process::exit(1);
        }
    };

    // Block devices demand sector-aligned writes; 1 MiB chunks are aligned for
    // every sector size in practice.
    let zeros = vec![0u8; CHUNK];
    let mut zero_range = |start: u64, len: u64| -> std::io::Result<()> {
        file.seek(SeekFrom::Start(start))?;
        let mut left = len;
        while left > 0 {
            let n = (left as usize).min(CHUNK);
            file.write_all(&zeros[..n])?;
            left -= n as u64;
        }
        Ok(())
    };

    let head = HEAD.min(size);
    if let Err(e) = zero_range(0, head) {
        eprintln!("dd-Etcher: wiping the start of {device} failed: {e}");
        std::process::exit(1);
    }
    if size > HEAD + TAIL {
        if let Err(e) = zero_range(size - TAIL, TAIL) {
            eprintln!("dd-Etcher: wiping the end of {device} failed: {e}");
            std::process::exit(1);
        }
    }
    if let Err(e) = file.sync_all() {
        eprintln!("dd-Etcher: flushing {device} failed: {e}");
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

/// Check whether the app has Full Disk Access.
///
/// In debug builds the binary is a child of the dev server (node/pnpm), so
/// macOS TCC doesn't recognise it as the responsible app and the file-access
/// probe always returns EPERM regardless of FDA status. Skip the check there;
/// it works correctly in the release .app bundle where Finder is the launcher.
#[tauri::command]
fn check_fda() -> bool {
    #[cfg(debug_assertions)]
    {
        true
    }
    #[cfg(all(not(debug_assertions), target_os = "macos"))]
    {
        // The system TCC database is gated behind FDA for every process.
        // EPERM = TCC is blocking (no FDA). Any other result = FDA granted.
        let path = "/Library/Application Support/com.apple.TCC/TCC.db";
        match std::fs::File::open(path) {
            Ok(_) => true,
            Err(e) => e.raw_os_error() != Some(libc::EPERM),
        }
    }
    #[cfg(all(not(debug_assertions), not(target_os = "macos")))]
    {
        true
    }
}

/// Open System Settings directly to the Full Disk Access page.
#[tauri::command]
fn open_fda_settings() {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
        .spawn();
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

use tauri::Manager as _;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(flash::FlashState::default())
        .invoke_handler(tauri::generate_handler![
            drives::list_drives,
            flash::flash,
            flash::cancel_flash,
            check_fda,
            open_fda_settings,
            app_version,
            open_repo,
            get_file_size,
        ])
        .setup(|app| {
            #[cfg(debug_assertions)]
            if let Some(w) = app.get_webview_window("main") {
                w.open_devtools();
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running dd-Etcher");
}

#[cfg(test)]
mod tests {
    use super::is_safe_device;

    /// The allowlist guarding the privileged helper. A false positive here
    /// writes to the wrong device, so the interesting cases are the near
    /// misses, not the happy path.
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
            "/dev/rdisk2",        // raw device — we never write to it
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
