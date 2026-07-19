use crate::cmd::SmCommand;
use crate::verb;
use std::env;
use std::fs;
use std::process::{Command, Stdio};

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

    run_pipeline(cmd, &cmd.ff_args, previewing, progress && !previewing)?;

    if let Some(target) = &cmd.size_target {
        let actual_size = fs::metadata(&cmd.payload.out_path)
            .map_err(|error| format!("Could not inspect rendered output size: {error}"))?
            .len();
        if actual_size > target.max_bytes {
            let corrected = ((target.video_bitrate as f64 * target.max_bytes as f64
                / actual_size as f64)
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
                actual_size,
                target.max_bytes,
                corrected / 1000
            );
            let mut retry_args = cmd.ff_args.clone();
            replace_video_bitrate(&mut retry_args, corrected)?;
            run_pipeline(cmd, &retry_args, previewing, progress && !previewing)?;

            let retry_size = fs::metadata(&cmd.payload.out_path)
                .map_err(|error| format!("Could not inspect retried output size: {error}"))?
                .len();
            if retry_size > target.max_bytes {
                return Err(format!(
                    "Could not keep {} under {} bytes after the safety retry (result: {} bytes)",
                    cmd.payload.out_path.display(),
                    target.max_bytes,
                    retry_size
                ));
            }
        }
    }

    Ok(())
}

fn run_pipeline(
    cmd: &SmCommand,
    ffmpeg_args: &[String],
    previewing: bool,
    progress: bool,
) -> Result<(), String> {
    verb!("FF args: {}", ffmpeg_args.join(" "));
    let mut vs = Command::new(&cmd.vs_path)
        .args(&cmd.vs_args)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Failed to start VSPipe: {error}"))?;
    let pipe = vs
        .stdout
        .take()
        .ok_or_else(|| "Failed piping output from VSPipe".to_owned())?;

    let mut ffmpeg = Command::new(&cmd.ff_path)
        .args(ffmpeg_args)
        .stdin(pipe)
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
        let stderr = ffmpeg
            .stderr
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
    let vspipe_status = vs
        .wait()
        .map_err(|error| format!("Failed waiting for VSPipe: {error}"))?;
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
    use super::{parse_fps, render_command, replace_video_bitrate};
    use crate::cmd::{SizeTarget, SmCommand};
    use crate::recipe::Recipe;
    use crate::video::{ClipSelection, Payload};
    use std::fs;
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

        let generated = Command::new(&ffmpeg)
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x180:rate=30:duration=3",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=3",
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert!(generated.success());
        let probe = ffprobe::ffprobe(&source).unwrap();

        let mut recipe = Recipe::new();
        recipe.insert_value("preview window", "enabled".to_owned(), "no".to_owned());
        recipe.insert_value("frame blending", "enabled".to_owned(), "no".to_owned());
        recipe.insert_value("timescale", "in".to_owned(), "1.0".to_owned());
        recipe.insert_value("timescale", "out".to_owned(), "1.0".to_owned());

        let selection = ClipSelection {
            start_seconds: 0.5,
            end_seconds: 2.5,
            audio_stream_indices: vec![1],
            max_size_bytes: Some(500_000),
        };
        let payload = Payload {
            in_path: source.clone(),
            out_path: output.clone(),
            basename: "synthetic".to_owned(),
            probe,
            timecodes: None,
            selection: Some(selection),
        };
        let source_string = source.display().to_string();
        let output_string = output.display().to_string();
        let ff_args: Vec<String> = [
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
        let vs_args: Vec<String> = [
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
        let target = SizeTarget {
            max_bytes: 500_000,
            video_bitrate: 1_500_000,
        };
        let command = SmCommand {
            vs_path: ffmpeg.display().to_string(),
            vs_args,
            payload,
            ff_path: ffmpeg.display().to_string(),
            recipe,
            ff_args,
            size_target: Some(target.clone()),
            ffplay_path: None,
            ffplay_args: None,
        };

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
}
