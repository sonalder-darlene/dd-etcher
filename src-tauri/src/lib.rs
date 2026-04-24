mod drives;
mod flash;

/// Privileged helper: hash the first `limit` bytes of a device, print hex SHA-256 to stdout.
/// Called via `sudo -n` after flashing — reuses the cached sudo credential.
pub fn privileged_sha256(device: &str, limit: u64) {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    if !is_safe_device(device) {
        eprintln!("dd-etcher: rejected unsafe device path: {device}");
        std::process::exit(1);
    }

    let mut file = match std::fs::File::open(device) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("dd-etcher: cannot open {device}: {e}");
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
                eprintln!("dd-etcher: read error: {e}");
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
        eprintln!("dd-etcher: rejected unsafe device path: {device}");
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
            eprintln!("dd-etcher: failed to run dd: {e}");
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

use tauri::Manager as _;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            drives::list_drives,
            flash::flash,
        ])
        .setup(|app| {
            #[cfg(debug_assertions)]
            app.get_webview_window("main").unwrap().open_devtools();
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running dd-etcher");
}
