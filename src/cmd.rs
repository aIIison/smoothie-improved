use std::env::current_exe;
use which::which;

use crate::cli::Arguments;
use crate::parse::parse_encoding_args;
use crate::recipe::Recipe;
use crate::video::{ClipSelection, Payload};

use crate::verb;
use std::env;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SmCommand {
    pub vs_path: String,
    pub vs_args: Vec<String>,
    pub payload: Payload,
    pub ff_path: String,
    pub recipe: Recipe,
    pub ff_args: Vec<String>,
    pub size_target: Option<SizeTarget>,
    pub ffplay_path: Option<String>,
    pub ffplay_args: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct SizeTarget {
    pub max_bytes: u64,
    pub video_bitrate: u64,
}

pub fn build_commands(args: Arguments, payloads: Vec<Payload>, recipe: Recipe) -> Vec<SmCommand> {
    let executable: String = if args.tompv {
        which("mpv")
            .expect("mpv has not been installed or has not been added to PATH")
            .display()
            .to_string()
    } else {
        let ff_path = recipe.get("output", "process");
        if ff_path == "ffmpeg" {
            which(ff_path)
                .expect("FFmpeg has not been installed or has not been added to PATH")
                .display()
                .to_string()
        } else {
            let is_ffmpeg: bool = ff_path.ends_with("ffmpeg") || ff_path.ends_with("ffmpeg.exe");
            let r#override: bool = env::var("SM_ALLOW_MISC_OUTPUT") == Ok("1".to_owned());

            if !is_ffmpeg && !r#override {
                panic!("You specified an output process which does not have the filename 'ffmpeg', to override this error message please set the environment variable SM_ALLOW_MISC_OUTPUT to 1");
            } else {
                ff_path
            }
        }
    };

    let mut cmd_arguments: Vec<String> = vec![];
    if args.tompv {
        cmd_arguments.push("-".to_string());
    } else {
        cmd_arguments.append(
            &mut recipe
                .get("miscellaneous", "ffmpeg options")
                .split(" ")
                .map(String::from)
                .collect(),
        );
    }

    let enc_args: Vec<String> = parse_encoding_args(&args, &recipe)
        .split(" ")
        .map(String::from)
        .filter(|s| !s.is_empty())
        .collect();

    let cur_exe = current_exe().unwrap();
    let cur_exe_dir = cur_exe.parent().unwrap();
    let vs_bin = if cfg!(target_os = "windows") {
        "vspipe.exe"
    } else {
        "vspipe"
    };
    let bin_dir_vspipe = cur_exe_dir.join(vs_bin);
    let dev_vspipe = cur_exe_dir
        .parent()
        .map(|target_dir| target_dir.join("VapourSynth").join(vs_bin));
    let env_vspipe =
        env::var_os("VAPOURSYNTH_HOME").map(|home| std::path::PathBuf::from(home).join(vs_bin));
    let vspipe_in_path = which("vspipe");
    let vs_path = (if let Some(vspipe_path) = args.vspipe_path {
        vspipe_path
    } else if bin_dir_vspipe.exists() {
        verb!("Using vspipe that's in same directory as binary");
        bin_dir_vspipe
    } else if env_vspipe.as_ref().is_some_and(|path| path.exists()) {
        verb!("Using VSPipe from VAPOURSYNTH_HOME");
        env_vspipe.unwrap()
    } else if dev_vspipe.as_ref().is_some_and(|path| path.exists()) {
        verb!("Using development VSPipe from target/VapourSynth");
        dev_vspipe.unwrap()
    } else if vspipe_in_path.is_ok() {
        verb!("Using VSPipe from PATH");
        vspipe_in_path.unwrap()
    } else {
        panic!(
            "VSPipe was not found. Install the portable VapourSynth bundle at \
             target/VapourSynth, set VAPOURSYNTH_HOME, add vspipe to PATH, or \
             pass --vspipe-path."
        );
    })
    .display()
    .to_string();

    let vpy_path = if args.vpy.exists() {
        args.vpy
    } else if cur_exe_dir.parent().unwrap().join(&args.vpy).exists() {
        cur_exe_dir.parent().unwrap().join(&args.vpy)
    } else {
        panic!(
            "jamba.vpy not found, expected {:?}",
            cur_exe_dir.parent().unwrap().join(&args.vpy)
        );
    };

    /*
        scuffed, but works

        https://github.com/indexmap-rs/indexmap/issues/325

        old one : let rc_string = serde_json::to_string(&recipe).expect("Failed serializing recipe to JSON");
    */
    let rc_string = (format!("{:?}", &recipe)).replace("Recipe { data: {", "{ \"data\": {");

    let vs_args = vec![
        // "--progress".to_owned(),
        "--container".to_owned(),
        "y4m".to_owned(),
        "-".to_owned(),
        vpy_path.display().to_string(),
        "--arg".to_owned(),
        format!("recipe={rc_string:?}"),
    ];

    let mut ret: Vec<SmCommand> = vec![];

    for payload in payloads {
        let mut cur_vs_args = vs_args.clone();
        let wants_render_preview =
            recipe.get_bool("preview window", "enabled") && !args.tompv && args.peek.is_none();
        let resolved_ffplay_path = if wants_render_preview {
            let configured = recipe.get("preview window", "process");
            if configured == "ffplay" {
                match which(&configured) {
                    Ok(path) => Some(path.display().to_string()),
                    Err(_) if payload.selection.is_some() => {
                        eprintln!(
                            "WARNING: FFplay was not found; continuing without audio or render preview."
                        );
                        None
                    }
                    Err(_) => panic!("FFplay (previewer) has not been installed or added to PATH"),
                }
            } else {
                Some(configured)
            }
        } else {
            None
        };

        cur_vs_args.append(&mut vec![
            "--arg".to_owned(),
            format!("input_video={}", payload.in_path.display()),
        ]);
        if let Some(timecodes) = payload.timecodes.clone() {
            let json_timecodes =
                serde_json::to_string(&timecodes).expect("Failed serializing timecodes to JSON");

            cur_vs_args.append(&mut vec![
                "--arg".to_owned(),
                format!("timecodes={json_timecodes:?}"),
            ]);
        }
        if let Some(selection) = &payload.selection {
            cur_vs_args.append(&mut vec![
                "--arg".to_owned(),
                format!("trim_start={:.9}", selection.start_seconds),
                "--arg".to_owned(),
                format!("trim_end={:.9}", selection.end_seconds),
            ]);
        }

        if payload.in_path == payload.out_path {
            panic!("Output path has same path as input")
        }

        let mut cur_cmd_arguments = cmd_arguments.clone();
        let mut size_target = None;

        if args.tompv {
            // nothing to do, but this still needs to step in to break out the if chain
            if let Some(p) = args.peek {
                // duplicate sowwy :33
                cur_vs_args.append(&mut vec![
                    "--start".to_owned(),
                    p.to_string(),
                    "--end".to_owned(),
                    p.to_string(),
                ]);
            }
        } else if args.tonull {
            cur_cmd_arguments.append(&mut vec![
                "-i".to_owned(),
                payload.in_path.display().to_string(),
                "-f".to_owned(),
                "null".to_owned(),
                "NUL".to_owned(),
            ]);
            // cur_cmd_arguments.push(format!(" -i {:?} -f null NUL ", payload.in_path));
        } else if let Some(selection) = &payload.selection {
            if let Some(max_bytes) = selection.max_size_bytes {
                let final_duration = selected_output_duration(selection, &recipe);
                let audio_track_count = if args.stripaudio {
                    0
                } else {
                    selection.audio_stream_indices.len()
                };
                let video_bitrate =
                    calculate_video_bitrate(max_bytes, final_duration, audio_track_count)
                        .unwrap_or_else(|message| panic!("{message}"));
                if args.stripaudio {
                    cur_cmd_arguments.extend([
                        "-map".to_owned(),
                        "0:v".to_owned(),
                        "-an".to_owned(),
                    ]);
                } else {
                    append_selected_audio(
                        &mut cur_cmd_arguments,
                        &payload,
                        selection,
                        &recipe,
                        true,
                    );
                }
                cur_cmd_arguments.append(&mut vec![
                    "-c:v".to_owned(),
                    "libx264".to_owned(),
                    "-preset".to_owned(),
                    "fast".to_owned(),
                    "-b:v".to_owned(),
                    video_bitrate.to_string(),
                    "-maxrate".to_owned(),
                    ((video_bitrate as f64 * 1.25).round() as u64).to_string(),
                    "-bufsize".to_owned(),
                    (video_bitrate * 2).to_string(),
                    "-pix_fmt".to_owned(),
                    "yuv420p".to_owned(),
                    "-movflags".to_owned(),
                    "+faststart".to_owned(),
                    "-y".to_owned(),
                    payload.out_path.display().to_string(),
                ]);
                size_target = Some(SizeTarget {
                    max_bytes,
                    video_bitrate,
                });
            } else if args.stripaudio {
                cur_cmd_arguments.append(&mut enc_args.clone());
                cur_cmd_arguments.push("-an".to_owned());
                cur_cmd_arguments.push(payload.out_path.display().to_string());
            } else {
                append_selected_audio(&mut cur_cmd_arguments, &payload, selection, &recipe, false);
                cur_cmd_arguments.append(&mut enc_args.clone());
                cur_cmd_arguments.push(payload.out_path.display().to_string());
            }

            append_preview_output(
                &mut cur_cmd_arguments,
                &recipe,
                resolved_ffplay_path.is_some(),
            );
        } else {
            if let Some(p) = args.peek {
                cur_vs_args.append(&mut vec![
                    "--start".to_owned(),
                    p.to_string(),
                    "--end".to_owned(),
                    p.to_string(),
                ]);
            } else if args.stripaudio {
                cur_cmd_arguments.append(&mut enc_args.clone());
            } else {
                let mut audio_tracks = 0;
                for stream in &payload.probe.streams {
                    if stream.codec_type == Some("audio".to_owned()) {
                        audio_tracks += 1;
                    }
                }
                let timecodes = recipe.get_option("runtime", "timecodes");

                if audio_tracks > 0 && timecodes.is_some() && timecodes != Some("".to_string()) {
                    let timecodes = timecodes.unwrap();
                    let mut filter_complex = String::new();

                    for track_number in 0..audio_tracks {
                        let mut merge = String::new();
                        let mut iter = 1;
                        for timecode in timecodes.split(";") {
                            let (start, end) = timecode
                                .split_once("-")
                                .expect("runtine timecode split failed");

                            filter_complex.push_str(format!("[1:a:{track_number}]atrim=start={start}:end={end},asetpts=PTS-STARTPTS[a{iter}{track_number}];").as_str());
                            merge.push_str(format!("[a{iter}{track_number}]").as_str());
                            iter += 1;
                        }
                        iter -= 1;
                        filter_complex.push_str(merge.as_str());
                        filter_complex.push_str(
                            format!("concat=n={iter}:v=0:a=1[outa{track_number}];").as_str(),
                        );
                    }

                    cur_cmd_arguments.append(&mut vec![
                        "-i".to_owned(),
                        format!("{}", payload.in_path.display().to_string()),
                        "-filter_complex".to_owned(),
                        filter_complex,
                        "-map".to_owned(),
                        "0:v".to_owned(),
                    ]);

                    for track_number in 0..audio_tracks {
                        cur_cmd_arguments.append(&mut vec![
                            "-map".to_owned(),
                            format!("[outa{track_number}]").to_owned(),
                        ]);
                    }
                } else {
                    cur_cmd_arguments.append(&mut vec![
                        "-i".to_owned(),
                        format!("{}", payload.in_path.display().to_string()),
                        "-map".to_owned(),
                        "0:v".to_owned(),
                        "-map".to_owned(),
                        "1:a?".to_owned(),
                    ]);
                }
            }
            cur_cmd_arguments.append(&mut enc_args.clone());
            cur_cmd_arguments.push(payload.out_path.display().to_string());

            append_preview_output(
                &mut cur_cmd_arguments,
                &recipe,
                resolved_ffplay_path.is_some(),
            );
        }

        let (ffplay_path, ffplay_args) = if let Some(ffplay_path) = resolved_ffplay_path {
            let ffplay_args: Vec<String> = recipe
                .get("miscellaneous", "ffplay options")
                .split(" ")
                .map(String::from)
                .collect();
            (Some(ffplay_path), Some(ffplay_args))
        } else {
            (None, None)
        };
        // dbg!(&cur_cmd_arguments);
        ret.push(SmCommand {
            payload,
            ff_path: executable.clone(),
            ff_args: cur_cmd_arguments,
            size_target,
            recipe: recipe.clone(),
            ffplay_path,
            ffplay_args,
            vs_path: vs_path.clone(),
            vs_args: cur_vs_args.clone(),
        });
    }

    ret
}

