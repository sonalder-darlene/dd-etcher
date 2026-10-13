use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

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
    // drive we ask permission for can only be an enumerated one.
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

    // The only privileged step: macOS asks for permission and hands back an
    // open descriptor. The write, the read-back and any wipe all go through it.
    let dev = tokio::task::spawn_blocking({
        let device_path = device_path.clone();
        move || open_device(&device_path)
    })
    .await
    .context("open task panicked")??;
    let dev = Arc::new(dev);

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
        let dev = dev.clone();
        let progress = progress.clone();
        let cancel = cancel.clone();
        move || run_dd(&image_path, &dev, progress, cancel)
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
            let dev = dev.clone();
            move || wipe(&dev, drive_size_bytes)
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
    // Read back through the descriptor we wrote with. On macOS it is the raw
    // device, so nothing comes from a cache; and while we hold it open macOS
    // does not re-read the new partition table, so nothing gets mounted under
    // the check.
    let _ = app.emit(
        "flash-progress",
        FlashProgress {
            bytes_written: 0,
            total_bytes,
            bytes_per_second: 0,
            phase: "verifying",
        },
    );
    let verified = Arc::new(AtomicU64::new(0));
    let verify_stop = Arc::new(AtomicBool::new(false));
    let mut verify_ticker = spawn_progress_ticker(
        app.clone(),
        verified.clone(),
        verify_stop.clone(),
        cancel.clone(),
        total_bytes,
        "verifying",
    );
    let read_back = tokio::task::spawn_blocking({
        let dev = dev.clone();
        let verified = verified.clone();
        let cancel = cancel.clone();
        move || hash_device(&dev, total_bytes, verified, cancel)
    });
    let device_hash = tokio::select! {
        biased;
        // The ticker only ends on its own after a two-minute stall. Do not wait
        // on a read that a dying drive may never return.
        _ = &mut verify_ticker => {
            return Err(anyhow!(
                "the drive stopped responding during verification — the write itself completed"
            ));
        }
        r = read_back => r.context("verify task panicked")?,
    };
    verify_stop.store(true, Ordering::Relaxed);
    let _ = verify_ticker.await;
    let device_hash = device_hash?;

    if device_hash != image_hash {
        let hex = |h: &[u8; 32]| h.iter().map(|b| format!("{b:02x}")).collect::<String>();
        return Err(anyhow!(
            "verification failed: checksum mismatch\nimage:  {}\ndevice: {}",
            hex(&image_hash),
            hex(&device_hash)
        ));
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
    Ok(format!("/dev/{drive_id}"))
}

/// Open the whole drive for reading and writing — the one privileged step.
///
/// authopen(1) is Apple's tool for exactly this. We obtain its
/// `sys.openfile` right ourselves first, so the password / Touch ID sheet
/// names dd-Etcher rather than authopen; it then opens the device as root and
/// passes the descriptor back. authopen checks the Removable Volumes privacy
/// permission against this app instead of itself, so the first flash shows a
/// one-click "access files on a removable volume" prompt. Nothing of ours runs
/// as root, and Full Disk Access never comes into it.
#[cfg(target_os = "macos")]
fn open_device(device: &str) -> Result<File> {
    if !crate::is_safe_device(device) {
        return Err(anyhow!("refusing to open {device}"));
    }
    // The raw node bypasses the buffer cache: much faster, and the read-back
    // checks the drive rather than our own cached writes.
    let raw = device.replacen("/dev/disk", "/dev/rdisk", 1);
    let auth = authorization::Authorization::obtain(&format!("sys.openfile.readwrite.{raw}"))?;

    // Unmount after auth so "Waiting for your permission" is accurate right up
    // until we start writing.
    Command::new("diskutil")
        .args(["unmountDisk", "force", device])
        .status()
        .context("diskutil unmountDisk failed")?;

    let mut authopen = Command::new("/usr/libexec/authopen");
    authopen.args([
        "-stdoutpipe",
        "-extauth",
        "-o",
        &libc::O_RDWR.to_string(),
        &raw,
    ]);
    receive_fd(authopen, &auth.external_form()?).map_err(|e| {
        if format!("{e:#}").to_lowercase().contains("not permitted") {
            anyhow!(
                "macOS blocked access to the drive.\n\
                 Turn on Removable Volumes for dd-Etcher in System Settings → \
                 Privacy & Security → Files & Folders, then try again."
            )
        } else {
            e
        }
    })
}

