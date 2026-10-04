use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use zeroize::Zeroizing;

pub struct FlashState {
    pub cancel: Arc<AtomicBool>,
    /// Set while a flash is running. A plain flag rather than a Mutex: we only
    /// need to reject a second caller, not queue it, and there is no guard
    /// lifetime to thread through the async body. Release builds set
    /// `panic = "abort"`, so a panic cannot leave this stuck true.
    pub busy: AtomicBool,
}

impl Default for FlashState {
    fn default() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            busy: AtomicBool::new(false),
        }
    }
}

#[tauri::command]
pub fn cancel_flash(state: tauri::State<'_, FlashState>) {
    state.cancel.store(true, Ordering::Relaxed);
}

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
    state: tauri::State<'_, FlashState>,
) -> Result<(), String> {
    // Reject a second concurrent flash. The frontend disables the button, but
    // any IPC caller can reach this command directly; two `dd` writes to one
    // device produce garbage.
    if state.busy.swap(true, Ordering::SeqCst) {
        return Err("a flash is already in progress".into());
    }

    let cancel = state.cancel.clone();
    let result = flash_inner(app, image_path, drive_id, cancel.clone()).await;
    // Stop the background source hash if the run ended before it was needed.
    cancel.store(true, Ordering::Relaxed);

    // Drop the cached sudo credential rather than leaving it valid for the
    // default five minutes after we are done with it.
    #[cfg(target_os = "macos")]
    let _ = Command::new("/usr/bin/sudo").arg("-k").status();

    state.busy.store(false, Ordering::SeqCst);
    result.map_err(|e| format!("{e:#}"))
}

