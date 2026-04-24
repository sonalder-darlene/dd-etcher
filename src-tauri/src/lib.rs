mod drives;
mod flash;

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
    println!("{}", hash.iter().map(|b| format!("{b:02x}")).collect::<String>());
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

fn is_safe_device(device: &str) -> bool {
    if let Some(rest) = device.strip_prefix("/dev/disk") {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
    } else {
        false
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

#[tauri::command]
fn get_file_size(path: String) -> Result<u64, String> {
    std::fs::metadata(&path)
        .map(|m| m.len())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn open_repo() {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("https://github.com/sonalder-darlene/dd-Etcher")
        .spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open")
        .arg("https://github.com/sonalder-darlene/dd-Etcher")
        .spawn();
}

use tauri::Manager as _;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            drives::list_drives,
            flash::flash,
            check_fda,
            open_fda_settings,
            app_version,
            open_repo,
            get_file_size,
        ])
        .setup(|app| {
            #[cfg(debug_assertions)]
            app.get_webview_window("main").unwrap().open_devtools();
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running dd-Etcher");
}