fn append_preview_output(arguments: &mut Vec<String>, recipe: &Recipe, available: bool) {
    if recipe.get_bool("preview window", "enabled") && available {
        arguments.extend(
            recipe
                .get("preview window", "output args")
                .split_whitespace()
                .map(String::from),
        );
    }
}

fn append_selected_audio(
    arguments: &mut Vec<String>,
    payload: &Payload,
    selection: &ClipSelection,
    recipe: &Recipe,
    force_aac: bool,
) {
    arguments.push("-i".to_owned());
    arguments.push(payload.in_path.display().to_string());
    arguments.push("-map".to_owned());
    arguments.push("0:v".to_owned());

    if selection.audio_stream_indices.is_empty() {
        arguments.push("-an".to_owned());
        return;
    }

    let speed = audio_speed(recipe);
    let speed_filter = atempo_filters(speed);
    let mut filter = String::new();
    for (output_index, stream_index) in selection.audio_stream_indices.iter().enumerate() {
        filter.push_str(&format!(
            "[1:{stream_index}]atrim=start={:.9}:end={:.9},asetpts=PTS-STARTPTS{}[trim_audio_{output_index}];",
            selection.start_seconds,
            selection.end_seconds,
            speed_filter
        ));
    }
    arguments.push("-filter_complex".to_owned());
    arguments.push(filter);

    for (output_index, stream_index) in selection.audio_stream_indices.iter().enumerate() {
        arguments.push("-map".to_owned());
        arguments.push(format!("[trim_audio_{output_index}]"));

        if let Some(stream) = payload
            .probe
            .streams
            .iter()
            .find(|stream| stream.index == *stream_index)
        {
            if let Some(tags) = &stream.tags {
                if let Some(language) = &tags.language {
                    arguments.push(format!("-metadata:s:a:{output_index}"));
                    arguments.push(format!("language={language}"));
                }
                if let Some(name) = &tags.handler_name {
                    arguments.push(format!("-metadata:s:a:{output_index}"));
                    arguments.push(format!("title={name}"));
                }
            }
        }
    }

    if force_aac {
        arguments.extend([
            "-c:a".to_owned(),
            "aac".to_owned(),
            "-b:a".to_owned(),
            "128k".to_owned(),
        ]);
    }
}