async fn flash_inner(
    app: AppHandle,
    image_path: String,
    drive_id: String,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    cancel.store(false, Ordering::Relaxed);
    let image = Path::new(&image_path);
    if !image.is_file() {
        return Err(anyhow!("image not found: {image_path}"));
    }
    // The bytes that will land on the device — decompressed, for a .xz.
    let total_bytes = crate::image::image_size(&image_path)?;

    // Re-query drive size from the OS — do not trust the frontend-supplied value.
    let drive_size_bytes =
        crate::drives::query_drive_size(&drive_id).context("could not determine drive size")?;
    if total_bytes > drive_size_bytes {
        return Err(anyhow!(
            "image is larger than the drive — aborting to prevent a partial write."
        ));
    }

    // Resolve the device path for the selected drive id. We intentionally
    // build the path ourselves rather than trusting the frontend, so the
    // privileged dd invocation can only target an enumerated drive.
    let device_path = resolve_device_path(&drive_id)?;

    // Hash the source alongside the write rather than after it: it reads from
    // the local disk and is normally done long before the USB write is.
    let hashed = Arc::new(AtomicU64::new(0));
    let hash_task = tokio::task::spawn_blocking({
        let path = image_path.clone();
        let hashed = hashed.clone();
        let cancel = cancel.clone();
        move || sha256_file(&path, hashed, cancel)
    });

    // --- phase: flashing ---
    let progress = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let ticker = spawn_progress_ticker(
        app.clone(),
        progress.clone(),
        stop.clone(),
        cancel.clone(),
        total_bytes,
        "flashing",
    );

    let flash_result = tokio::task::spawn_blocking({
        let image_path = image_path.clone();
        let device_path = device_path.clone();
        let drive_id_c = drive_id.clone();
        let progress = progress.clone();
        let cancel = cancel.clone();
        move || run_dd(&image_path, &device_path, &drive_id_c, progress, cancel)
    })
    .await
    .context("flash task panicked")?;

    stop.store(true, Ordering::Relaxed);
    let _ = ticker.await;

    // Any abort after bytes have landed leaves a drive that still mounts and
    // still looks bootable. Blank it before handing control back, so the user
    // ends up with an empty drive rather than a convincing broken one. Covers
    // cancel, the stall watchdog (which sets the same flag), and a dd failure.
    let aborted = cancel.load(Ordering::Relaxed);
    if (aborted || flash_result.is_err()) && progress.load(Ordering::Relaxed) > 0 {
        let _ = app.emit(
            "flash-progress",
            FlashProgress {
                bytes_written: 0,
                total_bytes,
                bytes_per_second: 0,
                phase: "wiping",
            },
        );
        let wiped = tokio::task::spawn_blocking({
            let device_path = device_path.clone();
            move || wipe_device(&device_path)
        })
        .await
        .context("wipe task panicked")?;

        let reason = if aborted {
            "cancelled".to_string()
        } else {
            format!("{:#}", flash_result.unwrap_err())
        };
        return Err(match wiped {
            Ok(()) => anyhow!("{reason}"),
            // Never let a failed wipe pass as a clean abort — the drive really
            // does hold a partial image at this point and the user must know.
            Err(e) => anyhow!("wipe failed after {reason}: {e:#}"),
        });
    }

    if aborted {
        return Err(anyhow!("cancelled"));
    }
    flash_result?;

    // --- phase: hashing ---
    // Only shown if the source hash is still running — a big .xz on a fast
    // stick, say. The ticker's first event comes after 500 ms, so a hash that
    // is nearly done never flashes the phase on screen.
    let hash_stop = Arc::new(AtomicBool::new(false));
    let hash_ticker = (!hash_task.is_finished()).then(|| {
        spawn_progress_ticker(
            app.clone(),
            hashed.clone(),
            hash_stop.clone(),
            cancel.clone(),
            total_bytes,
            "hashing",
        )
    });
    let image_hash = hash_task.await.context("hash task panicked");
    hash_stop.store(true, Ordering::Relaxed);
    if let Some(t) = hash_ticker {
        let _ = t.await;
    }
    let image_hash = image_hash??;

    // --- phase: verifying ---

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
    let _ = app.emit(
        "flash-progress",
        FlashProgress {
            bytes_written: 0,
            total_bytes,
            bytes_per_second: 0,
            phase: "verifying",
        },
    );

    let self_exe = std::env::current_exe().context("cannot find own executable")?;

    // Use tokio::process so we can read stderr line-by-line for live progress
    // while the privileged helper hashes the device.
    let mut child = tokio::process::Command::new("/usr/bin/sudo")
        .args([
            "-n",
            self_exe.to_string_lossy().as_ref(),
            "--privileged-sha256",
            &device_path,
            &total_bytes.to_string(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn privileged sha256")?;

    // Read progress lines from stderr and emit events.
    let stderr = child.stderr.take().unwrap();
    use tokio::io::AsyncBufReadExt as _;
    let mut lines = tokio::io::BufReader::new(stderr);
    let mut line = String::new();
    let mut last_bytes = 0u64;
    let mut last_tick = std::time::Instant::now();
    // Poll in one-second slices rather than blocking on read_line: a drive that
    // dies after the write would otherwise hang here forever, and a cancel
    // would go unnoticed. Matches the write phase's 120s stall budget.
    let mut stalled = Duration::ZERO;
    loop {
        line.clear();
        match tokio::time::timeout(Duration::from_secs(1), lines.read_line(&mut line)).await {
            Err(_elapsed) => {
                if cancel.load(Ordering::Relaxed) {
                    let _ = child.kill().await;
                    return Err(anyhow!("cancelled after write"));
                }
                stalled += Duration::from_secs(1);
                if stalled >= Duration::from_secs(120) {
                    let _ = child.kill().await;
                    let _ = app.emit("flash-stalled", ());
                    return Err(anyhow!(
                        "the drive stopped responding during verification — the write itself completed"
                    ));
                }
                continue;
            }
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => stalled = Duration::ZERO,
            Ok(Err(e)) => return Err(e).context("reading verification progress"),
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill().await;
            return Err(anyhow!("cancelled after write"));
        }
        if let Ok(bytes) = line.trim().parse::<u64>() {
            let now = std::time::Instant::now();
            let dt = now.duration_since(last_tick).as_secs_f64().max(0.001);
            let rate = ((bytes.saturating_sub(last_bytes)) as f64 / dt) as u64;
            last_bytes = bytes;
            last_tick = now;
            let _ = app.emit(
                "flash-progress",
                FlashProgress {
                    bytes_written: bytes,
                    total_bytes,
                    bytes_per_second: rate,
                    phase: "verifying",
                },
            );
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
        return Err(anyhow!(
            "verification read failed: helper exited with {status}"
        ));
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

    notify_complete();
    Ok(())
}

fn notify_complete() {
    #[cfg(target_os = "macos")]
    let _ = Command::new("osascript")
        .args(["-e", "display notification \"Your drive is ready to use.\" with title \"dd-Etcher\" subtitle \"Flash complete ✓\""])
        .spawn();
    #[cfg(target_os = "linux")]
    let _ = Command::new("notify-send")
        .args(["dd-Etcher", "Flash complete — your drive is ready."])
        .spawn();
}

/// Eject a drive that now holds a finished image. Without this macOS mounts
/// the new partitions and then warns "Disk Not Ejected Properly" when the
/// stick is pulled.
#[tauri::command]
pub async fn eject_drive(drive_id: String) -> Result<(), String> {
    let device = resolve_device_path(&drive_id).map_err(|e| format!("{e:#}"))?;
    if !crate::is_safe_device(&device) {
        return Err(format!("refusing to eject {device}"));
    }
    #[cfg(target_os = "macos")]
    let status = Command::new("diskutil").args(["eject", &device]).status();
    #[cfg(target_os = "linux")]
    let status = Command::new("udisksctl")
        .args(["power-off", "-b", &device])
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("eject exited with {s}")),
        Err(e) => Err(format!("could not run eject: {e}")),
    }
}

fn resolve_device_path(drive_id: &str) -> Result<String> {
    // Reject anything that isn't plain alphanumerics so we can't be tricked
    // into targeting arbitrary paths through the admin prompt.
    // `all` is vacuously true on an empty string, which would build "/dev/".
    if drive_id.is_empty()
        || !drive_id
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
             \"Enter your password to let dd-Etcher write to the drive.\" \
             & return & return & \
             \"macOS only lets an administrator write to a whole disk. \
             The password is used for this flash and its check, then forgotten.\" \
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

    // Wrap the password in Zeroizing so it is wiped from memory on drop.
    let password = Zeroizing::new(dialog.stdout);

    // sudo -S reads the password from stdin; -p "" suppresses its own prompt;
    // -v just validates and caches the credential without running a command.
    let mut sudo = Command::new("/usr/bin/sudo")
        .args(["-S", "-p", "", "-v"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn sudo")?;

    let mut stdin = sudo.stdin.take().unwrap();
    stdin.write_all(&password).context("sending password")?;
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
fn run_dd(
    image: &str,
    device: &str,
    drive_id: &str,
    progress: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
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
        .args([
            "-n",
            self_exe.to_string_lossy().as_ref(),
            "--privileged-flash",
            device,
        ])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn privileged helper")?;

    #[cfg(target_os = "linux")]
    let mut child = Command::new("pkexec")
        .args([
            self_exe.to_string_lossy().as_ref(),
            "--privileged-flash",
            device,
        ])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn pkexec")?;

    let pipe_result: Result<()> = {
        let mut stdin = child.stdin.take().unwrap();
        let mut src = crate::image::open_image(image)?;
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let n = src.read(&mut buf).context("reading image")?;
            if n == 0 {
                break;
            }
            stdin.write_all(&buf[..n]).context("writing to helper")?;
            progress.fetch_add(n as u64, Ordering::Relaxed);
        }
        Ok(())
    };

    if cancel.load(Ordering::Relaxed) {
        let _ = child.kill();
    }

    let stderr_bytes = child
        .stderr
        .take()
        .map(|mut r| {
            let mut b = Vec::new();
            std::io::Read::read_to_end(&mut r, &mut b).ok();
            b
        })
        .unwrap_or_default();

    let status = child.wait().context("waiting for helper")?;
    pipe_result?;

    if cancel.load(Ordering::Relaxed) {
        return Err(anyhow!("cancelled"));
    }

    if !status.success() {
        let msg = String::from_utf8_lossy(&stderr_bytes);
        let msg_lower = msg.to_lowercase();
        if msg_lower.contains("operation not permitted") || msg_lower.contains("permission denied")
        {
            return Err(anyhow!(
                "Permission denied.\n\
                 Go to System Settings → Privacy & Security → Full Disk Access\n\
                 and add dd-Etcher, then try again."
            ));
        }
        return Err(anyhow!(
            "helper exited with {status}{}",
            if msg.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", msg.trim())
            }
        ));
    }
    Ok(())
}

/// Ask the privileged helper to blank a partially written device. Reuses the
/// sudo credential cached by the flash step, so there is no second prompt.
fn wipe_device(device_path: &str) -> Result<()> {
    let self_exe = std::env::current_exe().context("cannot find own executable")?;

    #[cfg(target_os = "macos")]
    let output = Command::new("/usr/bin/sudo")
        .args([
            "-n",
            self_exe.to_string_lossy().as_ref(),
            "--privileged-wipe",
            device_path,
        ])
        .output();

    #[cfg(target_os = "linux")]
    let output = Command::new("pkexec")
        .args([
            self_exe.to_string_lossy().as_ref(),
            "--privileged-wipe",
            device_path,
        ])
        .output();

    let output = output.context("failed to spawn the wipe helper")?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "{}",
            if msg.trim().is_empty() {
                format!("helper exited with {}", output.status)
            } else {
                msg.trim().to_string()
            }
        ));
    }
    Ok(())
}

fn spawn_progress_ticker(
    app: AppHandle,
    progress: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    total: u64,
    phase: &'static str,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let start = Instant::now();
        let mut last_bytes = 0u64;
        let mut last_tick = start;
        let mut stall_since: Option<Instant> = None;
        while !stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let now = Instant::now();
            let bytes = progress.load(Ordering::Relaxed);
            let dt = now.duration_since(last_tick).as_secs_f64().max(0.001);
            let rate = ((bytes.saturating_sub(last_bytes)) as f64 / dt) as u64;

            // Stall detection: if bytes have started but not moved for 120s, auto-cancel.
            if bytes > 0 && bytes == last_bytes {
                match stall_since {
                    None => stall_since = Some(now),
                    Some(t) if now.duration_since(t).as_secs() >= 120 => {
                        cancel.store(true, Ordering::Relaxed);
                        let _ = app.emit("flash-stalled", ());
                        break;
                    }
                    _ => {}
                }
            } else {
                stall_since = None;
            }

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

fn sha256_file(path: &str, hashed: Arc<AtomicU64>, cancel: Arc<AtomicBool>) -> Result<[u8; 32]> {
    // Hash the decompressed stream — those are the bytes on the device.
    let mut reader = crate::image::open_image(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("cancelled after write"));
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        hashed.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::resolve_device_path;

    /// The frontend supplies a drive id, never a path. This is the charset
    /// gate that stands between an IPC caller and an arbitrary target; the
    /// shape check in `is_safe_device` is the second, independent gate.
    #[test]
    fn rejects_anything_that_is_not_a_bare_id() {
        for bad in [
            "",
            "..",
            "../../dev/disk0",
            "disk2 disk3",
            "disk2;reboot",
            "disk2/../disk0",
            "disk2\n",
            "/dev/disk2", // already a path — ids are bare
        ] {
            assert!(resolve_device_path(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn builds_the_path_itself() {
        assert_eq!(resolve_device_path("disk2").unwrap(), "/dev/disk2");
        assert_eq!(resolve_device_path("nvme0n1").unwrap(), "/dev/nvme0n1");
    }

    /// An empty id passes nothing useful downstream, and a partition id is
    /// charset-clean here but caught by `is_safe_device` in the helper. This
    /// documents that the two checks are deliberately not the same check.
    #[test]
    fn charset_gate_is_not_the_shape_gate() {
        assert_eq!(resolve_device_path("disk2s1").unwrap(), "/dev/disk2s1");
        assert!(!crate::is_safe_device("/dev/disk2s1"));
    }
}
