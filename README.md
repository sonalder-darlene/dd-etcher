![dd-Etcher](assets/logo-wide.png)

A minimal image flasher for macOS (and soon Linux). Think Balena Etcher, but without the telemetry, the bloat, or the bundled Chromium. It wraps the reliable `dd(1)` shell tool directly under the hood.

> **Disclaimer:** dd-Etcher is in early development. Please use with caution and report bugs.

## Install

1. Download the `.dmg` from [Releases](https://github.com/sonalder-darlene/dd-etcher/releases). One build runs on both Apple Silicon and Intel Macs.
2. Drag **dd-Etcher** into `/Applications`. Install it there, not in `~/Applications`: the app runs itself as root to write the drive, which is only safe from a folder you cannot write to without a password.
3. dd-Etcher is not notarized by Apple, so on first launch macOS will claim the app "is damaged and can't be opened". It isn't — macOS says this about every download that is not notarized. Clear the download flag once in Terminal:

   ```sh
   xattr -dr com.apple.quarantine /Applications/dd-Etcher.app
   ```

4. Launch it. It will ask for **Full Disk Access** and walk you through granting it ([why](#requirements)).

## Why

Balena Etcher is the de-facto GUI image flasher on macOS but ships with aggressive telemetry and a heavy Electron bundle. For many users, the terminal is intimidating and `dd` has a reputation for being "Disk Destroyer". That's why I created a BS-free `dd` GUI with the help of Claude Code. dd-Etcher does the same four things as Balena Etcher:
1. Pick an image
2. Choose a drive
3. Flash the image onto it
4. Verify its integrity

It aims to be a simple, bloat-free, telemetry-free tool for people who are uncomfortable using `dd` in the terminal directly.

## Features

- Select an image (`.img`, `.iso`, `.dmg`, `.bin`, `.raw`) or an xz-compressed one (`.img.xz`) — the format Raspberry Pi OS ships in
- Drag and drop an image onto the window
- Select an external drive (internal drives should be hidden automatically to avoid mistakes)
- Flash via `dd` (It will ask for your admin password to do so)
- Verify flash integrity via SHA-256 checksum with live progress
- Full Disk Access onboarding screen — blocks the UI and guides you through the one-time setup if FDA has not been granted yet
- Built with Rust and Tauri + WKWebView
- No telemetry, no background network calls, no accounts, no BS

## Architecture

```
┌─────────────────────────────┐      ┌──────────────────────────────────┐
│  Frontend (TypeScript)      │      │  Backend (Rust / Tauri)          │
│  src/                       │◄────►│  src-tauri/src/                  │
│                             │ IPC  │                                  │
│  • drive picker             │      │  drives.rs  — enumerate drives   │
│  • image picker             │      │  flash.rs   — auth + flash       │
│  • progress bar             │      │  lib.rs     — privileged helper  │
└─────────────────────────────┘      └──────────────────────────────────┘
                                                    │
                                          sudo (child process)
                                                    │
                                      dd-etcher --privileged-flash
                                      dd-etcher --privileged-sha256
```

### Privilege escalation

Writing to a raw block device requires root. dd-Etcher uses a two-step approach that keeps the process tree intact so macOS TCC correctly identifies our app as the responsible process:

1. **Authenticate once:** a native macOS password dialog (via `osascript`) collects the credential. `sudo -S -v` validates and caches it. Clicking Cancel aborts immediately.
2. **Flash and verify as root:** `sudo -n` (non-interactive, cached credential) invokes our own binary with `--privileged-flash` or `--privileged-sha256`. No separate helper binary is needed; the app doubles as its own privileged helper.

The image is piped from our process (which has read access granted by the file picker dialog) directly into the privileged helper's stdin, bypassing macOS's TCC restriction on reading `~/Downloads` as root.

### Drive filtering

- **macOS** — `diskutil list -plist external` returns only external media. The boot disk never appears.
- **Linux** — `lsblk --json` filtered to `type=disk` and `rm=true` (removable).

### Verification

After flashing, the drive is force-unmounted (macOS auto-mounts recognised filesystems). The privileged helper reads back exactly `image_size` bytes from the device and computes a SHA-256. The app hashes the source image independently. A mismatch is a hard error.

## Requirements

- macOS 13 Ventura or later (uses WKWebView, `diskutil`, `sudo`)
- The app must be granted **Full Disk Access** in System Settings → Privacy & Security → Full Disk Access

> **Why Full Disk Access?** macOS 15+ enforces TCC for block device access even for root processes. FDA is the user-controlled gate; no app can bypass it silently.

## Development

### Prerequisites

- Rust 1.85+ (`rustup update stable`) — 1.77+ declared in `Cargo.toml` but some dependencies require 1.85 in practice
- Node 20+ and pnpm (`npm install -g pnpm`)

### Run

```sh
pnpm install
pnpm tauri dev
```

Grant Full Disk Access to the debug binary the first time:
`src-tauri/target/debug/dd-etcher`

> **Note:** The FDA onboarding overlay only activates in release builds. In dev mode the check is skipped automatically, so the overlay will not appear. The FDA grant is still required for the privileged flash helper to write to the drive — grant it once and it persists across rebuilds.

### Build

```sh
pnpm tauri build
```

Produces a `.app` and `.dmg` for your own Mac's architecture in `src-tauri/target/release/bundle/`. Code signing and notarization require an Apple Developer certificate.

Releases are universal (Apple Silicon and Intel in one binary):

```sh
rustup target add x86_64-apple-darwin aarch64-apple-darwin
pnpm tauri build --target universal-apple-darwin
```

Output lands in `src-tauri/target/universal-apple-darwin/release/bundle/`.


## Porting to Linux

The Rust core has platform-specific branches for drive enumeration (`lsblk` instead of `diskutil`) and privilege escalation (`pkexec` instead of `sudo`). The frontend is identical. Contributions and testing are welcome.

## Roadmap

- [ ] Improve privilege escalation, probably via SMAppService helper (removes the FDA requirement)
- [ ] Friendlier error messages when `dd` fails mid-write (capture and surface `dd` stderr)
- [x] Progress tracking during the verify phase
- [ ] TUI frontend sharing the same Rust core (`ratatui`)
- [x] Compressed image support (`.img.xz`); `.gz` still to do
- [ ] CI: build + notarize macOS, Flatpak on Linux

## Logo icon

The icon uses the [Gulax](https://velvetyne.fr/fonts/gulax/) typeface, designed by Morgan Gilbert with contributions from Anton Moglia. Originally published on March 4, 2014 under the [SIL Open Font License, Version 1.1](https://openfontlicense.org).

## License

GPL-3.0 — see [LICENSE](LICENSE).