fn audio_speed(recipe: &Recipe) -> f64 {
    let input = recipe
        .get("timescale", "in")
        .parse::<f64>()
        .unwrap_or(1.0)
        .max(0.000_001);
    let output = recipe
        .get("timescale", "out")
        .parse::<f64>()
        .unwrap_or(1.0)
        .max(0.000_001);
    output / input
}

fn atempo_filters(mut speed: f64) -> String {
    if (speed - 1.0).abs() < 0.000_001 {
        return String::new();
    }
    let mut filters = Vec::new();
    while speed > 2.0 {
        filters.push(2.0);
        speed /= 2.0;
    }
    while speed < 0.5 {
        filters.push(0.5);
        speed /= 0.5;
    }
    filters.push(speed);
    filters
        .into_iter()
        .map(|factor| format!(",atempo={factor:.9}"))
        .collect()
}

fn selected_output_duration(selection: &ClipSelection, recipe: &Recipe) -> f64 {
    let source_duration = (selection.end_seconds - selection.start_seconds).max(0.0);
    source_duration / audio_speed(recipe)
}

fn calculate_video_bitrate(
    max_bytes: u64,
    duration: f64,
    audio_tracks: usize,
) -> Result<u64, String> {
    if duration <= 0.0 {
        return Err("The selected clip has no duration".to_owned());
    }
    // One-pass ABR is normally close to its target. Eight percent of headroom
    // covers muxing and rate-control variance, with a verified retry as backup.
    let usable_bits = max_bytes as f64 * 8.0 * 0.92;
    let total_bitrate = usable_bits / duration;
    let reserved_audio = audio_tracks as f64 * 128_000.0;
    let video_bitrate = total_bitrate - reserved_audio - 32_000.0;
    if video_bitrate < 100_000.0 {
        return Err(format!(
            "The requested size is too small for a {:.2}-second clip with {audio_tracks} audio track(s)",
            duration
        ));
    }
    if video_bitrate < 1_000_000.0 {
        eprintln!(
            "WARNING: The size target leaves only {:.0} kbps for video; quality may be poor.",
            video_bitrate / 1000.0
        );
    }
    Ok(video_bitrate.floor() as u64)
}