/// Linux has no authopen: pkexec runs our own binary just long enough to open
/// the device and pass the descriptor back (`privileged_open` in lib.rs).
#[cfg(target_os = "linux")]
fn open_device(device: &str) -> Result<File> {
    if !crate::is_safe_device(device) {
        return Err(anyhow!("refusing to open {device}"));
    }
    let exe = std::env::current_exe().context("cannot find own executable")?;
    let mut pkexec = Command::new("pkexec");
    pkexec.arg(exe).args(["--privileged-open", device]);
    receive_fd(pkexec, &[])
}

/// Run a device opener with a Unix socket as its stdout, feed it `input`, and
/// take the file descriptor it sends back.
fn receive_fd(mut cmd: Command, input: &[u8]) -> Result<File> {
    let (ours, theirs) = UnixStream::pair().context("creating a socket pair")?;
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(OwnedFd::from(theirs))
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start the device opener")?;
    // `cmd` still holds our copy of the child's end. Close it, or an opener
    // that exits without sending anything leaves recvmsg waiting forever.
    drop(cmd);
    // A write error means the opener already quit; its stderr says why.
    let _ = child.stdin.take().map(|mut s| s.write_all(input));

    let fd = recv_fd(&ours);
    let out = child
        .wait_with_output()
        .context("waiting for the device opener")?;
    match fd {
        Ok(Some(fd)) if out.status.success() => Ok(File::from(fd)),
        _ => {
            let msg = String::from_utf8_lossy(&out.stderr);
            Err(anyhow!(
                "could not open the drive: {}",
                if msg.trim().is_empty() {
                    out.status.to_string()
                } else {
                    msg.trim().to_string()
                }
            ))
        }
    }
}

/// Receive one file descriptor sent with SCM_RIGHTS. `None` if the sender
/// hung up without sending one.
fn recv_fd(sock: &UnixStream) -> std::io::Result<Option<OwnedFd>> {
    let mut data = [0u8; 64];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    // u64s, not bytes: the control buffer must be aligned for cmsghdr.
    let mut space = [0u64; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = space.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&space) as _;

    let n = loop {
        let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, 0) };
        if n >= 0 {
            break n;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    };
    if n == 0 {
        return Ok(None);
    }
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Ok(None);
        }
        let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::c_int>());
        Ok(Some(OwnedFd::from_raw_fd(fd)))
    }
}

