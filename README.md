![dd-Etcher](assets/logo-wide.png)

A minimal image flasher for macOS (and soon Linux). Think Balena Etcher, but without the telemetry, the bloat, or the bundled Chromium. It wraps the reliable `dd(1)` shell tool directly under the hood.

> **Disclaimer:** dd-Etcher is in early development. Please use with caution and report bugs.

## Install

1. Download the `.dmg` from [Releases](https://github.com/sonalder-darlene/dd-etcher/releases). One build runs on both Apple Silicon and Intel Macs.
2. Drag **dd-Etcher** into `/Applications`.
3. dd-Etcher is not notarized by Apple, so on first launch macOS will claim the app "is damaged and can't be opened". It isn't — macOS says this about every download that is not notarized. Clear the download flag once in Terminal:

   ```sh
   xattr -dr com.apple.quarantine /Applications/dd-Etcher.app
   ```

4. Launch it. When you flash, macOS asks for an administrator password (or Touch ID) and, the first time only, whether dd-Etcher may access removable volumes — click **Allow**. No Full Disk Access ([how](#permissions)).

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
- Flash via `dd`, running as you — macOS's own password prompt opens the drive, nothing runs as root
- Verify flash integrity via SHA-256 checksum with live progress
- No Full Disk Access — only the narrow Removable Volumes permission, granted with one click
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
│  • progress bar             │      │  lib.rs     — app setup          │
└─────────────────────────────┘      └──────────────────────────────────┘
                                                    │
                                     authopen(1) opens /dev/rdiskN and
                                     hands back a file descriptor
                                                    │
                                     dd (as you) ──► the drive
```

### Permissions

Writing a whole disk takes administrator approval, but no part of dd-Etcher runs as root. It uses `authopen(1)`, the tool macOS provides for opening a device with authorization:

1. **Ask.** dd-Etcher requests the `sys.openfile.readwrite./dev/rdiskN` right for the drive you picked. macOS shows its own administrator prompt, naming dd-Etcher. Cancel stops there, before the drive is even unmounted.
2. **Open.** `/usr/libexec/authopen` opens the raw device and passes the open file descriptor back over a socket. authopen checks the Removable Volumes privacy permission against dd-Etcher rather than itself, which is why the first flash shows a one-time "access files on a removable volume" prompt.
3. **Write, verify, wipe — as you.** `dd` runs with your own rights and the drive as its stdout. The image is piped into it from the app, so a `.xz` decompresses on the way and progress can be counted. The read-back, and the wipe after a cancelled or failed write, go through the same descriptor.

Earlier versions ran `dd` as root through `sudo`. macOS checks a root process against the system-wide privacy rules, so that only worked once the app had Full Disk Access.

### Drive filtering

- **macOS** — `diskutil list -plist external` returns only external media. The boot disk never appears.
- **Linux** — `lsblk --json` filtered to `type=disk` and `rm=true` (removable).

### Verification

The app hashes the source image while it writes. It then reads back exactly `image_size` bytes through the same raw-device descriptor — uncached, and still held open, so macOS cannot mount the new partitions mid-check — and compares the two SHA-256s. A mismatch is a hard error.

## Requirements

- macOS 13 Ventura or later (uses WKWebView, `diskutil`, `authopen`)
- An administrator password, to approve each flash

If you clicked **Don't Allow** at the removable-volume prompt, turn dd-Etcher back on in System Settings → Privacy & Security → Files & Folders → Removable Volumes.

## Development

### Prerequisites

- Rust 1.85+ (`rustup update stable`) — 1.77+ declared in `Cargo.toml` but some dependencies require 1.85 in practice
- Node 20+ and pnpm (`npm install -g pnpm`)

### Run

```sh
pnpm install
pnpm tauri dev
```

> **Note:** In dev mode the app is a child of your terminal, so macOS may name the terminal, not dd-Etcher, in the removable-volume prompt.

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

The Rust core has platform-specific branches for drive enumeration (`lsblk` instead of `diskutil`) and for opening the drive: where macOS has `authopen`, Linux runs the app's own `--privileged-open` helper through `pkexec`, which opens the device and passes the descriptor back. Writing and verifying are shared. The frontend is identical. The Linux path has not been run on real hardware yet — contributions and testing are welcome.

## Roadmap

- [x] No Full Disk Access: `authopen` instead of `sudo`, nothing runs as root
- [ ] Friendlier error messages when `dd` fails mid-write (capture and surface `dd` stderr)
- [x] Progress tracking during the verify phase
- [ ] TUI frontend sharing the same Rust core (`ratatui`)
- [x] Compressed image support (`.img.xz`); `.gz` still to do
- [ ] CI: build + notarize macOS, Flatpak on Linux

## Logo icon

The icon uses the [Gulax](https://velvetyne.fr/fonts/gulax/) typeface, designed by Morgan Gilbert with contributions from Anton Moglia. Originally published on March 4, 2014 under the [SIL Open Font License, Version 1.1](https://openfontlicense.org).

## License

GPL-3.0 — see [LICENSE](LICENSE).