#[cfg(test)]
fn unique_test_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("smoothie-{}-{nonce}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::{append_selected_audio, atempo_filters, calculate_video_bitrate, unique_test_path};
    use crate::recipe::Recipe;
    use crate::video::{ClipSelection, Payload};
    use std::fs;
    use std::process::Command;

    #[test]
    fn bitrate_reserves_audio_and_headroom() {
        let bitrate = calculate_video_bitrate(50_000_000, 30.0, 2).unwrap();
        assert!(bitrate > 11_500_000);
        assert!(bitrate < 12_500_000);
    }

    #[test]
    fn atempo_supports_extreme_timescales() {
        assert_eq!(atempo_filters(1.0), "");
        assert_eq!(
            atempo_filters(4.0),
            ",atempo=2.000000000,atempo=2.000000000"
        );
        assert_eq!(
            atempo_filters(0.25),
            ",atempo=0.500000000,atempo=0.500000000"
        );
    }

    #[test]
    fn selected_audio_is_trimmed_and_mapped_by_absolute_stream_index() {
        let Ok(ffmpeg) = which::which("ffmpeg") else {
            return;
        };
        let stem = unique_test_path();
        let input = stem.with_extension("input.mkv");
        let output = stem.with_extension("output.mkv");
        let generated = Command::new(&ffmpeg)
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x90:rate=30:duration=3",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=3",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=880:duration=3",
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:a",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-metadata:s:a:1",
                "language=hun",
            ])
            .arg(&input)
            .status()
            .expect("failed to generate synthetic media");
        assert!(generated.success());

        let probe = ffprobe::ffprobe(&input).expect("failed probing synthetic media");
        let selection = ClipSelection {
            start_seconds: 1.0,
            end_seconds: 2.0,
            audio_stream_indices: vec![2],
            max_size_bytes: None,
        };
        let payload = Payload {
            in_path: input.clone(),
            out_path: output.clone(),
            basename: "synthetic".to_owned(),
            probe,
            timecodes: None,
            selection: Some(selection.clone()),
        };
        let mut recipe = Recipe::new();
        recipe.insert_value("timescale", "in".to_owned(), "1.0".to_owned());
        recipe.insert_value("timescale", "out".to_owned(), "1.0".to_owned());

        let mut arguments = vec![
            "-y".to_owned(),
            "-loglevel".to_owned(),
            "error".to_owned(),
            "-f".to_owned(),
            "lavfi".to_owned(),
            "-i".to_owned(),
            "color=c=black:s=160x90:r=30:d=1".to_owned(),
        ];
        append_selected_audio(&mut arguments, &payload, &selection, &recipe, true);
        arguments.extend([
            "-c:v".to_owned(),
            "libx264".to_owned(),
            "-preset".to_owned(),
            "ultrafast".to_owned(),
            "-shortest".to_owned(),
            output.display().to_string(),
        ]);
        let rendered = Command::new(ffmpeg)
            .args(arguments)
            .status()
            .expect("failed rendering selected audio");
        assert!(rendered.success());

        let result = ffprobe::ffprobe(&output).expect("failed probing selected output");
        let audio: Vec<_> = result
            .streams
            .iter()
            .filter(|stream| stream.codec_type.as_deref() == Some("audio"))
            .collect();
        assert_eq!(audio.len(), 1);
        assert_eq!(
            audio[0]
                .tags
                .as_ref()
                .and_then(|tags| tags.language.as_deref()),
            Some("hun")
        );
        let duration = result.format.duration.unwrap().parse::<f64>().unwrap();
        assert!((duration - 1.0).abs() < 0.1);

        let _ = fs::remove_file(input);
        let _ = fs::remove_file(output);
    }
}