/// Send `fd` over the Unix socket `sock` — the other half of `recv_fd`, used by
/// the Linux helper.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn send_fd(sock: libc::c_int, fd: libc::c_int) -> std::io::Result<()> {
    let int = std::mem::size_of::<libc::c_int>() as u32;
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut space = [0u64; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = space.as_mut_ptr().cast();
    unsafe {
        msg.msg_controllen = libc::CMSG_SPACE(int) as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(int) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::c_int>(), fd);
        if libc::sendmsg(sock, &msg, 0) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The Security.framework calls behind authopen's `-extauth`.
#[cfg(target_os = "macos")]
mod authorization {
    use anyhow::{anyhow, Result};
    use std::ffi::{c_char, c_void, CString};
    use std::ptr;

    #[repr(C)]
    struct AuthorizationItem {
        name: *const c_char,
        value_length: usize,
        value: *mut c_void,
        flags: u32,
    }

    #[repr(C)]
    struct AuthorizationItemSet {
        count: u32,
        items: *mut AuthorizationItem,
    }

    type AuthorizationRef = *mut c_void;

    const INTERACTION_ALLOWED: u32 = 1 << 0;
    const EXTEND_RIGHTS: u32 = 1 << 1;
    const PRE_AUTHORIZE: u32 = 1 << 4;
    const ERR_CANCELED: i32 = -60006;

    #[link(name = "Security", kind = "framework")]
    extern "C" {
        fn AuthorizationCreate(
            rights: *const AuthorizationItemSet,
            environment: *const AuthorizationItemSet,
            flags: u32,
            authorization: *mut AuthorizationRef,
        ) -> i32;
        fn AuthorizationMakeExternalForm(
            authorization: AuthorizationRef,
            form: *mut [u8; 32],
        ) -> i32;
        fn AuthorizationFree(authorization: AuthorizationRef, flags: u32) -> i32;
    }

    /// A granted right. Released on drop, so it never outlives the flash.
    pub struct Authorization(AuthorizationRef);

    impl Authorization {
        /// Ask for `right`. macOS shows its own administrator sheet.
        pub fn obtain(right: &str) -> Result<Self> {
            let name = CString::new(right)?;
            let mut item = AuthorizationItem {
                name: name.as_ptr(),
                value_length: 0,
                value: ptr::null_mut(),
                flags: 0,
            };
            let rights = AuthorizationItemSet {
                count: 1,
                items: &mut item,
            };
            let mut auth: AuthorizationRef = ptr::null_mut();
            let flags = INTERACTION_ALLOWED | EXTEND_RIGHTS | PRE_AUTHORIZE;
            let status = unsafe { AuthorizationCreate(&rights, ptr::null(), flags, &mut auth) };
            if status == 0 {
                return Ok(Self(auth));
            }
            if !auth.is_null() {
                unsafe { AuthorizationFree(auth, 0) };
            }
            Err(if status == ERR_CANCELED {
                anyhow!("Authentication cancelled.")
            } else {
                anyhow!("macOS did not grant access to the drive (authorization error {status})")
            })
        }

        /// The token authopen reads from stdin with `-extauth`.
        pub fn external_form(&self) -> Result<[u8; 32]> {
            let mut form = [0u8; 32];
            match unsafe { AuthorizationMakeExternalForm(self.0, &mut form) } {
                0 => Ok(form),
                status => Err(anyhow!(
                    "could not pass the authorization to authopen (error {status})"
                )),
            }
        }
    }

    impl Drop for Authorization {
        fn drop(&mut self) {
            unsafe { AuthorizationFree(self.0, 0) };
        }
    }
}

/// Bytes per sector. A raw macOS disk refuses reads and writes that are not
/// whole sectors; a Linux block device goes through the page cache and takes
/// any length.
fn sector_size(dev: &File) -> u64 {
    #[cfg(target_os = "macos")]
    {
        const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418; // _IOR('d', 24, uint32_t)
        let mut size: u32 = 0;
        let ok = unsafe { libc::ioctl(dev.as_raw_fd(), DKIOCGETBLOCKSIZE, &mut size as *mut u32) };
        if ok == 0 && size > 0 {
            size as u64
        } else {
            512
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = dev;
        1
    }
}

/// Write the image with dd(1). dd runs as the user, not root: it inherits the
/// already-open drive as its stdout. The image is pumped through this process
/// so a .xz decompresses on the way and progress can be counted.
fn run_dd(
    image: &str,
    dev: &File,
    progress: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    // ibs/obs rather than bs: dd then gathers short pipe reads into whole
    // 4 MiB writes, and a raw disk refuses anything but whole sectors.
    #[cfg(target_os = "macos")]
    let args = ["ibs=4m", "obs=4m"];
    #[cfg(target_os = "linux")]
    let args = ["bs=4M", "conv=fsync"];

    let mut child = Command::new("/bin/dd")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(dev.try_clone().context("duplicating the drive handle")?)
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start dd")?;

    let mut stdin = child.stdin.take().unwrap();
    let pumped = pump(image, &mut stdin, sector_size(dev), &progress, &cancel);
    drop(stdin);

    if cancel.load(Ordering::Relaxed) {
        let _ = child.kill();
    }
    let out = child.wait_with_output().context("waiting for dd")?;

    if cancel.load(Ordering::Relaxed) {
        return Err(anyhow!("cancelled"));
    }
    // dd's own message says more than the broken pipe it leaves us with.
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(
            "dd failed ({}): {}",
            out.status,
            msg.lines().next().unwrap_or("").trim()
        ));
    }
    pumped
}

