# Smoothie - automatic HUD protection fork

This fork of [Smoothie](https://github.com/couleur-tweak-tips/smoothie-rs)
adds automatic protection for crosshairs, HUD elements, input overlays, and
other graphics that remain fixed on screen.

Smoothie can create very smooth motion blur by interpolating and blending
frames. In game footage, motion-based filters may also distort the HUD. This
fork analyzes the video, finds persistent screen-fixed details, and keeps them
clear while the game world is processed normally.

## What this fork adds

- Automatic HUD and crosshair detection.
- Protection during frame interpolation and FlowBlur.
- No game-specific profiles or hand-made masks required.
- Optional mask preview for checking a clip before rendering.
- Manual masks remain supported when automatic detection needs help.

HUD protection is enabled by default. Existing interpolation, frame blending,
FlowBlur, encoding, and recipe controls remain available.

## Download and use

1. Download the [latest release](../../releases/latest/download/smoothie-rs-nightly.zip).
2. Extract the archive.
3. Run `launch.cmd`.
4. Select a video and render it normally.

The automatic settings should work without adjustment. To inspect what was
detected, enable `preview mask` under **artifact masking**. Black areas are
protected; white areas receive normal processing. Disable the preview again
for the final render.

If detection is too broad or too narrow, adjust `auto sensitivity`:

- Lower values are stricter and produce tighter masks.
- Higher values protect more of the image.

## Building

```powershell
cargo build --release
```

The release workflow packages Smoothie together with its portable
VapourSynth runtime.

## Upstream and license

This project is based on
[couleur-tweak-tips/smoothie-rs](https://github.com/couleur-tweak-tips/smoothie-rs).
The original project and this fork are distributed under the
[GNU General Public License v3.0](./LICENSE).
