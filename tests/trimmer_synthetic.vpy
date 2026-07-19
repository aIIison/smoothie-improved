"""VSPipe smoke test for GUI trimming before Smoothie's processing stages."""

import configparser
import json
import os

import vapoursynth as vs


ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if not os.path.exists(os.path.join(ROOT, "target", "defaults.ini")):
    ROOT = next(
        candidate
        for candidate in (os.getcwd(), os.path.dirname(os.getcwd()))
        if os.path.exists(os.path.join(candidate, "target", "defaults.ini"))
    )
JAMBA = os.path.join(ROOT, "target", "jamba.vpy")
DEFAULTS = os.path.join(ROOT, "target", "defaults.ini")

config = configparser.ConfigParser(interpolation=None)
config.read(DEFAULTS)
data = {section: dict(config[section]) for section in config.sections()}
for section in ("interpolation", "frame blending", "flowblur", "artifact masking", "pre-interp"):
    data[section]["enabled"] = "no"
data["preview window"]["enabled"] = "no"
data["miscellaneous"]["play ding"] = "no"

recipe = json.dumps({"data": data})
input_video = "synthetic"
container_fps = 30
video_in = vs.core.std.BlankClip(width=320, height=180, length=120, fpsnum=30, fpsden=1)
trim_start = "1.0"
trim_end = "2.0"

__file__ = JAMBA
with open(JAMBA, "rb") as script:
    exec(compile(script.read(), JAMBA, "exec"), globals(), globals())

assert clip.num_frames == 30, f"expected 30 trimmed frames, got {clip.num_frames}"
