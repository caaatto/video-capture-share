# vicash

Use your capture card as a second monitor for your console, without an actual second monitor. Single 10 MB exe, no installer, no setup. VIdeo CApture SHare.

![vicash preview window with F1 settings panel open](assets/screenshots/hero.png)

## Download

[Latest release](https://github.com/caaatto/vicash/releases/latest) - one file, double-click, pick your capture card from the dropdown, you are done. Press F1 inside the window for live settings.

## What it solves

You plug a console (Switch, PS, Xbox) into an HDMI splitter, one leg goes to a TV, the other into a USB capture card on your PC. You want to actually use that capture feed as a playable monitor: low latency, with audio, full screen, without buying anything else.

The usual options all hurt:

- OBS preview adds 100-400 ms of lag and burns CPU you wanted for the game.
- The Windows Camera app treats your capture card like a webcam, no audio, no fullscreen toggle.
- Elgato's tools only work with Elgato hardware.
- VLC and PotPlayer can open a DirectShow device but default to hundreds of ms of caching.

vicash is built for the cheap "fake USB3" capture cards (MS2109, MS2130, generic AliExpress HDMI grabbers) that everyone actually owns. It defaults to settings those cards can actually deliver, ships latency numbers you can verify in the F1 panel, and does not try to be OBS.

## Features

- GPU rendered preview, redraws only when capture publishes a new frame so CPU and GPU stay idle the rest of the time
- NV12 zero-copy upload: Y and UV planes go straight to GPU textures, BT.709 conversion in a shader, no CPU colour conversion
- Borderless fullscreen + always on top + hide cursor (F11) so the window behaves like a real second monitor
- Audio passthrough from the capture card audio device to your default output, with a live volume slider, mute, and a sync delay slider so you can match audio to picture by ear
- Live device switching from the F1 panel: pick audio input, audio output, capture resolution and fps without restarting
- MJPEG over HTTP relay (`--serve 0.0.0.0:8080`) so a second PC can pull the feed in OBS as a browser source
- Honest performance dashboard: capture-to-present pipeline latency in milliseconds and capture-side frame interval, so you can see exactly which part of the chain is slow
- Settings persist to `%APPDATA%\caaatto\vicash\config.toml` and reload on next launch
- Three UI languages: Deutsch, English, 简体中文

<table>
<tr>
<td><img src="assets/screenshots/stats-overlay.png" alt="Stats overlay corner" width="380"></td>
<td><img src="assets/screenshots/language-picker.png" alt="Language picker DE / EN / 简体中文" width="380"></td>
</tr>
</table>

## How vicash compares

| | vicash | OBS preview | Elgato 4K Capture | VLC / PotPlayer | TackleCast | VideoGameCapture |
|---|---|---|---|---|---|---|
| Single .exe, no install | yes | no | no | no | zip | Unity bundle |
| Audio passthrough with live sync slider | yes | partial | yes | no | yes (no slider) | JSON edit only |
| MJPEG HTTP relay | yes | no | no | no | no | no |
| Optimised for cheap MS2109 / MS2130 cards | yes | no | no | no | partial | partial |
| Works without vendor lock-in | yes | yes | Elgato only | yes | yes | yes |
| Live capture device / resolution / fps switch | yes | yes | partial | partial | partial | partial |
| Pipeline latency meter in UI | yes | no | no | no | no | no |

If you have an Elgato card and already run OBS, Elgato's own utility is fine. If you want one .exe that opens, shows your console, lets you tweak everything live, and tells you exactly how much latency the software is adding, vicash is the tighter fit.

## Run

Inside the preview window:

- `F1` opens the settings panel (Sprache, Monitor-Modus, Anzeige, Capture, Audio, Relay, Performance)
- `F11` toggles borderless fullscreen, `Esc` always leaves fullscreen

Command line, if you prefer:

```
vicash.exe                                    # interactive device picker
vicash.exe --list                             # list video devices and exit
vicash.exe --list-audio                       # list audio devices and exit
vicash.exe --device 0                         # open device 0 in a preview window
vicash.exe --device 0 --audio                 # also pass audio through to default output
vicash.exe --device 0 --serve 0.0.0.0:8080    # also serve MJPEG over HTTP
```

In OBS on the second PC, add a Browser source pointed at `http://<host>:8080/`.

## A note on the cheap-card limit

A lot of capture cards sold as USB 3.0 are actually USB 2.0 chips inside a blue housing. They list 1920x1080 60fps as a supported mode but the USB pipe cannot sustain the bandwidth, so they silently fall back to 30 fps once you start streaming. vicash defaults to 1280x720 60 fps for this reason; that fits comfortably inside even fake-USB3 bandwidth and matches the native output of the most common console (Switch). If your card is genuinely fast enough for 1080p60, switch to 1080p in the F1 Capture section, hit Apply, and vicash will remember it.

The Performance section in F1 shows your actual capture interval (target 16.7 ms at 60 fps) so you can see whether the card is keeping up.

## Non goals

vicash does not replace OBS. It does not encode, composite, or stream to Twitch. No scenes, no transitions, no effects, no recording.

## Build from source

Requires a Rust toolchain (stable). On Windows you also need either the MSVC build tools, or the GNU toolchain (`rustup default stable-x86_64-pc-windows-gnu`) with MinGW-w64 on PATH so `windres.exe` can compile the embedded icon resource.

```
cargo build --release
```

The binary lands at `target/release/vicash.exe`.

## License

MIT. The bundled JetBrains Mono font is under the SIL Open Font License 1.1 (see `assets/JetBrainsMono-OFL.txt`).