/// Feed the image into dd, then zeros up to the next whole sector.
fn pump(
    image: &str,
    to_dd: &mut ChildStdin,
    sector: u64,
    progress: &AtomicU64,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut src = crate::image::open_image(image)?;
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut written = 0u64;
    while !cancel.load(Ordering::Relaxed) {
        let n = src.read(&mut buf).context("reading image")?;
        if n == 0 {
            let pad = (sector - written % sector) % sector;
            to_dd
                .write_all(&vec![0u8; pad as usize])
                .context("writing to dd")?;
            break;
        }
        to_dd.write_all(&buf[..n]).context("writing to dd")?;
        written += n as u64;
        progress.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(())
}

/// Make a partially written drive unambiguously blank.
///
/// A cancelled or failed write is the dangerous case, not a harmless one. An
/// image lays down its partition table and bootloader first, so a drive
/// abandoned at 60% still mounts, still looks bootable, and will still start an
/// installer that then fails partway through repartitioning the user's real
/// disk. Zeroing the head takes out the MBR, the primary GPT, and the common
/// filesystem superblocks; zeroing the tail takes out the backup GPT that would
/// otherwise let the OS reconstruct the partition table. What is left is a blank
/// drive the OS offers to initialise — which is what "cancel" should produce.
///
/// `size` is the drive size the OS reported: seeking to the end of a macOS
/// disk device answers 0, which would wipe nothing at all.
fn wipe(dev: &File, size: u64) -> Result<()> {
    const HEAD: u64 = 16 * 1024 * 1024;
    const TAIL: u64 = 1024 * 1024;

    zero(dev, 0, HEAD.min(size)).context("wiping the start of the drive")?;
    if size > HEAD + TAIL {
        zero(dev, size - TAIL, TAIL).context("wiping the end of the drive")?;
    }
    // macOS raw-device writes are already on the drive (and F_FULLFSYNC, which
    // sync_all uses there, is refused on device nodes). Linux goes through the
    // page cache.
    #[cfg(target_os = "linux")]
    dev.sync_all().context("flushing the drive")?;
    Ok(())
}

/// Write `len` zero bytes at `start`, in 1 MiB chunks — whole sectors for every
/// sector size in practice.
fn zero(mut dev: &File, start: u64, len: u64) -> std::io::Result<()> {
    let zeros = vec![0u8; 1024 * 1024];
    dev.seek(SeekFrom::Start(start))?;
    let mut left = len;
    while left > 0 {
        let n = left.min(zeros.len() as u64) as usize;
        dev.write_all(&zeros[..n])?;
        left -= n as u64;
    }
    Ok(())
}

/// SHA-256 of the first `len` bytes on the drive.
fn hash_device(
    mut dev: &File,
    len: u64,
    done: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
) -> Result<[u8; 32]> {
    // Linux would answer from the page cache the write just filled.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::posix_fadvise(dev.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
    let sector = sector_size(dev);
    dev.seek(SeekFrom::Start(0))
        .context("rewinding the drive")?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut left = len;
    while left > 0 {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("cancelled after write"));
        }
        // Read whole sectors, as the write did; hash only the image's share.
        let want = left.min(buf.len() as u64);
        let read = want.next_multiple_of(sector) as usize;
        dev.read_exact(&mut buf[..read])
            .context("reading the drive back")?;
        hasher.update(&buf[..want as usize]);
        left -= want;
        done.fetch_add(want, Ordering::Relaxed);
    }
    Ok(hasher.finalize().into())
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

            // Stall detection: if bytes have not moved for 120s, auto-cancel.
            // A ticker only starts once the drive is open, so there is no
            // password prompt to wait out — no movement at all is a stall too.
            if bytes == last_bytes {
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
    use super::*;

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
    /// charset-clean here but caught by `is_safe_device` before the drive is
    /// opened. This documents that the two checks are deliberately not the
    /// same check.
    #[test]
    fn charset_gate_is_not_the_shape_gate() {
        assert_eq!(resolve_device_path("disk2s1").unwrap(), "/dev/disk2s1");
        assert!(!crate::is_safe_device("/dev/disk2s1"));
    }

    /// authopen hands the drive over as a descriptor; ours must arrive as the
    /// same open file, not merely a valid number.
    #[test]
    fn a_passed_descriptor_is_the_same_open_file() {
        let path = std::env::temp_dir().join(format!("dd-etcher-fd-{}", std::process::id()));
        let file = File::create(&path).unwrap();
        let (a, b) = UnixStream::pair().unwrap();
        send_fd(a.as_raw_fd(), file.as_raw_fd()).unwrap();
        let got = File::from(recv_fd(&b).unwrap().unwrap());
        (&got).write_all(b"same file").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"same file");
        std::fs::remove_file(&path).unwrap();
    }

    /// An opener that quits without sending — authopen after a refusal — must
    /// come back as an error. Holding our own copy of its socket end would
    /// turn this into a hang.
    #[test]
    fn an_opener_that_sends_nothing_is_an_error() {
        assert!(receive_fd(Command::new("/usr/bin/false"), &[]).is_err());
    }

    /// Write, read back, wipe — on a plain file standing in for the drive. The
    /// image is deliberately not whole sectors: the drive gets a padded tail,
    /// and the read-back must still hash exactly the image's bytes.
    #[test]
    fn writes_pads_reads_back_and_wipes() {
        let dir = std::env::temp_dir().join(format!("dd-etcher-flash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("odd.img");
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8 + 1).collect();
        std::fs::write(&image, &data).unwrap();
        let dev = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join("drive"))
            .unwrap();
        let idle = || Arc::new(AtomicBool::new(false));

        let progress = Arc::new(AtomicU64::new(0));
        run_dd(image.to_str().unwrap(), &dev, progress.clone(), idle()).unwrap();
        assert_eq!(progress.load(Ordering::Relaxed), 1000);
        let padded = 1000u64.next_multiple_of(sector_size(&dev));
        assert_eq!(dev.metadata().unwrap().len(), padded);

        let read_back = hash_device(&dev, 1000, Arc::new(AtomicU64::new(0)), idle()).unwrap();
        let expected: [u8; 32] = Sha256::digest(&data).into();
        assert_eq!(read_back, expected);

        wipe(&dev, padded).unwrap();
        assert!(std::fs::read(dir.join("drive"))
            .unwrap()
            .iter()
            .all(|&b| b == 0));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
