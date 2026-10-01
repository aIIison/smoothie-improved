use crate::cmd::SmCommand;
use crate::verb;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{SystemTime, UNIX_EPOCH};
use which::which;

/// Y4M above this size is not buffered to disk; the retry re-runs VSPipe like
/// before instead of risking a multi-GB temp file on a slow drive.
const MAX_BUFFERED_Y4M_BYTES: u64 = 12 * 1024 * 1024 * 1024;

enum VideoSource {
    /// VSPipe pipes straight into FFmpeg (current behavior).
    Live,
    /// VSPipe output is written to a temp file while being piped into FFmpeg,
    /// so an oversize retry can re-encode from the file without re-running
    /// the (potentially very expensive) VapourSynth pipeline.
    LiveBuffered(PathBuf),
    /// Retry: FFmpeg reads the previously buffered Y4M from disk.
    Buffered(PathBuf),
}

/// Deletes the temp Y4M on drop, success or failure.
struct TempY4m(PathBuf);

impl TempY4m {
    fn new() -> Option<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "smoothie-render-{}-{nonce}.y4m",
            std::process::id()
        ));
        Some(Self(path))
    }

    fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for TempY4m {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn vspipe_render(commands: Vec<SmCommand>, progress: bool) {
    for cmd in commands {
        let result = render_command(&cmd, progress);
        if let Err(error) = result {
            panic!("{error}");
        }
    }
}

fn render_command(cmd: &SmCommand, progress: bool) -> Result<(), String> {
    let previewing = cmd.recipe.get_bool("preview window", "enabled")
        && cmd.ffplay_args.is_some()
        && cmd.ffplay_path.is_some();

    // Trims are pre-cut from the source with a fast near-lossless FFmpeg pass
    // (stream copy with short re-encoded boundaries) before VapourSynth runs,
    // so the source plugin only indexes the short selection instead of the
    // whole file. Falls back to the normal flow automatically when the source
    // cannot be pre-cut cheaply.
    let pre_cut = prepare_pre_cut(cmd);
    let run_cmd = match &pre_cut {
        Some(pre_cut) => &pre_cut.run_cmd,
        None => cmd,
    };

    // For size-targeted renders the retry would otherwise re-run the whole
    // VapourSynth graph. Buffer the Y4M once and reuse it on the retry.
    let temp = run_cmd
        .size_target
        .as_ref()
        .filter(|_| should_buffer_y4m(run_cmd))
        .and_then(|_| TempY4m::new());
    let first_source = match &temp {
        Some(temp) => VideoSource::LiveBuffered(temp.path().clone()),
        None => VideoSource::Live,
    };

    run_pipeline(run_cmd, &run_cmd.ff_args, previewing, progress && !previewing, &first_source)?;

    if let Some(target) = &run_cmd.size_target {
        let actual_size = fs::metadata(&run_cmd.payload.out_path)
            .map_err(|error| format!("Could not inspect rendered output size: {error}"))?
            .len();
        if actual_size > target.max_bytes {
            // Buffered retries only re-encode (VSPipe is not re-run), so a few
            // converging attempts are cheap. Unbuffered renders re-run the
            // whole VapourSynth graph, so they keep a single safety retry.
            let mut attempts_left = if temp.is_some() { 4 } else { 1 };
            let mut current_bitrate = target.video_bitrate;
            let mut current_size = actual_size;
            let mut retry_size = actual_size;
            while attempts_left > 0 {
                attempts_left -= 1;
                // 0.96 leaves a small headroom for muxing and rate-control
                // variance; later attempts use the measured size of the
                // previous one, which converges far better than the initial
                // guess.
                let corrected = ((current_bitrate as f64 * target.max_bytes as f64
                    / current_size as f64)
                    * 0.96)
                    .floor() as u64;
                if corrected < 100_000 {
                    return Err(format!(
                        "The {} byte target cannot be met without dropping video below 100 kbps",
                        target.max_bytes
                    ));
                }
                eprintln!(
                    "Output exceeded the size target ({} > {} bytes); retrying at {} kbps.",
                    current_size,
                    target.max_bytes,
                    corrected / 1000
                );
                let mut retry_args = run_cmd.ff_args.clone();
                replace_video_bitrate(&mut retry_args, corrected)?;
                let retry_source = match &temp {
                    Some(temp) => VideoSource::Buffered(temp.path().clone()),
                    None => VideoSource::Live,
                };
                run_pipeline(run_cmd, &retry_args, previewing, progress && !previewing, &retry_source)?;

                retry_size = fs::metadata(&run_cmd.payload.out_path)
                    .map_err(|error| format!("Could not inspect retried output size: {error}"))?
                    .len();
                if retry_size <= target.max_bytes {
                    break;
                }
                current_bitrate = corrected;
                current_size = retry_size;
            }
            if retry_size > target.max_bytes {
                return Err(format!(
                    "Could not keep {} under {} bytes after the safety retries (result: {} bytes)",
                    run_cmd.payload.out_path.display(),
                    target.max_bytes,
                    retry_size
                ));
            }
        }
    }

    Ok(())
}

fn should_buffer_y4m(cmd: &SmCommand) -> bool {
    estimated_y4m_bytes(cmd).is_some_and(|estimated| estimated <= MAX_BUFFERED_Y4M_BYTES)
}

/// Removes all registered temp files on drop, success or failure.
struct TempFiles(Vec<PathBuf>);

impl TempFiles {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn create(&mut self, index: usize, extension: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "smoothie-cut-{}-{nonce}-{index}.{extension}",
            std::process::id()
        ));
        self.0.push(path.clone());
        path
    }
}

