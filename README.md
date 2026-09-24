# LibrePods for Linux (unofficial fork)

[![linux-rust CI](https://github.com/leafaar/librepods/actions/workflows/linux-rust-ci.yml/badge.svg)](https://github.com/leafaar/librepods/actions/workflows/linux-rust-ci.yml)

Control your AirPods from Linux like an Apple device does: listening modes, battery, ear
detection, conversation awareness, the high-quality microphone, switching between your
phone and your PC, and the settings Apple keeps in iOS.

This is an **unofficial fork** of [LibrePods](https://github.com/librepods-org/librepods)
by Kavish Devar, maintained by [leafaar](https://github.com/leafaar). It is not affiliated
with or endorsed by the LibrePods project. The fork focuses on the Linux app in
[`linux-rust/`](linux-rust/); the Android app in [`android/`](android/) is upstream's code,
unchanged here. For Android, use [upstream](https://github.com/librepods-org/librepods).

## What is different from upstream

- **A native GNOME app.** The UI is GTK 4 and libadwaita instead of iced: system font,
  Adwaita icons, follows GNOME's light and dark style, one instance per session, closes to
  the tray. It works back to GTK 4.6 and libadwaita 1.1 (Ubuntu and Pop!_OS 22.04).
- **The hi-res microphone works on this branch.** Apple's high-quality AirPods microphone
  stream (AAC-ELD over AACP) shows up as a PipeWire input while music keeps playing in full
  quality, with a built-in microphone test (record, play back, seek). It builds against
  FFmpeg 4.4 through 8.
- **Switching between your phone and this PC.** A "Connect to this PC" button and tray
  item, and an optional automatic switch when media starts playing here.
- **Settings backported from the Android app.** Press and hold, call controls, microphone
  side, press speed and duration, noise cancellation with one AirPod, tone volume, volume
  swipe, Adaptive Audio strength, pause when falling asleep, and the custom equalizer.
- **Reliability fixes.** Malformed packets no longer crash the app, battery shows up after
  taking the AirPods over from a phone, the case shows its last known level, the audio
  profile no longer fights playback, AAC is the default codec, and the device files are
  written atomically.
- **No dependency advisories.** `cargo deny` passes with no exceptions, the whole crate is
  clippy-pedantic clean, and CI checks every push.

## Features

| Feature | Linux (this fork) |
| --- | --- |
| Listening modes (Off, Transparency, Adaptive, Noise Cancellation) | ✅ |
| Battery for each bud and the case (last known case level) | ✅ |
| Ear detection with automatic play and pause | ✅ |
| Conversation awareness, personalized volume | ✅ |
| Rename | ✅ |
| Hi-res microphone with music kept in full quality | ✅ |
| Microphone test | ✅ |
| Connect to this PC, switch on playback | ✅ |
| Press and hold, calls, microphone side, accessibility, Adaptive Audio strength, sleep detection | ✅ new, report issues |
| Custom equalizer | ✅ new; needs recent AirPods firmware |
| Hearing aid, transparency customization, loud sound reduction | ❌ needs VendorID spoofing, see below |
| Head gestures, spatial audio, heart rate, Find My | ❌ |

### VendorID spoofing

Some features only unlock when the computer claims to be an Apple device
(`DeviceID = bluetooth:004C:0000:0000` in `/etc/bluetooth/main.conf`). With current AirPods
firmware (for example 9A348 on AirPods Pro 3) this causes constant disconnects, so this
fork does not rely on it and does not ship features that need it.

## Install

The app builds from source; AppImage and Flatpak packaging have not been updated for the
GTK UI yet.

### Dependencies

Ubuntu, Pop!_OS, Debian:

```bash
sudo apt install build-essential pkg-config libclang-dev libdbus-1-dev libpulse-dev \
  libavcodec-dev libavutil-dev libgtk-4-dev libadwaita-1-dev
```

Fedora:

```bash
sudo dnf install gcc pkg-config clang-devel dbus-devel pulseaudio-libs-devel \
  ffmpeg-free-devel gtk4-devel libadwaita-devel
```

Arch:

```bash
sudo pacman -S base-devel clang dbus libpulse ffmpeg gtk4 libadwaita
```

Rust comes from [rustup](https://rustup.rs); the toolchain is pinned in
`linux-rust/rust-toolchain.toml` and installed on first build.

### Build and install

```bash
git clone https://github.com/leafaar/librepods
cd librepods/linux-rust
cargo build --release
install -Dm755 target/release/librepods ~/.local/bin/librepods
install -Dm644 assets/icon.png ~/.local/share/icons/hicolor/256x256/apps/me.kavishdevar.librepods.png
install -Dm644 assets/me.kavishdevar.librepods.desktop ~/.local/share/applications/me.kavishdevar.librepods.desktop
```

To start it with your session, minimized to the tray:

```bash
mkdir -p ~/.config/autostart
sed 's|^Exec=.*|Exec=librepods --start-minimized|' assets/me.kavishdevar.librepods.desktop \
  > ~/.config/autostart/librepods.desktop
```

### Nix

```bash
nix run github:leafaar/librepods
```

## Usage

Pair the AirPods in your Bluetooth settings first, then open LibrePods.

```text
librepods [OPTIONS]
  -d, --debug            Debug logging
      --no-tray          Run headless: no window and no tray, only the Bluetooth logic
      --start-minimized  Start with the window hidden in the tray
      --le-debug         Debug logging for Bluetooth LE (very verbose)
  -v, --version          Show the version
```

- **Tray.** On GNOME the tray icon needs the AppIndicator extension (on by default on
  Ubuntu and Pop!_OS). Closing the window keeps the app in the tray.
- **Hi-res microphone.** Turn it on in the AirPods page, then pick "AirPods_HiRes_Mic" as
  the microphone in your app. The mic test on the same page records you and plays it back.
- **Media controls.** With PipeWire, stem presses need WirePlumber's AVRCP player:

  ```ini
  # ~/.config/wireplumber/wireplumber.conf.d/51-bluez-avrcp.conf
  monitor.bluez.properties = {
    bluez5.dummy-avrcp-player = true
  }
  ```

  Do not run `mpris-proxy` together with WirePlumber.
- **Logs.** `librepods -d`, or `RUST_LOG=librepods=debug librepods`.

## Development

See [`linux-rust/README.md`](linux-rust/README.md) for the architecture and conventions.
Every change passes the same checks as CI:

```bash
cd linux-rust
cargo +nightly fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo deny check
nix flake check   # from the repository root
```

The protocol notes in [`docs/`](docs/) describe the AACP packets the app speaks. The
Wireshark dissector by [@pabloaul](https://github.com/pabloaul/apple-wireshark) covers the
wider set of Apple Bluetooth protocols.

## Credits

- [Kavish Devar](https://github.com/kavishdevar) and the
  [LibrePods contributors](https://github.com/librepods-org/librepods/graphs/contributors),
  who wrote LibrePods and reverse engineered the AirPods protocol.
- Upstream pull requests merged into this fork:
  - #655 hi-res microphone support, by [@LuanAdemi](https://github.com/LuanAdemi)
  - #766 A2DP profile handling and codec choice, #775 send path deadlock, by
    [@injkgz](https://github.com/injkgz)
  - #724 takeover with two devices connected, by
    [@RamfiAogusto](https://github.com/RamfiAogusto)
  - #688 stale playback listeners, #689 takeover state lock, by
    [@mmatczuk](https://github.com/mmatczuk)
  - #733 tray startup, by [@harshach](https://github.com/harshach)
  - #586 scrollable device views, by [@debarkak](https://github.com/debarkak)
  - #665 open the window from the tray, by
    [@ousamabenyounes](https://github.com/ousamabenyounes)

# License

LibrePods - AirPods liberated from Apple’s ecosystem
Copyright (C) 2025 LibrePods contributors

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 3 of the License, or
any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with this program.  If not, see <https://www.gnu.org/licenses/>.

# Trademark Notice

The GPL does not grant any rights to use the LibrePods name, logo, or branding. The LibrePods name and logo may not be used for software, websites, domains, products, services, or other projects in a manner that suggests affiliation with, endorsement by, or association with the official LibrePods project without prior permission.

If you see any misuse of the LibrePods name or logo, please report it to [me@kavish.xyz](mailto:me@kavish.xyz).

The SF Pro font used in the Android app is the property of Apple Inc.. This will be removed in future versions of the app and replaced with an open alternative soon.

AirPods, AirPods Pro, AirPods Max, and the AirPods logo are trademarks of Apple Inc. The LibrePods project is not affiliated with or endorsed by Apple Inc. in any way.
