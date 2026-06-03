# video-capture-share

Low overhead capture card preview and LAN relay for streamers on low end or workaround setups.

## What this is for

If you stream a console (Switch, PS, Xbox) through an HDMI splitter into a budget capture card, the standard tools work but cost you frames. OBS preview alone can eat real CPU, and adding a second PC usually means buying another capture card. This tool gives you two things that solve those pain points without extra hardware:

1. A direct, low latency preview window for the capture device. No scene compositor, no encoder, just pixels from the device drawn to a window.
2. An optional MJPEG over HTTP relay so a second PC or another OBS instance can pull the feed over your LAN as a browser source.

Built for the case where every CPU percent matters.

## Goals

- Single small native binary, no installer, no runtime.
- Zero copy from capture device to screen where the OS allows it.
- Sane defaults that work the first time on a fresh Windows machine.
- Plain text config, plain text logs. Nothing to learn.

## Non goals

- Replacing OBS. This does not encode, composite, or stream to Twitch.
- Fancy effects, overlays, scenes, or transitions.
- Touching audio. Use whatever you already use for audio.

## Status

Early development. Targeting Windows first because that is where capture cards live.

## Build

Requires a Rust toolchain (stable). On Windows you also need either the MSVC build tools or the GNU toolchain.

```
cargo build --release
```

The binary will be at `target/release/video-capture-share.exe`.

## Run

```
video-capture-share.exe              # list devices and exit
video-capture-share.exe --device 0   # open device 0 in a preview window
video-capture-share.exe --device 0 --serve 0.0.0.0:8080   # also serve MJPEG
```

In OBS on the second PC, add a Browser source pointed at `http://<host>:8080/`.

## License

MIT
