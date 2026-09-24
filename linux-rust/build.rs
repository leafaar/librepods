//! Pick the FFmpeg channel API the installed libavcodec provides.
//!
//! FFmpeg 5.1 added AVChannelLayout and 7.0 removed the old `channels` and
//! `channel_layout` fields, so no single API builds everywhere: Ubuntu 22.04
//! ships 4.4, nixpkgs and Flatpak runtimes ship 7 and later. ffmpeg-sys-next
//! (links = "ffmpeg") exports its version probes as DEP_FFMPEG_*.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(ffmpeg_ch_layout)");
    println!("cargo::rerun-if-env-changed=DEP_FFMPEG_FFMPEG_5_1");
    if std::env::var("DEP_FFMPEG_FFMPEG_5_1").is_ok_and(|v| v == "true") {
        println!("cargo::rustc-cfg=ffmpeg_ch_layout");
    }
}
