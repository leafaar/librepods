# linux-rust

The Linux app: a GTK 4 / libadwaita front end over a tokio backend that speaks Apple's AACP
protocol to AirPods. Installing and using it is covered in the [top-level
README](../README.md); this file is for working on the code.

## Architecture

Two threads, joined by channels:

- **Backend** (tokio runtime, started in `main.rs`): BlueZ over D-Bus, the AACP connection
  to each pair of AirPods, audio profile handling, the hi-res microphone, auto-switch. It
  never touches widgets.
- **UI** (GTK main thread, `ui/gtk`): owns every widget. It receives `BluetoothUIMessage`
  from the backend on a tokio channel, and sends work back by spawning on the backend
  runtime handle. Nothing blocking runs on this thread.

`main.rs` first checks over D-Bus whether LibrePods already runs in the session; if so it
shows that window and exits, before a second backend could compete for the AirPods.

| Area | Modules |
| --- | --- |
| AACP connection, packet parsing, commands | `bluetooth/aacp.rs`, `bluetooth/l2cap.rs`, `bluetooth/aacp_audio.rs` |
| Setting and EQ encodings | `bluetooth/settings.rs`, `bluetooth/eq.rs` |
| BLE advertisements (battery, auto-connect) | `bluetooth/le.rs` |
| Nothing earbuds (ATT) | `bluetooth/att.rs`, `devices/nothing.rs` |
| Per-connection setup and event routing | `devices/airpods.rs` |
| A2DP profile, ear detection pause/resume, takeover | `media_controller.rs` |
| Sound server and media players | `audio/pulse.rs`, `audio/mpris.rs` |
| Hi-res microphone and its test | `audio/hires_mic.rs`, `audio/eld.rs`, `audio/agc.rs`, `audio/output.rs`, `audio/mic_test.rs` |
| Switching to this PC | `auto_switch.rs` |
| Settings and device storage | `utils.rs` |
| UI | `ui/gtk/*`, tray in `ui/tray.rs` and `ui/tray_icon.rs` |

### UI: model and view

`ui/gtk/mod.rs` spells the pattern out; in short:

1. Signal handlers only queue an `Input` (`widgets::Dispatch`). Backend messages go into the
   same queue.
2. One loop in `app.rs` applies the queued inputs to the `Model`, renders once, then runs
   the `Effect`s the model returned (send a command, save settings, start a timer).
3. Rendering never sends anything: widgets whose handlers send are wrapped in `Guarded`,
   which blocks the handler while the render sets the value.
4. Decisions live in the model (`model.rs`, `model/`, `controls.rs`, `battery.rs`), which
   has no GTK in it and is unit tested.

Only widgets available in GTK 4.6 and libadwaita 1.1 are used, so the app runs on Ubuntu
and Pop!_OS 22.04. `ui::gtk::smoke` builds the real widgets and renders a sequence of
states; it needs a display and is ignored by default.

### Testing seams

IO sits behind small traits so logic is tested with fakes instead of devices:

- `audio::pulse::SoundServer`: cards, profiles and sinks.
- `audio::mpris::MediaPlayers`: local media players.
- `bluetooth::aacp::DeviceStore`: the saved device records.
- AACP and ATT managers take their send channel from the test (`attach_transport`) and are
  driven with byte fixtures through `receive_packet`.
- Time-dependent logic uses `tokio::time::Instant`, so tests run on a paused clock.

## Checks

The same set CI runs (`.github/workflows/linux-rust-ci.yml`):

```bash
cargo +nightly fmt --all -- --check       # nightly: one `use` block per file
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo +1.92.0 check --locked              # the MSRV in Cargo.toml
cargo +nightly miri test --locked -- bluetooth:: audio::agc
cargo deny check                          # advisories, licenses, bans, sources
nix flake check                           # from the repository root
G_DEBUG=fatal-criticals cargo test smoke -- --ignored --test-threads=1   # needs a display
```

- The toolchain is pinned in `rust-toolchain.toml`; formatting needs nightly rustfmt for
  `imports_granularity`.
- Lints: `clippy::all` and `clippy::pedantic` warn, with the exceptions listed and explained
  in `Cargo.toml`; `unwrap_used` is only allowed in tests.
- `deny.toml` has no advisory exceptions. Adding one needs a reason next to it.
- The FFmpeg channel API differs across versions; `build.rs` selects it, and the code builds
  against FFmpeg 4.4 through 8.

## Conventions

- Errors are `thiserror` types, one per domain; `anyhow` only in `main.rs`.
- Logging goes through `tracing`; `RUST_LOG` overrides the defaults.
- Commits are signed and follow `feat|fix|refactor|chore(scope): title`, with one bullet
  per change in the body.