impl Drop for TempFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

enum FastCutSegment {
    /// Stream-copy this range verbatim.
    Copy(f64, f64),
    /// Re-encode this short range so the cut stays frame-accurate.
    Reencode(f64, f64),
}

/// A pre-cut trim plus the command variant that renders from it.
struct PreCut {
    run_cmd: SmCommand,
    // Keeps the pre-cut clip and segment files alive until the render ends.
    _temps: TempFiles,
}

/// Pre-cut a trim: cut the source range into a short temp clip with a fast
/// near-lossless FFmpeg pass (stream copy + re-encoded boundaries), then point
/// VSPipe at that clip instead of the whole source. The source plugin then
/// only indexes the short selection, which is the difference between seconds
/// and minutes for long recordings. Returns None when pre-cutting isn't worth
/// it or the source can't be pre-cut cheaply (the normal flow is used then).
fn prepare_pre_cut(cmd: &SmCommand) -> Option<PreCut> {
    let selection = cmd.payload.selection.as_ref()?;
    if !pre_cut_worthwhile(cmd, selection) {
        return None;
    }
    let mut temps = TempFiles::new();
    let clip = temps.create(0, "mp4");
    if cut_video_range(
        cmd,
        selection.start_seconds,
        selection.end_seconds,
        &clip,
    )
    .is_err()
    {
        return None;
    }
    let mut run_cmd = cmd.clone();
    redirect_vs_input(&mut run_cmd.vs_args, &clip);
    Some(PreCut {
        run_cmd,
        _temps: temps,
    })
}

/// Pre-cutting only pays off when the selection is a small slice of a large
/// source; otherwise indexing the source directly is just as cheap.
fn pre_cut_worthwhile(cmd: &SmCommand, selection: &crate::video::ClipSelection) -> bool {
    let source_duration = cmd
        .payload
        .probe
        .format
        .duration
        .as_deref()
        .and_then(|duration| duration.parse::<f64>().ok())
        .unwrap_or(0.0);
    let selected = selection.end_seconds - selection.start_seconds;
    source_duration >= 30.0 && selected <= source_duration * 0.95
}

/// Replace VSPipe's `input_video` argument with the pre-cut clip and drop the
/// trim arguments (the clip already spans exactly the selection).
fn redirect_vs_input(vs_args: &mut Vec<String>, clip: &Path) {
    let mut index = 0;
    while index < vs_args.len() {
        if vs_args[index] == "--arg" && index + 1 < vs_args.len() {
            let value = &vs_args[index + 1];
            if value.starts_with("input_video=") {
                vs_args[index + 1] = format!("input_video={}", clip.display());
            } else if value.starts_with("trim_start=") || value.starts_with("trim_end=") {
                vs_args.remove(index + 1);
                vs_args.remove(index);
                continue;
            }
        }
        index += 1;
    }
}

/// Writes a video-only, frame-accurate cut of [start, end] to `output`: the
/// bulk is stream-copied GOPs and only the few frames between the selection
/// edges and the neighbouring keyframes are re-encoded (LosslessCut style).
/// Returns Err when the source can't be pre-cut cheaply.
fn cut_video_range(cmd: &SmCommand, start: f64, end: f64, output: &Path) -> Result<(), String> {
    let start = start.max(0.0);
    let end = end.max(start + 1e-9);
    let input = &cmd.payload.in_path;
    let container = output
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "mp4".to_owned());
    let video_index = cmd
        .payload
        .probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"))
        .map(|stream| stream.index)
        .ok_or_else(|| "Source has no video stream".to_owned())?;

    let encoder_args = boundary_encoder_args(cmd)?;
    let keyframes = probe_keyframes(input)?;
    // The first keyframe at or after the start and the last keyframe at or
    // before the end split the range into: a short re-encoded head, a
    // stream-copied middle of complete GOPs, and a short re-encoded tail.
    let kf1 = keyframes.iter().copied().find(|frame| *frame >= start - 1e-6);
    let kf2 = keyframes.iter().copied().rev().find(|frame| *frame <= end + 1e-6);
    let mut segments: Vec<FastCutSegment> = Vec::new();
    if let (Some(kf1), Some(kf2)) = (kf1, kf2) {
        if kf2 > kf1 + 1e-6 {
            if kf1 > start + 1e-6 {
                segments.push(FastCutSegment::Reencode(start, kf1));
            }
            segments.push(FastCutSegment::Copy(kf1, kf2));
            if end > kf2 + 1e-6 {
                segments.push(FastCutSegment::Reencode(kf2, end));
            }
        } else {
            // The whole range fits inside one GOP.
            segments.push(FastCutSegment::Reencode(start, end));
        }
    } else {
        // No usable keyframes (or none at all); just re-encode the range.
        segments.push(FastCutSegment::Reencode(start, end));
    }

    let ffmpeg = PathBuf::from(&cmd.ff_path);
    let mut temps = TempFiles::new();
    let mut segment_paths = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        let (segment_start, segment_end) = match *segment {
            FastCutSegment::Copy(a, b) => (a, b),
            FastCutSegment::Reencode(a, b) => (a, b),
        };
        let path = temps.create(index, &container);
        let mut args = vec![
            "-y".to_owned(),
            "-loglevel".to_owned(),
            "error".to_owned(),
            "-ss".to_owned(),
            format_seconds(segment_start),
            "-i".to_owned(),
            input.display().to_string(),
            "-t".to_owned(),
            format_seconds(segment_end - segment_start),
            "-map".to_owned(),
            format!("0:{video_index}"),
            "-an".to_owned(),
        ];
        match *segment {
            FastCutSegment::Copy(..) => {
                args.extend(["-c".to_owned(), "copy".to_owned()]);
            }
            FastCutSegment::Reencode(..) => {
                args.extend(encoder_args.clone());
            }
        }
        args.push(path.display().to_string());
        run_ffmpeg(&ffmpeg, &args)?;
        segment_paths.push(path);
    }

    let mut mux_args = vec![
        "-y".to_owned(),
        "-loglevel".to_owned(),
        "error".to_owned(),
        "-f".to_owned(),
        "concat".to_owned(),
        "-safe".to_owned(),
        "0".to_owned(),
        "-i".to_owned(),
    ];
    let list_path = temps.create(100, "txt");
    let list_content = segment_paths
        .iter()
        .map(|path| {
            format!(
                "file '{}'\n",
                path.display().to_string().replace('\\', "/")
            )
        })
        .collect::<String>();
    fs::write(&list_path, list_content)
        .map_err(|error| format!("Could not write concat list: {error}"))?;
    mux_args.push(list_path.display().to_string());
    mux_args.extend([
        "-map".to_owned(),
        "0:v".to_owned(),
        "-c:v".to_owned(),
        "copy".to_owned(),
    ]);
    if matches!(container.as_str(), "mp4" | "mov" | "m4v") {
        mux_args.extend(["-movflags".to_owned(), "+faststart".to_owned()]);
    }
    mux_args.push(output.display().to_string());
    run_ffmpeg(&ffmpeg, &mux_args)
}

