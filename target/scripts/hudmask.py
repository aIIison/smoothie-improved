import sys
from typing import List

import vapoursynth as vs
from vapoursynth import core


def _eprint(message: str) -> None:
    print(f"HUD mask: {message}", file=sys.stderr)


def _sample_indices(frame_count: int, sample_count: int) -> List[int]:
    if frame_count <= 1:
        return [0]

    pool_size = min(frame_count, max(sample_count * 3, sample_count))
    if pool_size == 1:
        return [0]

    return sorted({
        round(i * (frame_count - 1) / (pool_size - 1))
        for i in range(pool_size)
    })


def _diverse_indices(
    proxy: vs.VideoNode,
    candidates: List[int],
    sample_count: int,
    minimum_difference: float = 0.012,
) -> List[int]:
    selected = [candidates[0]]

    for candidate in candidates[1:]:
        previous = selected[-1]
        difference = core.std.PlaneStats(
            proxy[previous:previous + 1],
            proxy[candidate:candidate + 1],
        )
        value = float(difference.get_frame(0).props["PlaneStatsDiff"])

        if value >= minimum_difference:
            selected.append(candidate)

    if len(selected) <= sample_count:
        return selected

    return [
        selected[round(i * (len(selected) - 1) / (sample_count - 1))]
        for i in range(sample_count)
    ]


def _running_average(clips: List[vs.VideoNode]) -> vs.VideoNode:
    result = clips[0]
    for index, current in enumerate(clips[1:], start=2):
        result = core.std.Merge(result, current, weight=1.0 / index)
    return result


def _reduce(clips: List[vs.VideoNode], operator: str) -> vs.VideoNode:
    result = clips[0]
    for current in clips[1:]:
        result = core.std.Expr([result, current], expr=f"x y {operator}")
    return result


def _expand(clip: vs.VideoNode, passes: int) -> vs.VideoNode:
    for _ in range(passes):
        clip = core.std.Maximum(clip)
    return clip


def _build_protection_mask(
    frames: List[vs.VideoNode],
    sensitivity: float,
) -> vs.VideoNode:
    edge_maps = [
        core.std.Binarize(core.std.Prewitt(frame), threshold=20)
        for frame in frames
    ]

    edge_persistence = _running_average(edge_maps)
    persistence_threshold = round((0.96 - 0.46 * sensitivity) * 255)
    persistent_edges = core.std.Binarize(
        edge_persistence,
        threshold=persistence_threshold,
    )

    temporal_max = _reduce(frames, "max")
    temporal_min = _reduce(frames, "min")
    temporal_range = core.std.Expr(
        [temporal_max, temporal_min],
        expr="x y -",
    )
    stable_threshold = round(3 + 17 * sensitivity)
    stable_pixels = core.std.Binarize(
        temporal_range,
        threshold=stable_threshold,
        v0=255,
        v1=0,
    )

    near_persistent_edges = _expand(persistent_edges, 1)
    hud = core.std.Expr(
        [persistent_edges, near_persistent_edges, stable_pixels],
        expr="x y z min max",
    )

    # Close one-pixel gaps and add only a small margin around detected strokes.
    # Automatic masks must stay tight; broad feathering creates visible islands
    # of sharp background around small crosshairs.
    hud = _expand(hud, 1)
    hud = core.std.Minimum(hud)
    hud = _expand(hud, 1)
    hud = core.std.BoxBlur(
        hud,
        hradius=1,
        vradius=1,
        hpasses=1,
        vpasses=1,
    )
    return core.std.Invert(hud)


def generate(
    clip: vs.VideoNode,
    sensitivity: float = 0.15,
    sample_count: int = 48,
    analysis_width: int = 960,
) -> vs.VideoNode:
    """Return a one-frame protection mask: black HUD, white processed background."""
    sensitivity = min(max(float(sensitivity), 0.0), 1.0)
    sample_count = min(max(int(sample_count), 8), 96)

    width = min(clip.width, analysis_width)
    width -= width % 2
    height = max(2, round((clip.height * width / clip.width) / 2) * 2)

    proxy = core.resize.Bilinear(
        clip,
        width=width,
        height=height,
        format=vs.GRAY8,
    )

    candidates = _sample_indices(proxy.num_frames, sample_count)
    selected = _diverse_indices(proxy, candidates, sample_count)
    minimum_samples = min(sample_count, max(8, sample_count // 4))

    if len(selected) < minimum_samples:
        _eprint(
            f"only {len(selected)} sufficiently different frames were found; "
            "automatic detection was disabled for this clip"
        )
        return core.std.BlankClip(proxy, length=1, color=[255])

    frames = [proxy[index:index + 1] for index in selected]
    protection = _build_protection_mask(frames, sensitivity)
    protected_fraction = 1.0 - float(
        core.std.PlaneStats(protection).get_frame(0).props["PlaneStatsAverage"]
    )

    if protected_fraction > 0.25:
        _eprint(
            f"detection covered {protected_fraction:.1%} of the frame; "
            "automatic detection was disabled as a safety measure"
        )
        return core.std.BlankClip(proxy, length=1, color=[255])

    _eprint(
        f"used {len(selected)} diverse frames and protected "
        f"{protected_fraction:.1%} of the image"
    )
    return protection
