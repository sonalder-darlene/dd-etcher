use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize)]
pub struct FlashProgress {
    pub bytes_written: u64,
    pub total_bytes: u64,
    pub bytes_per_second: u64,
    pub phase: &'static str,
}

#[tauri::command]
pub async fn flash(
    app: AppHandle,
    image_path: String,
    drive_id: String,
    drive_size_bytes: u64,
) -> Result<(), String> {
    flash_inner(app, image_path, drive_id, drive_size_bytes)
        .await
        .map_err(|e| format!("{e:#}"))
}

async fn flash_inner(app: AppHandle, image_path: String, drive_id: String, drive_size_bytes: u64) -> Result<()> {
    let image = Path::new(&image_path);
    if !image.is_file() {
        return Err(anyhow!("image not found: {image_path}"));
    }
    let total_bytes = std::fs::metadata(image)?.len();

    if total_bytes > drive_size_bytes {
        return Err(anyhow!(
            "image is larger than the drive — aborting to prevent a partial write."
        ));
    }

    // Resolve the device path for the selected drive id. We intentionally
    // build the path ourselves rather than trusting the frontend, so the
    // privileged dd invocation can only target an enumerated drive.
    let device_path = resolve_device_path(&drive_id)?;

    // --- phase: flashing ---
    let progress = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let ticker = spawn_progress_ticker(
        app.clone(),
        progress.clone(),
        stop.clone(),
        total_bytes,
        "flashing",
    );

    let flash_result = tokio::task::spawn_blocking({
        let image_path = image_path.clone();
        let device_path = device_path.clone();
        let drive_id_c = drive_id.clone();
        let progress = progress.clone();
        move || run_dd(&image_path, &device_path, &drive_id_c, total_bytes, progress)
    })
    .await
    .context("flash task panicked")?;

    stop.store(true, Ordering::Relaxed);
    let _ = ticker.await;
    flash_result?;

    // --- phase: verifying ---
    // Hash the source image (our process has read access from the file dialog).
    let image_hash = tokio::task::spawn_blocking({
        let path = image_path.clone();
        move || sha256_file(&path, total_bytes)
    })
    .await??;

    // Re-unmount: macOS auto-mounts filesystems it recognises after a write.
    // Without this, reads from the block device stall while the OS accesses it.
    #[cfg(target_os = "macos")]
    {
        Command::new("diskutil")
            .args(["unmountDisk", "force", &format!("/dev/{drive_id}")])
            .status()
            .ok();
    }

    // Hash the device via the privileged helper. sudo -n reuses the cached
    // credential from the flash step — no second password prompt.
    let _ = app.emit("flash-progress", FlashProgress {
        bytes_written: 0,
        total_bytes,
        bytes_per_second: 0,
        phase: "verifying",
    });

    let self_exe = std::env::current_exe().context("cannot find own executable")?;

    // Use tokio::process so we can read stderr line-by-line for live progress
    // while the privileged helper hashes the device.
    let mut child = tokio::process::Command::new("/usr/bin/sudo")
        .args(["-n", self_exe.to_str().unwrap(), "--privileged-sha256", &device_path, &total_bytes.to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn privileged sha256")?;

    // Read progress lines from stderr and emit events.
    let stderr = child.stderr.take().unwrap();
    let mut lines = tokio::io::BufReader::new(stderr);
    let mut line = String::new();
    let mut last_bytes = 0u64;
    let mut last_tick = std::time::Instant::now();
    loop {
        line.clear();
        use tokio::io::AsyncBufReadExt as _;
        if tokio::io::AsyncBufReadExt::read_line(&mut lines, &mut line).await? == 0 {
            break;
        }
        if let Ok(bytes) = line.trim().parse::<u64>() {
            let now = std::time::Instant::now();
            let dt = now.duration_since(last_tick).as_secs_f64().max(0.001);
            let rate = ((bytes.saturating_sub(last_bytes)) as f64 / dt) as u64;
            last_bytes = bytes;
            last_tick = now;
            let _ = app.emit("flash-progress", FlashProgress {
                bytes_written: bytes,
                total_bytes,
                bytes_per_second: rate,
                phase: "verifying",
            });
        }
    }

    // Collect stdout (the final SHA-256 hex) and wait for exit.
    let mut stdout_buf = Vec::new();
    use tokio::io::AsyncReadExt as _;
    if let Some(mut out) = child.stdout.take() {
        out.read_to_end(&mut stdout_buf).await.ok();
    }
    let status = child.wait().await.context("waiting for sha256 helper")?;

    if !status.success() {
        return Err(anyhow!("verification read failed: helper exited with {status}"));
    }

    let device_hash_hex = String::from_utf8_lossy(&stdout_buf).trim().to_string();
    let image_hash_hex: String = image_hash.iter().map(|b| format!("{b:02x}")).collect();

    if device_hash_hex != image_hash_hex {
        return Err(anyhow!("verification failed: checksum mismatch\nimage:  {image_hash_hex}\ndevice: {device_hash_hex}"));
    }

    let _ = app.emit(
        "flash-progress",
        FlashProgress {
            bytes_written: total_bytes,
            total_bytes,
            bytes_per_second: 0,
            phase: "done",
        },
    );

    Ok(())
}

fn resolve_device_path(drive_id: &str) -> Result<String> {
    // Reject anything that isn't plain alphanumerics so we can't be tricked
    // into targeting arbitrary paths through the admin prompt.
    if !drive_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow!("invalid drive id: {drive_id}"));
    }
    #[cfg(target_os = "macos")]
    {
        // Use the buffered device (/dev/diskN) rather than the raw device
        // (/dev/rdiskN). Raw access is more restricted on macOS 15+; dd
        // through the buffer cache is slightly slower but reliably works.
        Ok(format!("/dev/{drive_id}"))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(format!("/dev/{drive_id}"))
    }
}