/// Timestamps of all keyframes of the first video stream, in seconds.
fn probe_keyframes(input: &Path) -> Result<Vec<f64>, String> {
    let ffprobe = which("ffprobe").map_err(|_| "FFprobe was not found in PATH".to_owned())?;
    let output = Command::new(&ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-skip_frame",
            "nokey",
            "-show_frames",
            "-show_entries",
            "frame=best_effort_timestamp_time",
            "-of",
            "csv=p=0",
        ])
        .arg(input)
        .output()
        .map_err(|error| format!("Failed to probe keyframes: {error}"))?;
    if !output.status.success() {
        return Err("Failed to probe keyframes".to_owned());
    }
    let mut keyframes = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Ok(time) = line.trim().parse::<f64>() {
            if time >= 0.0 {
                keyframes.push(time);
            }
        }
    }
    Ok(keyframes)
}

/// Encoder settings that re-encode short boundary segments into the same
/// codec/frame layout as the source, so the concat demuxer can join them with
/// the stream-copied middle. Err means "cannot match cheaply" (exotic codec,
/// pixel format, rotation or non-square pixels), in which case the caller
/// skips the pre-cut and uses the normal pipeline.
fn boundary_encoder_args(cmd: &SmCommand) -> Result<Vec<String>, String> {
    let stream = cmd
        .payload
        .probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"))
        .ok_or_else(|| "Source has no video stream".to_owned())?;
    // Rotation and non-square pixels would make the re-encoded boundaries
    // display differently from the copied middle.
    if stream
        .side_data_list
        .iter()
        .any(|side_data| side_data.side_data_type.contains("displaymatrix"))
    {
        return Err("rotated sources cannot be pre-cut".to_owned());
    }
    if let Some(sar) = stream.sample_aspect_ratio.as_deref() {
        if sar != "1:1" {
            return Err("sources with non-square pixels cannot be pre-cut".to_owned());
        }
    }
    let pix_fmt = stream.pix_fmt.as_deref().unwrap_or("yuv420p");
    let (encoder, pix_fmt, crf) = match stream.codec_name.as_deref() {
        Some("h264") if pix_fmt == "yuv420p" || pix_fmt == "yuvj420p" => {
            (Some("libx264"), pix_fmt, "16")
        }
        Some("hevc") | Some("h265") => {
            let pix = if pix_fmt == "yuv420p10le" {
                "yuv420p10le"
            } else {
                "yuv420p"
            };
            (Some("libx265"), pix, "18")
        }
        _ => (None, "", ""),
    };
    let Some(encoder) = encoder else {
        return Err("cannot pre-cut this codec".to_owned());
    };

    let fps = parse_fps(&stream.avg_frame_rate).unwrap_or(30.0);
    let mut args = vec![
        "-c:v".to_owned(),
        encoder.to_owned(),
        "-preset".to_owned(),
        "veryfast".to_owned(),
        "-crf".to_owned(),
        crf.to_owned(),
        "-pix_fmt".to_owned(),
        pix_fmt.to_owned(),
        "-r".to_owned(),
        format!("{fps:.6}"),
    ];
    if let Some(color_range) = stream.color_range.as_deref() {
        args.extend(["-color_range".to_owned(), color_range.to_owned()]);
    }
    if let Some(color_space) = stream.color_space.as_deref() {
        args.extend(["-colorspace".to_owned(), color_space.to_owned()]);
    }
    Ok(args)
}

