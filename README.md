# Smoothie *improved*

This fork of [Smoothie](https://github.com/couleur-tweak-tips/smoothie-rs)
is made for turning gameplay recordings into smooth, shareable clips without
destroying the crosshair and HUD in the process.

It keeps Smoothie's interpolation, frame blending, FlowBlur, encoding, and
recipe controls, while adding automatic HUD protection and a complete trimming
workflow inside the app.

## Features added by this fork

### Automatic HUD protection

- Detects persistent screen-fixed details such as crosshairs, health bars,
  ability icons, minimaps, input overlays, and static text.
- Keeps detected details sharp during interpolation and FlowBlur while the game
  world still receives normal motion processing.
- Works automatically without requiring a game profile or hand-drawn mask.
- Enabled by default, with an optional mask preview so you can inspect what will
  be protected before rendering.
- Still supports manual masks for unusual footage or additional corrections.
- Fails conservatively when a clip does not contain enough visual movement for
  reliable automatic detection.

Automatic detection is intended for persistent HUD elements. Animated alerts,
killfeeds, damage effects, and other short-lived overlays may not always be
detected.

### Built-in clip trimmer

- Drag one or more recordings directly into the app.
- Preview video and audio without leaving Smoothie.
- Scrub using timeline thumbnails and frame-step controls.
- Select frame-accurate in and out points with timeline handles, timestamps, or
  the `I` and `O` shortcuts.
- Choose exactly which audio tracks to keep, and audition tracks individually.
- Configure multiple clips in a queue and render them together.

### Share-ready file-size targeting

- Optionally keep each rendered clip below a limit such as `50 MB` for Discord.
- Produces a broadly compatible MP4 with H.264 video and AAC audio.
- Calculates an appropriate bitrate from the selected duration and audio tracks.
- Verifies the finished file size and automatically retries at a corrected
  bitrate only if the result exceeds the limit.
- Uses a fast single-pass render in the normal case, so Smoothie's interpolation
  and blur pipeline is not unnecessarily processed twice.

Leave size targeting disabled to use the encoder and container selected in your
recipe unchanged.

## Quick start

1. Download the [latest release](../../releases/latest/download/smoothie-rs-nightly.zip).
2. Extract the archive and run `launch.cmd`.
3. Drop in one or more videos.
4. Select each clip's range, audio tracks, and optional file-size limit.
5. Select **Render all clips**.

HUD protection is already enabled with sensible defaults.

## Adjusting HUD detection

Enable `preview mask` under **artifact masking** to inspect the automatic mask.
Black areas are protected; white areas receive normal processing. Disable the
preview again before the final render.

If the mask needs adjustment, change `auto sensitivity`:

- Lower values create stricter, tighter protection.
- Higher values protect more of the image.

## Building

```powershell
cargo build --release
```

The release workflow packages Smoothie with its portable VapourSynth runtime.

## Upstream and license

This project is based on
[couleur-tweak-tips/smoothie-rs](https://github.com/couleur-tweak-tips/smoothie-rs).
The original project and this fork are distributed under the
[GNU General Public License v3.0](./LICENSE).