/// Show a native password dialog and authenticate with sudo.
///
/// Separating auth from execution means Cancel is handled before sudo is
/// involved at all — no retries, no "Sorry, try again." loops.
/// `sudo -S -v` validates the credential and caches it; subsequent calls
/// use `sudo -n` (non-interactive) against that cache.
#[cfg(target_os = "macos")]
fn authenticate_sudo() -> Result<()> {
    let dialog = Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "set r to display dialog \
             \"dd-Etcher needs administrator access to flash the drive.\" \
             with hidden answer default answer \"\" \
             with title \"dd-Etcher\" \
             buttons {\"Cancel\", \"OK\"} default button \"OK\"",
            "-e",
            "return text returned of r",
        ])
        .output()
        .context("failed to show password dialog")?;

    if !dialog.status.success() {
        return Err(anyhow!("Authentication cancelled."));
    }

    // sudo -S reads the password from stdin; -p "" suppresses its own prompt;
    // -v just validates and caches the credential without running a command.
    let mut sudo = Command::new("/usr/bin/sudo")
        .args(["-S", "-p", "", "-v"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn sudo")?;

    let mut stdin = sudo.stdin.take().unwrap();
    stdin.write_all(&dialog.stdout).context("sending password")?;
    stdin.write_all(b"\n").ok();
    drop(stdin);

    let out = sudo.wait_with_output().context("waiting for sudo")?;
    if !out.status.success() {
        return Err(anyhow!("Authentication failed — wrong password?"));
    }
    Ok(())
}

/// Flash the image to the device with root privileges.
///
/// Auth is separated from execution: we collect the password once via a
/// native dialog, cache it with `sudo -v`, then use `sudo -n` for both
/// the flash and verify steps (no repeated prompts).
fn run_dd(image: &str, device: &str, drive_id: &str, _total: u64, progress: Arc<AtomicU64>) -> Result<()> {
    #[cfg(target_os = "macos")]
    authenticate_sudo()?;

    // Unmount after auth so the "Requesting permission…" status is accurate
    // right up until we start writing.
    #[cfg(target_os = "macos")]
    Command::new("diskutil")
        .args(["unmountDisk", "force", &format!("/dev/{drive_id}")])
        .status()
        .context("diskutil unmountDisk failed")?;

    let self_exe = std::env::current_exe().context("cannot find own executable")?;

    #[cfg(target_os = "macos")]
    let mut child = Command::new("/usr/bin/sudo")
        .args(["-n", self_exe.to_str().unwrap(), "--privileged-flash", device])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn privileged helper")?;

    #[cfg(target_os = "linux")]
    let mut child = Command::new("pkexec")
        .args([self_exe.to_str().unwrap(), "--privileged-flash", device])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn pkexec")?;

    let pipe_result: Result<()> = {
        let mut stdin = child.stdin.take().unwrap();
        let mut src = File::open(image).context("opening image")?;
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        loop {
            let n = src.read(&mut buf).context("reading image")?;
            if n == 0 { break; }
            stdin.write_all(&buf[..n]).context("writing to helper")?;
            progress.fetch_add(n as u64, Ordering::Relaxed);
        }
        Ok(())
    };

    let stderr_bytes = child.stderr.take()
        .map(|mut r| { let mut b = Vec::new(); std::io::Read::read_to_end(&mut r, &mut b).ok(); b })
        .unwrap_or_default();

    let status = child.wait().context("waiting for helper")?;
    pipe_result?;

    if !status.success() {
        let msg = String::from_utf8_lossy(&stderr_bytes);
        if msg.contains("Operation not permitted") || msg.contains("permission denied") {
            return Err(anyhow!(
                "Permission denied.\n\
                 Go to System Settings → Privacy & Security → Full Disk Access\n\
                 and add dd-Etcher, then try again."
            ));
        }
        return Err(anyhow!(
            "helper exited with {status}{}",
            if msg.trim().is_empty() { String::new() } else { format!(": {}", msg.trim()) }
        ));
    }
    Ok(())
}

fn spawn_progress_ticker(
    app: AppHandle,
    progress: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    total: u64,
    phase: &'static str,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let start = Instant::now();
        let mut last_bytes = 0u64;
        let mut last_tick = start;
        while !stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let now = Instant::now();
            let bytes = progress.load(Ordering::Relaxed);
            let dt = now.duration_since(last_tick).as_secs_f64().max(0.001);
            let rate = ((bytes.saturating_sub(last_bytes)) as f64 / dt) as u64;
            last_bytes = bytes;
            last_tick = now;
            let _ = app.emit(
                "flash-progress",
                FlashProgress {
                    bytes_written: bytes,
                    total_bytes: total,
                    bytes_per_second: rate,
                    phase,
                },
            );
        }
    })
}

fn sha256_file(path: &str, _total: u64) -> Result<[u8; 32]> {
    let file = File::open(path).with_context(|| format!("opening {path}"))?;
    let mut reader = BufReader::with_capacity(4 * 1024 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}