fn run_ffmpeg(ffmpeg: &Path, args: &[String]) -> Result<(), String> {
    let status = Command::new(ffmpeg)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .map_err(|error| format!("Failed to start FFmpeg: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("FFmpeg failed ({status}) with: {}", args.join(" ")))
    }
}

fn format_seconds(value: f64) -> String {
    format!("{value:.6}")
}

/// Rough Y4M size for the buffering decision. Uses two bytes per pixel to
/// cover 10-bit sources; being conservative here only changes whether the
/// retry re-runs VSPipe or reuses the buffer.
fn estimated_y4m_bytes(cmd: &SmCommand) -> Option<u64> {
    let stream = cmd
        .payload
        .probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"))?;
    let width = stream.width? as u64;
    let height = stream.height? as u64;
    let frames = (selected_duration_seconds(cmd) * output_fps(cmd) as f64).ceil() as u64;
    Some(frames.saturating_mul(width.saturating_mul(height).saturating_mul(2)))
}

fn run_pipeline(
    cmd: &SmCommand,
    ffmpeg_args: &[String],
    previewing: bool,
    progress: bool,
    source: &VideoSource,
) -> Result<(), String> {
    verb!("FF args: {}", ffmpeg_args.join(" "));

    let mut vs: Option<Child> = match source {
        VideoSource::Buffered(_) => None,
        _ => Some(
            Command::new(&cmd.vs_path)
                .args(&cmd.vs_args)
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|error| format!("Failed to start VSPipe: {error}"))?,
        ),
    };

    let stdin = match source {
        VideoSource::Buffered(path) => {
            let file = fs::File::open(path)
                .map_err(|error| format!("Could not open buffered frames: {error}"))?;
            Stdio::from(file)
        }
        VideoSource::Live => {
            let pipe = vs
                .as_mut()
                .and_then(|vs| vs.stdout.take())
                .ok_or_else(|| "Failed piping output from VSPipe".to_owned())?;
            Stdio::from(pipe)
        }
        VideoSource::LiveBuffered(_) => Stdio::piped(),
    };

    let mut ffmpeg = Command::new(&cmd.ff_path)
        .args(ffmpeg_args)
        .stdin(stdin)
        .stdout(if previewing {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(if progress {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .spawn()
        .map_err(|error| format!("Failed to start FFmpeg: {error}"))?;
    let mut ffmpeg_stderr = ffmpeg.stderr.take();

    if let VideoSource::LiveBuffered(path) = source {
        let mut vs_stdout = vs
            .as_mut()
            .and_then(|vs| vs.stdout.take())
            .ok_or_else(|| "Failed piping output from VSPipe".to_owned())?;
        let mut ffmpeg_stdin = ffmpeg
            .stdin
            .take()
            .ok_or_else(|| "Failed piping frames into FFmpeg".to_owned())?;
        let mut buffer_file = fs::File::create(path)
            .map_err(|error| format!("Could not create frame buffer: {error}"))?;
        let buffering_failed = Arc::new(AtomicBool::new(false));
        let buffering_failed_clone = Arc::clone(&buffering_failed);
        std::thread::spawn(move || {
            let mut buffer = vec![0_u8; 256 * 1024];
            loop {
                match vs_stdout.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = &buffer[..n];
                        if buffer_file.write_all(chunk).is_err() {
                            buffering_failed_clone.store(true, Ordering::Relaxed);
                            break;
                        }
                        if ffmpeg_stdin.write_all(chunk).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let pipeline_result = finish_pipeline(
            cmd,
            &mut vs,
            &mut ffmpeg,
            previewing,
            progress,
            ffmpeg_stderr.take(),
        );
        // If the pipeline errored before both processes finished (e.g. FFplay
        // failed to start), the detached pump thread would otherwise block on
        // VSPipe's still-open stdout forever. Kill both so the pump hits EOF.
        if pipeline_result.is_err() {
            if let Some(vs) = vs.as_mut() {
                let _ = vs.kill();
            }
            let _ = ffmpeg.kill();
        }
        if buffering_failed.load(Ordering::Relaxed) {
            let _ = fs::remove_file(&cmd.payload.out_path);
            return Err(
                "Failed to buffer frames for the size-target retry (is the temp disk full?)"
                    .to_owned(),
            );
        }
        return pipeline_result;
    }

    finish_pipeline(
        cmd,
        &mut vs,
        &mut ffmpeg,
        previewing,
        progress,
        ffmpeg_stderr,
    )
}

fn finish_pipeline(
    cmd: &SmCommand,
    vs: &mut Option<Child>,
    ffmpeg: &mut Child,
    previewing: bool,
    progress: bool,
    mut stderr: Option<std::process::ChildStderr>,
) -> Result<(), String> {
    let mut ffplay = if previewing {
        let ffplay_pipe = ffmpeg
            .stdout
            .take()
            .ok_or_else(|| "Failed piping preview output from FFmpeg".to_owned())?;
        Some(
            Command::new(cmd.ffplay_path.as_ref().unwrap())
                .args(cmd.ffplay_args.as_ref().unwrap())
                .stdin(ffplay_pipe)
                .spawn()
                .map_err(|error| format!("Failed to start FFplay: {error}"))?,
        )
    } else {
        None
    };

    if progress {
        let stderr = stderr
            .take()
            .ok_or_else(|| "Failed to capture FFmpeg progress".to_owned())?;
        let duration = selected_duration_seconds(cmd).ceil().max(1.0) as usize;
        let fps = output_fps(cmd);
        crate::ffpb::ffmpeg(stderr, duration, Some(fps))
            .map_err(|error| format!("Failed reading FFmpeg progress: {error}"))?;
    }

    let ffmpeg_status = ffmpeg
        .wait()
        .map_err(|error| format!("Failed waiting for FFmpeg: {error}"))?;
    let vspipe_status = match vs {
        Some(vs) => vs
            .wait()
            .map_err(|error| format!("Failed waiting for VSPipe: {error}"))?,
        None => ffmpeg_status,
    };
    if let Some(player) = ffplay.as_mut() {
        let _ = player.wait();
    }

    if !ffmpeg_status.success() || !vspipe_status.success() {
        return Err(format!(
            "FFmpeg/VapourSynth failed while rendering {} (FFmpeg: {}, VSPipe: {})",
            cmd.payload.in_path.display(),
            ffmpeg_status,
            vspipe_status
        ));
    }
    Ok(())
}

fn selected_duration_seconds(cmd: &SmCommand) -> f64 {
    if let Some(selection) = &cmd.payload.selection {
        let input_scale = cmd
            .recipe
            .get("timescale", "in")
            .parse::<f64>()
            .unwrap_or(1.0);
        let output_scale = cmd
            .recipe
            .get("timescale", "out")
            .parse::<f64>()
            .unwrap_or(1.0);
        return (selection.end_seconds - selection.start_seconds) * input_scale / output_scale;
    }
    cmd.payload
        .probe
        .format
        .duration
        .as_deref()
        .and_then(|duration| duration.parse().ok())
        .unwrap_or(1.0)
}

fn output_fps(cmd: &SmCommand) -> i32 {
    if cmd.recipe.get_bool("frame blending", "enabled") {
        return cmd
            .recipe
            .get("frame blending", "fps")
            .parse::<i32>()
            .unwrap_or(60);
    }
    cmd.payload
        .probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"))
        .and_then(|stream| parse_fps(&stream.avg_frame_rate))
        .unwrap_or(30.0)
        .round()
        .max(1.0) as i32
}

fn parse_fps(value: &str) -> Option<f64> {
    if let Some((numerator, denominator)) = value.split_once('/') {
        let numerator = numerator.parse::<f64>().ok()?;
        let denominator = denominator.parse::<f64>().ok()?;
        (denominator != 0.0).then_some(numerator / denominator)
    } else {
        value.parse().ok()
    }
}

fn replace_video_bitrate(arguments: &mut [String], bitrate: u64) -> Result<(), String> {
    replace_option(arguments, "-b:v", bitrate)?;
    if arguments.iter().any(|argument| argument == "-maxrate") {
        replace_option(
            arguments,
            "-maxrate",
            (bitrate as f64 * 1.25).round() as u64,
        )?;
    }
    if arguments.iter().any(|argument| argument == "-bufsize") {
        replace_option(arguments, "-bufsize", bitrate * 2)?;
    }
    Ok(())
}

fn replace_option(arguments: &mut [String], option: &str, value: u64) -> Result<(), String> {
    let index = arguments
        .iter()
        .position(|argument| argument == option)
        .ok_or_else(|| format!("Target-size command did not contain {option}"))?;
    let argument = arguments
        .get_mut(index + 1)
        .ok_or_else(|| format!("Target-size command had an incomplete {option}"))?;
    *argument = value.to_string();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        cut_video_range, parse_fps, prepare_pre_cut, redirect_vs_input, render_command,
        replace_video_bitrate,
    };
    use crate::cmd::{SizeTarget, SmCommand};
    use crate::recipe::Recipe;
    use crate::video::{ClipSelection, Payload};
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn rational_fps_is_parsed() {
        assert!((parse_fps("60000/1001").unwrap() - 59.94005994).abs() < 0.0001);
    }

    #[test]
    fn retry_replaces_bitrate() {
        let mut args = vec![
            "-b:v".to_owned(),
            "5000000".to_owned(),
            "-maxrate".to_owned(),
            "6250000".to_owned(),
            "-bufsize".to_owned(),
            "10000000".to_owned(),
        ];
        replace_video_bitrate(&mut args, 4_000_000).unwrap();
        assert_eq!(args[1], "4000000");
        assert_eq!(args[3], "5000000");
        assert_eq!(args[5], "8000000");
    }

    /// Generates the synthetic source used by the render tests.
    fn generate_source(ffmpeg: &std::path::Path, path: &std::path::Path, seconds: u32) {
        let generated = Command::new(ffmpeg)
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc2=size=320x180:rate=30:duration={seconds}"),
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:duration={seconds}"),
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-g",
                "30",
                "-c:a",
                "aac",
            ])
            .arg(path)
            .status()
            .unwrap();
        assert!(generated.success());
    }

    fn base_command(
        ffmpeg: &std::path::Path,
        source: &std::path::Path,
        output: &std::path::Path,
        selection: Option<ClipSelection>,
    ) -> SmCommand {
        let probe = ffprobe::ffprobe(source).unwrap();
        let mut recipe = Recipe::new();
        recipe.insert_value("preview window", "enabled".to_owned(), "no".to_owned());
        recipe.insert_value("frame blending", "enabled".to_owned(), "no".to_owned());
        recipe.insert_value("timescale", "in".to_owned(), "1.0".to_owned());
        recipe.insert_value("timescale", "out".to_owned(), "1.0".to_owned());
        SmCommand {
            vs_path: ffmpeg.display().to_string(),
            vs_args: vec![],
            payload: Payload {
                in_path: source.to_path_buf(),
                out_path: output.to_path_buf(),
                basename: "synthetic".to_owned(),
                probe,
                timecodes: None,
                selection,
            },
            ff_path: ffmpeg.display().to_string(),
            recipe,
            ff_args: vec![],
            size_target: None,
            ffplay_path: None,
            ffplay_args: None,
        }
    }

    #[test]
    fn verified_size_target_render_stays_under_requested_size() {
        let Ok(ffmpeg) = which::which("ffmpeg") else {
            return;
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("smoothie-render-test-{nonce}"));
        let source = base.with_extension("source.mkv");
        let output = base.with_extension("output.mp4");
        generate_source(&ffmpeg, &source, 3);

        let selection = ClipSelection {
            start_seconds: 0.5,
            end_seconds: 2.5,
            audio_stream_indices: vec![1],
            max_size_bytes: Some(500_000),
        };
        let mut command = base_command(&ffmpeg, &source, &output, Some(selection));
        let source_string = source.display().to_string();
        let output_string = output.display().to_string();
        command.ff_args = [
            "-loglevel",
            "error",
            "-i",
            "-",
            "-i",
            &source_string,
            "-filter_complex",
            "[1:1]atrim=start=0.5:end=2.5,asetpts=PTS-STARTPTS[a]",
            "-map",
            "0:v",
            "-map",
            "[a]",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-b:v",
            "1500000",
            "-maxrate",
            "1875000",
            "-bufsize",
            "3000000",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-pix_fmt",
            "yuv420p",
            "-y",
            &output_string,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        command.vs_args = [
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=30:duration=2",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "yuv4mpegpipe",
            "-",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        command.size_target = Some(SizeTarget {
            max_bytes: 500_000,
            video_bitrate: 1_500_000,
        });

        let result = render_command(&command, false);
        assert!(result.is_ok(), "{}", result.unwrap_err());
        assert!(fs::metadata(&output).unwrap().len() <= 500_000);
        let rendered = ffprobe::ffprobe(&output).unwrap();
        assert_eq!(
            rendered
                .streams
                .iter()
                .filter(|stream| stream.codec_type.as_deref() == Some("audio"))
                .count(),
            1
        );

        let _ = fs::remove_file(source);
        let _ = fs::remove_file(output);
    }

    #[test]
    fn oversize_retry_renders_from_buffered_frames() {
        let Ok(ffmpeg) = which::which("ffmpeg") else {
            return;
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("smoothie-retry-test-{nonce}"));
        let source = base.with_extension("source.mkv");
        let output = base.with_extension("output.mp4");
        generate_source(&ffmpeg, &source, 3);

        // 1.5 Mbps over 2 seconds is ~408 KB of video plus 32 KB of AAC audio;
        // the 150 KB target forces the first pass to overshoot and the
        // adaptive buffered retry to converge over a couple of attempts.
        let max_bytes = 150_000;
        let selection = ClipSelection {
            start_seconds: 0.5,
            end_seconds: 2.5,
            audio_stream_indices: vec![1],
            max_size_bytes: Some(max_bytes),
        };
        let mut command = base_command(&ffmpeg, &source, &output, Some(selection));
        let source_string = source.display().to_string();
        let output_string = output.display().to_string();
        command.ff_args = [
            "-loglevel",
            "error",
            "-i",
            "-",
            "-i",
            &source_string,
            "-filter_complex",
            "[1:1]atrim=start=0.5:end=2.5,asetpts=PTS-STARTPTS[a]",
            "-map",
            "0:v",
            "-map",
            "[a]",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-b:v",
            "1500000",
            "-maxrate",
            "1875000",
            "-bufsize",
            "3000000",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-pix_fmt",
            "yuv420p",
            "-y",
            &output_string,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        command.vs_args = [
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=30:duration=2",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "yuv4mpegpipe",
            "-",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        command.size_target = Some(SizeTarget {
            max_bytes,
            video_bitrate: 1_500_000,
        });

        let result = render_command(&command, false);
        assert!(result.is_ok(), "{}", result.unwrap_err());
        let final_size = fs::metadata(&output).unwrap().len();
        assert!(
            final_size <= max_bytes,
            "retry left the file at {final_size} bytes, target {max_bytes}"
        );
        let rendered = ffprobe::ffprobe(&output).unwrap();
        assert_eq!(
            rendered
                .streams
                .iter()
                .filter(|stream| stream.codec_type.as_deref() == Some("video"))
                .count(),
            1
        );
        assert_eq!(
            rendered
                .streams
                .iter()
                .filter(|stream| stream.codec_type.as_deref() == Some("audio"))
                .count(),
            1
        );

        let _ = fs::remove_file(source);
        let _ = fs::remove_file(output);
    }

    #[test]
    fn pre_cut_mid_gop_is_frame_accurate() {
        let Ok(ffmpeg) = which::which("ffmpeg") else {
            return;
        };
        let Ok(ffprobe) = which::which("ffprobe") else {
            return;
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("smoothie-fastcut-{nonce}"));
        let source = base.with_extension("source.mp4");
        let cut = base.with_extension("cut.mp4");
        generate_source(&ffmpeg, &source, 6);
        let command = base_command(&ffmpeg, &source, &cut, None);

        // 2.5-4.5 lies mid-GOP at both ends (keyframes every second), so the
        // head and tail must be re-encoded while [3,4) is stream-copied.
        let result = cut_video_range(&command, 2.5, 4.5, &cut);
        assert!(result.is_ok(), "{}", result.unwrap_err());
        let rendered = ffprobe::ffprobe(&cut).unwrap();
        let video = rendered
            .streams
            .iter()
            .find(|stream| stream.codec_type.as_deref() == Some("video"))
            .unwrap();
        assert_eq!(video.width, Some(320));
        assert_eq!(video.height, Some(180));
        let duration = rendered.format.duration.unwrap().parse::<f64>().unwrap();
        assert!(
            (duration - 2.0).abs() < 0.1,
            "expected ~2.0s, got {duration}s"
        );
        // Decodes without errors and produces exactly 60 frames.
        let cut_arg = cut.display().to_string();
        let decoded = Command::new(&ffmpeg)
            .args(["-v", "error", "-i", &cut_arg, "-f", "null", "-"])
            .output()
            .unwrap();
        assert!(decoded.status.success());
        let frame_count = Command::new(&ffprobe)
            .args([
                "-v",
                "error",
                "-count_frames",
                "-select_streams",
                "v",
                "-show_entries",
                "stream=nb_read_frames",
                "-of",
                "csv=p=0",
                &cut_arg,
            ])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&frame_count.stdout).trim(), "60");

        let _ = fs::remove_file(source);
        let _ = fs::remove_file(cut);
    }

    #[test]
    fn pre_cut_on_keyframes_is_pixel_identical() {
        let Ok(ffmpeg) = which::which("ffmpeg") else {
            return;
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("smoothie-fastcut-key-{nonce}"));
        let source = base.with_extension("source.mp4");
        let cut = base.with_extension("cut.mp4");
        generate_source(&ffmpeg, &source, 6);
        let command = base_command(&ffmpeg, &source, &cut, None);

        // Both edges land on keyframes, so the entire range is stream-copied
        // and the output must be pixel-identical to the source frames.
        let result = cut_video_range(&command, 3.0, 5.0, &cut);
        assert!(result.is_ok(), "{}", result.unwrap_err());

        let cut_raw = base.with_extension("cut.raw");
        let source_raw = base.with_extension("source.raw");
        let cut_arg = cut.display().to_string();
        let source_arg = source.display().to_string();
        let cut_raw_arg = cut_raw.display().to_string();
        let source_raw_arg = source_raw.display().to_string();
        let cut_ok = Command::new(&ffmpeg)
            .args([
                "-loglevel",
                "error",
                "-i",
                &cut_arg,
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                &cut_raw_arg,
            ])
            .status()
            .unwrap();
        let source_ok = Command::new(&ffmpeg)
            .args([
                "-loglevel",
                "error",
                "-ss",
                "3.0",
                "-i",
                &source_arg,
                "-t",
                "2.0",
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                &source_raw_arg,
            ])
            .status()
            .unwrap();
        assert!(cut_ok.success() && source_ok.success());
        let cut_bytes = fs::read(&cut_raw).unwrap();
        let source_frames = fs::read(&source_raw).unwrap();
        assert_eq!(cut_bytes.len(), source_frames.len());
        assert_eq!(cut_bytes, source_frames, "copied frames differ from the source");

        let _ = fs::remove_file(source);
        let _ = fs::remove_file(cut);
        let _ = fs::remove_file(cut_raw);
        let _ = fs::remove_file(source_raw);
    }

    #[test]
    fn redirect_vs_input_points_at_pre_cut_and_drops_trims() {
        let mut args = vec![
            "--container".to_owned(),
            "y4m".to_owned(),
            "-".to_owned(),
            "jamba.vpy".to_owned(),
            "--arg".to_owned(),
            "recipe={}".to_owned(),
            "--arg".to_owned(),
            "input_video=C:\\videos\\source.mp4".to_owned(),
            "--arg".to_owned(),
            "trim_start=1.5".to_owned(),
            "--arg".to_owned(),
            "trim_end=3.5".to_owned(),
        ];
        redirect_vs_input(&mut args, std::path::Path::new("C:\\temp\\cut.mp4"));
        assert_eq!(
            args,
            vec![
                "--container".to_owned(),
                "y4m".to_owned(),
                "-".to_owned(),
                "jamba.vpy".to_owned(),
                "--arg".to_owned(),
                "recipe={}".to_owned(),
                "--arg".to_owned(),
                "input_video=C:\\temp\\cut.mp4".to_owned(),
            ]
        );
    }

    #[test]
    fn pre_cut_trim_renders_through_real_vspipe() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let vspipe = root.join("target").join("VapourSynth").join("VSPipe.exe");
        let vpy = root.join("target").join("jamba.vpy");
        let Ok(ffmpeg) = which::which("ffmpeg") else {
            return;
        };
        let Ok(ffprobe) = which::which("ffprobe") else {
            return;
        };
        if !vspipe.exists() || !vpy.exists() {
            return;
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("smoothie-vspipe-{nonce}"));
        let source = base.with_extension("source.mp4");
        let output = base.with_extension("output.mp4");
        generate_source(&ffmpeg, &source, 40);

        let probe = ffprobe::ffprobe(&source).unwrap();
        let mut recipe = Recipe::new();
        crate::recipe::parse_recipe(
            root.join("target").join("defaults.ini"),
            None,
            &mut recipe,
            &mut None,
            false,
        );
        for (section, key) in [
            ("preview window", "enabled"),
            ("interpolation", "enabled"),
            ("pre-interp", "enabled"),
            ("frame blending", "enabled"),
            ("flowblur", "enabled"),
            ("artifact masking", "enabled"),
            ("color grading", "enabled"),
            ("lut", "enabled"),
        ] {
            recipe.insert_value(section, key.to_owned(), "no".to_owned());
        }
        recipe.insert_value("miscellaneous", "dedup threshold".to_owned(), "no".to_owned());
        recipe.insert_value("miscellaneous", "source plugin".to_owned(), "bestsource".to_owned());
        recipe.insert_value("miscellaneous", "play ding".to_owned(), "no".to_owned());

        let rc_string = (format!("{:?}", &recipe)).replace("Recipe { data: {", "{ \"data\": {");
        let source_string = source.display().to_string();
        let output_string = output.display().to_string();
        let vs_args: Vec<String> = [
            "--container",
            "y4m",
            "-",
            &vpy.display().to_string(),
            "--arg",
            &format!("recipe={rc_string:?}"),
            "--arg",
            &format!("input_video={source_string}"),
            "--arg",
            "trim_start=5.000000000",
            "--arg",
            "trim_end=10.000000000",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let ff_args: Vec<String> = [
            "-loglevel",
            "error",
            "-i",
            "-",
            "-i",
            &source_string,
            "-filter_complex",
            "[1:1]atrim=start=5:end=10,asetpts=PTS-STARTPTS[a]",
            "-map",
            "0:v",
            "-map",
            "[a]",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "18",
            "-c:a",
            "aac",
            "-pix_fmt",
            "yuv420p",
            "-y",
            &output_string,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let selection = ClipSelection {
            start_seconds: 5.0,
            end_seconds: 10.0,
            audio_stream_indices: vec![1],
            max_size_bytes: None,
        };
        let payload = Payload {
            in_path: source.clone(),
            out_path: output.clone(),
            basename: "synthetic".to_owned(),
            probe,
            timecodes: None,
            selection: Some(selection),
        };
        let command = SmCommand {
            vs_path: vspipe.display().to_string(),
            vs_args,
            payload,
            ff_path: ffmpeg.display().to_string(),
            recipe,
            ff_args,
            size_target: None,
            ffplay_path: None,
            ffplay_args: None,
        };

        let result = render_command(&command, false);
        assert!(result.is_ok(), "{}", result.unwrap_err());
        let rendered = ffprobe::ffprobe(&output).unwrap();
        let duration = rendered.format.duration.unwrap().parse::<f64>().unwrap();
        assert!(
            (duration - 5.0).abs() < 0.15,
            "expected ~5.0s, got {duration}s"
        );
        let frame_count = Command::new(&ffprobe)
            .args([
                "-v",
                "error",
                "-count_frames",
                "-select_streams",
                "v",
                "-show_entries",
                "stream=nb_read_frames",
                "-of",
                "csv=p=0",
                &output_string,
            ])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&frame_count.stdout).trim(), "150");
        assert_eq!(
            rendered
                .streams
                .iter()
                .filter(|stream| stream.codec_type.as_deref() == Some("audio"))
                .count(),
            1
        );

        // The render must have gone through the pre-cut: the temp clip exists,
        // spans exactly the selection, and VSPipe's input points at it with
        // the trim arguments stripped.
        let pre_cut = prepare_pre_cut(&command).expect("pre-cut should have applied");
        let input_video = pre_cut
            .run_cmd
            .vs_args
            .windows(2)
            .find_map(|pair| {
                (pair[0] == "--arg")
                    .then(|| pair[1].strip_prefix("input_video=").map(str::to_owned))
                    .flatten()
            })
            .expect("input_video arg missing");
        assert!(
            !pre_cut
                .run_cmd
                .vs_args
                .iter()
                .any(|arg| arg.starts_with("trim_")),
            "trim args should be stripped after the pre-cut"
        );
        assert_ne!(input_video, source_string);
        let clip_probe = ffprobe::ffprobe(std::path::Path::new(&input_video)).unwrap();
        let clip_duration = clip_probe
            .format
            .duration
            .unwrap()
            .parse::<f64>()
            .unwrap();
        assert!(
            (clip_duration - 5.0).abs() < 0.15,
            "pre-cut clip should be ~5.0s, got {clip_duration}s"
        );

        let _ = fs::remove_file(source);
        let _ = fs::remove_file(output);
    }
}

