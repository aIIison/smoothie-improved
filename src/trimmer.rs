use crate::video::{probe_video, ClipSelection, GuiClipJob};
use eframe::egui::{
    self, Color32, ColorImage, Pos2, Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2,
};
use ffprobe::FfProbe;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};
use which::which;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const PREVIEW_WIDTH: usize = 960;
const PREVIEW_HEIGHT: usize = 540;
const THUMB_WIDTH: usize = 160;
const THUMB_HEIGHT: usize = 90;
const THUMB_COUNT: usize = 12;

pub enum TrimmerAction {
    Render(Vec<GuiClipJob>),
}

#[derive(Clone, Copy, PartialEq)]
enum DragTarget {
    Start,
    End,
    Playhead,
}

struct AudioTrack {
    stream_index: i64,
    label: String,
    keep: bool,
}

struct PreviewPlayer {
    video: Option<Child>,
    audio: Option<Child>,
    frames: Option<Receiver<Vec<u8>>>,
    origin: f64,
    started: Option<Instant>,
}

impl PreviewPlayer {
    fn new() -> Self {
        Self {
            video: None,
            audio: None,
            frames: None,
            origin: 0.0,
            started: None,
        }
    }

    fn is_playing(&self) -> bool {
        self.started.is_some()
    }

    fn position(&self) -> f64 {
        self.started
            .map(|started| self.origin + started.elapsed().as_secs_f64())
            .unwrap_or(self.origin)
    }

    fn stop(&mut self) -> f64 {
        let position = self.position();
        if let Some(child) = self.video.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(child) = self.audio.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.video = None;
        self.audio = None;
        self.frames = None;
        self.started = None;
        self.origin = position;
        position
    }

    fn start(
        &mut self,
        path: &Path,
        position: f64,
        end: f64,
        audio_stream: Option<i64>,
    ) -> Result<(), String> {
        self.stop();
        let ffmpeg = which("ffmpeg").map_err(|_| "FFmpeg was not found in PATH".to_owned())?;
        let duration = (end - position).max(0.0);
        if duration <= 0.0 {
            return Ok(());
        }

        let filter = format!(
            "scale={PREVIEW_WIDTH}:{PREVIEW_HEIGHT}:force_original_aspect_ratio=decrease,pad={PREVIEW_WIDTH}:{PREVIEW_HEIGHT}:(ow-iw)/2:(oh-ih)/2,fps=30"
        );
        let mut command = quiet_command(&ffmpeg);
        let mut video = command
            .args([
                "-loglevel",
                "error",
                "-ss",
                &format_seconds(position),
                "-t",
                &format_seconds(duration),
                "-i",
            ])
            .arg(path)
            .args([
                "-an", "-vf", &filter, "-pix_fmt", "rgb24", "-f", "rawvideo", "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Failed to start preview decoder: {error}"))?;

        let mut stdout = video.stdout.take().ok_or("Preview decoder had no output")?;
        let (sender, receiver) = mpsc::sync_channel(2);
        thread::spawn(move || read_preview_frames(&mut stdout, sender));

        let audio = if let (Some(stream), Ok(ffplay)) = (audio_stream, which("ffplay")) {
            let mut audio_command = quiet_command(&ffplay);
            audio_command
                .args([
                    "-nodisp",
                    "-autoexit",
                    "-loglevel",
                    "quiet",
                    "-ss",
                    &format_seconds(position),
                    "-t",
                    &format_seconds(duration),
                    "-ast",
                    &stream.to_string(),
                ])
                .arg(path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()
        } else {
            None
        };

        self.video = Some(video);
        self.audio = audio;
        self.frames = Some(receiver);
        self.origin = position;
        self.started = Some(Instant::now());
        Ok(())
    }
}

impl Drop for PreviewPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct TrimmerClip {
    path: PathBuf,
    probe: FfProbe,
    duration: f64,
    fps: f64,
    start: f64,
    end: f64,
    playhead: f64,
    start_text: String,
    end_text: String,
    audio_tracks: Vec<AudioTrack>,
    preview_audio: Option<i64>,
    size_enabled: bool,
    size_mb: f64,
    preview: PreviewPlayer,
    preview_texture: Option<TextureHandle>,
    still_receiver: Option<Receiver<(u64, Vec<u8>)>>,
    pending_still: Option<(Instant, f64)>,
    still_generation: u64,
    thumbnails_receiver: Option<Receiver<Vec<Vec<u8>>>>,
    thumbnails: Vec<TextureHandle>,
    drag_target: Option<DragTarget>,
    error: Option<String>,
    last_preview_frame: Instant,
    resume_after_seek: bool,
}

impl TrimmerClip {
    fn load(path: PathBuf) -> Result<Self, String> {
        let probe =
            probe_video(&path).ok_or_else(|| format!("Could not open {}", path.display()))?;
        let duration = probe
            .format
            .duration
            .as_deref()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| *value > 0.0)
            .ok_or_else(|| format!("Could not determine the duration of {}", path.display()))?;
        let fps = probe
            .streams
            .iter()
            .find(|stream| stream.codec_type.as_deref() == Some("video"))
            .and_then(|stream| parse_rational(&stream.avg_frame_rate))
            .filter(|fps| *fps > 0.0)
            .unwrap_or(30.0);

        let audio_tracks: Vec<AudioTrack> = probe
            .streams
            .iter()
            .filter(|stream| stream.codec_type.as_deref() == Some("audio"))
            .map(|stream| {
                let tags = stream.tags.as_ref();
                let language = tags
                    .and_then(|tags| tags.language.as_deref())
                    .unwrap_or("und");
                let name = tags
                    .and_then(|tags| tags.handler_name.as_deref())
                    .unwrap_or("Audio");
                let codec = stream.codec_name.as_deref().unwrap_or("unknown");
                let channels = stream
                    .channels
                    .map(|channels| format!("{channels} ch"))
                    .unwrap_or_else(|| "unknown channels".to_owned());
                AudioTrack {
                    stream_index: stream.index,
                    label: format!(
                        "#{0}  {name} · {language} · {codec} · {channels}",
                        stream.index
                    ),
                    keep: true,
                }
            })
            .collect();

        let preview_audio = probe
            .streams
            .iter()
            .find(|stream| {
                stream.codec_type.as_deref() == Some("audio") && stream.disposition.default == 1
            })
            .map(|stream| stream.index)
            .or_else(|| audio_tracks.first().map(|track| track.stream_index));

        let mut clip = Self {
            path,
            probe,
            duration,
            fps,
            start: 0.0,
            end: duration,
            playhead: 0.0,
            start_text: format_time(0.0),
            end_text: format_time(duration),
            audio_tracks,
            preview_audio,
            size_enabled: false,
            size_mb: 50.0,
            preview: PreviewPlayer::new(),
            preview_texture: None,
            still_receiver: None,
            pending_still: None,
            still_generation: 0,
            thumbnails_receiver: None,
            thumbnails: Vec::new(),
            drag_target: None,
            error: None,
            last_preview_frame: Instant::now() - Duration::from_millis(34),
            resume_after_seek: false,
        };
        clip.request_still(0.0);
        clip.request_thumbnails();
        Ok(clip)
    }

    fn frame_duration(&self) -> f64 {
        1.0 / self.fps.max(1.0)
    }

    fn snap(&self, value: f64) -> f64 {
        ((value * self.fps).round() / self.fps).clamp(0.0, self.duration)
    }

    fn request_still(&mut self, position: f64) {
        self.pending_still = Some((
            Instant::now() + Duration::from_millis(100),
            position.clamp(0.0, self.duration),
        ));
    }

    fn start_pending_still(&mut self, position: f64) {
        self.still_generation += 1;
        let generation = self.still_generation;
        let path = self.path.clone();
        let (sender, receiver) = mpsc::channel();
        self.still_receiver = Some(receiver);
        thread::spawn(move || {
            if let Some(frame) = decode_still(&path, position, PREVIEW_WIDTH, PREVIEW_HEIGHT) {
                let _ = sender.send((generation, frame));
            }
        });
    }

    fn request_thumbnails(&mut self) {
        let path = self.path.clone();
        let duration = self.duration;
        let (sender, receiver) = mpsc::channel();
        self.thumbnails_receiver = Some(receiver);
        thread::spawn(move || {
            let mut frames = Vec::with_capacity(THUMB_COUNT);
            for index in 0..THUMB_COUNT {
                let position = duration * (index as f64 + 0.5) / THUMB_COUNT as f64;
                if let Some(frame) = decode_still(&path, position, THUMB_WIDTH, THUMB_HEIGHT) {
                    frames.push(frame);
                }
            }
            let _ = sender.send(frames);
        });
    }

    fn poll_images(&mut self, ctx: &egui::Context, clip_index: usize) {
        if self.preview.is_playing()
            && self.last_preview_frame.elapsed() >= Duration::from_millis(33)
        {
            let frame = self
                .preview
                .frames
                .as_ref()
                .and_then(|receiver| receiver.try_recv().ok());
            if let Some(frame) = frame {
                self.set_preview_texture(ctx, clip_index, frame);
                self.last_preview_frame = Instant::now();
            }
        }

        let mut clear_still_receiver = false;
        let still = if let Some(receiver) = &self.still_receiver {
            match receiver.try_recv() {
                Ok(result) => {
                    clear_still_receiver = true;
                    Some(result)
                }
                Err(TryRecvError::Disconnected) => {
                    clear_still_receiver = true;
                    None
                }
                Err(TryRecvError::Empty) => None,
            }
        } else {
            None
        };
        if clear_still_receiver {
            self.still_receiver = None;
        }
        if let Some((generation, frame)) = still {
            if generation == self.still_generation
                && self.pending_still.is_none()
                && !self.preview.is_playing()
            {
                self.set_preview_texture(ctx, clip_index, frame);
            }
        }

        if self.still_receiver.is_none() && !self.preview.is_playing() {
            if let Some((deadline, position)) = self.pending_still {
                if Instant::now() >= deadline {
                    self.pending_still = None;
                    self.start_pending_still(position);
                } else {
                    ctx.request_repaint_after(deadline.saturating_duration_since(Instant::now()));
                }
            }
        }

        let thumbs = self
            .thumbnails_receiver
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok());
        if let Some(frames) = thumbs {
            self.thumbnails = frames
                .into_iter()
                .enumerate()
                .map(|(index, frame)| {
                    ctx.load_texture(
                        format!("trim-thumb-{clip_index}-{index}"),
                        ColorImage::from_rgb([THUMB_WIDTH, THUMB_HEIGHT], &frame),
                        TextureOptions::LINEAR,
                    )
                })
                .collect();
            self.thumbnails_receiver = None;
        }
    }

    fn set_preview_texture(&mut self, ctx: &egui::Context, clip_index: usize, frame: Vec<u8>) {
        let image = ColorImage::from_rgb([PREVIEW_WIDTH, PREVIEW_HEIGHT], &frame);
        if let Some(texture) = self.preview_texture.as_mut() {
            texture.set(image, TextureOptions::LINEAR);
        } else {
            self.preview_texture = Some(ctx.load_texture(
                format!("trim-preview-{clip_index}"),
                image,
                TextureOptions::LINEAR,
            ));
        }
    }

    fn seek(&mut self, position: f64) {
        let position = self.snap(position).clamp(self.start, self.end);
        let was_playing = self.preview.is_playing();
        self.preview.stop();
        self.playhead = position;
        self.preview.origin = position;
        if was_playing && position < self.end {
            self.start_playback();
        } else {
            self.request_still(position);
        }
    }

    fn start_playback(&mut self) {
        self.pending_still = None;
        if self.playhead >= self.end - self.frame_duration() {
            self.playhead = self.start;
        }
        self.error = self
            .preview
            .start(&self.path, self.playhead, self.end, self.preview_audio)
            .err();
        self.last_preview_frame = Instant::now() - Duration::from_millis(34);
    }

    fn toggle_playback(&mut self) {
        if self.preview.is_playing() {
            self.playhead = self.preview.stop().clamp(self.start, self.end);
            self.request_still(self.playhead);
        } else {
            self.start_playback();
        }
    }

    fn stop_preview(&mut self) {
        if self.preview.is_playing() {
            self.playhead = self.preview.stop().clamp(self.start, self.end);
        }
    }

    fn to_job(&self) -> GuiClipJob {
        GuiClipJob {
            path: self.path.clone(),
            probe: self.probe.clone(),
            selection: ClipSelection {
                start_seconds: self.start,
                end_seconds: self.end,
                audio_stream_indices: self
                    .audio_tracks
                    .iter()
                    .filter(|track| track.keep)
                    .map(|track| track.stream_index)
                    .collect(),
                max_size_bytes: self
                    .size_enabled
                    .then_some((self.size_mb.max(0.0) * 1_000_000.0).round() as u64),
            },
        }
    }

    fn is_valid(&self) -> bool {
        self.end - self.start >= self.frame_duration()
            && (!self.size_enabled || self.size_mb >= 1.0)
    }
}

pub struct Trimmer {
    clips: Vec<TrimmerClip>,
    active: usize,
    message: Option<String>,
}

impl Trimmer {
    pub fn new() -> Self {
        Self {
            clips: Vec::new(),
            active: 0,
            message: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }

    pub fn add_paths(&mut self, paths: Vec<PathBuf>) {
        for path in paths {
            match TrimmerClip::load(path) {
                Ok(clip) => self.clips.push(clip),
                Err(error) => self.message = Some(error),
            }
        }
        self.active = self.active.min(self.clips.len().saturating_sub(1));
    }

    pub fn stop(&mut self) {
        for clip in &mut self.clips {
            clip.stop_preview();
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) -> Option<TrimmerAction> {
        if self.clips.is_empty() {
            return None;
        }

        if let Some(message) = &self.message {
            ui.colored_label(Color32::LIGHT_RED, message);
        }

        let mut remove = None;
        let mut render = false;
        let mut new_active = None;
        ui.horizontal(|ui| {
            ui.heading("Clip trimmer");
            ui.separator();
            for (index, clip) in self.clips.iter().enumerate() {
                let name = clip.path.file_name().unwrap_or_default().to_string_lossy();
                if ui.selectable_label(self.active == index, name).clicked() {
                    new_active = Some(index);
                }
            }
            ui.separator();
            if ui.button("Remove").clicked() {
                remove = Some(self.active);
            }
            if ui.button("Add clips").clicked() {
                if let Some(paths) = pick_videos() {
                    self.add_paths(paths);
                }
            }
        });

        if let Some(index) = new_active {
            remove_playback(&mut self.clips, self.active);
            self.active = index;
        }

        if let Some(index) = remove {
            self.clips[index].stop_preview();
            self.clips.remove(index);
            self.active = self.active.min(self.clips.len().saturating_sub(1));
            return None;
        }
        if self.clips.is_empty() {
            return None;
        }

        let clip_index = self.active;
        let clip = &mut self.clips[clip_index];
        clip.poll_images(ctx, clip_index);
        if clip.preview.is_playing() {
            clip.playhead = clip.preview.position().clamp(clip.start, clip.end);
            if clip.playhead >= clip.end - 0.005 {
                clip.stop_preview();
                clip.playhead = clip.end;
            }
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        if !ctx.wants_keyboard_input() {
            if ctx.input(|input| input.key_pressed(egui::Key::Space)) {
                clip.toggle_playback();
            }
            if ctx.input(|input| input.key_pressed(egui::Key::I)) {
                let was_playing = clip.preview.is_playing();
                if was_playing {
                    clip.playhead = clip.preview.stop();
                }
                clip.start = clip.playhead.min(clip.end - clip.frame_duration());
                clip.start_text = format_time(clip.start);
                if was_playing {
                    clip.start_playback();
                }
            }
            if ctx.input(|input| input.key_pressed(egui::Key::O)) {
                let was_playing = clip.preview.is_playing();
                if was_playing {
                    clip.playhead = clip.preview.stop();
                }
                clip.end = clip.playhead.max(clip.start + clip.frame_duration());
                clip.end_text = format_time(clip.end);
                if was_playing {
                    clip.start_playback();
                }
            }
        }

        ui.add_space(8.0);
        let available = ui.available_width().min(960.0);
        let preview_size = Vec2::new(available, available * 9.0 / 16.0);
        ui.vertical_centered(|ui| {
            if let Some(texture) = &clip.preview_texture {
                ui.add(egui::Image::new((texture.id(), preview_size)));
            } else {
                ui.allocate_ui(preview_size, |ui| {
                    ui.centered_and_justified(|ui| ui.spinner());
                });
            }
        });

        let old_start = clip.start;
        let old_end = clip.end;
        let old_playhead = clip.playhead;
        timeline(ui, clip);
        if old_start != clip.start || old_end != clip.end || old_playhead != clip.playhead {
            clip.start_text = format_time(clip.start);
            clip.end_text = format_time(clip.end);
            if !clip.preview.is_playing() {
                clip.request_still(clip.playhead);
            }
        }

        ui.horizontal(|ui| {
            if ui
                .button(if clip.preview.is_playing() {
                    "Pause"
                } else {
                    "Play"
                })
                .clicked()
            {
                clip.toggle_playback();
            }
            if ui.button("◀ frame").clicked() {
                clip.seek(clip.playhead - clip.frame_duration());
            }
            if ui.button("frame ▶").clicked() {
                clip.seek(clip.playhead + clip.frame_duration());
            }
            ui.label(format!(
                "{} / {}",
                format_time(clip.playhead),
                format_time(clip.duration)
            ));
            ui.label("Space: play · I/O: set range");
        });

        ui.horizontal(|ui| {
            ui.label("In");
            let start_response = ui.text_edit_singleline(&mut clip.start_text);
            if start_response.lost_focus()
                || start_response
                    .ctx
                    .input(|i| i.key_pressed(egui::Key::Enter))
            {
                if let Some(value) = parse_time(&clip.start_text) {
                    clip.start = clip
                        .snap(value)
                        .min(clip.end - clip.frame_duration())
                        .max(0.0);
                    clip.start_text = format_time(clip.start);
                    clip.seek(clip.playhead.max(clip.start));
                }
            }
            ui.label("Out");
            let end_response = ui.text_edit_singleline(&mut clip.end_text);
            if end_response.lost_focus()
                || end_response.ctx.input(|i| i.key_pressed(egui::Key::Enter))
            {
                if let Some(value) = parse_time(&clip.end_text) {
                    clip.end = clip
                        .snap(value)
                        .max(clip.start + clip.frame_duration())
                        .min(clip.duration);
                    clip.end_text = format_time(clip.end);
                    clip.seek(clip.playhead.min(clip.end));
                }
            }
            ui.label(format!("Selected: {}", format_time(clip.end - clip.start)));
        });

        ui.separator();
        ui.columns(2, |columns| {
            columns[0].heading("Audio tracks");
            if clip.audio_tracks.is_empty() {
                columns[0].label("No audio tracks");
            }
            let old_preview_audio = clip.preview_audio;
            for track in &mut clip.audio_tracks {
                columns[0].horizontal(|ui| {
                    ui.checkbox(&mut track.keep, &track.label);
                    ui.radio_value(&mut clip.preview_audio, Some(track.stream_index), "listen");
                });
            }
            if clip.preview_audio != old_preview_audio && clip.preview.is_playing() {
                clip.playhead = clip.preview.stop().clamp(clip.start, clip.end);
                clip.start_playback();
            }
            if which("ffplay").is_err() {
                columns[0]
                    .colored_label(Color32::YELLOW, "FFplay not found: audio preview is muted");
            }

            columns[1].heading("Discord size target");
            columns[1].checkbox(&mut clip.size_enabled, "Keep this output under");
            columns[1].horizontal(|ui| {
                ui.add_enabled(
                    clip.size_enabled,
                    egui::DragValue::new(&mut clip.size_mb)
                        .range(1.0..=10_000.0)
                        .speed(1.0),
                );
                ui.label("MB");
            });
            if clip.size_enabled {
                columns[1].label("Uses a fast size-targeted MP4 with H.264 video and AAC audio.");
            }
        });

        if let Some(error) = &clip.error {
            ui.colored_label(Color32::LIGHT_RED, error);
        }

        ui.add_space(8.0);
        let all_valid = self.clips.iter().all(TrimmerClip::is_valid);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(all_valid, egui::Button::new("Render all clips"))
                .clicked()
            {
                render = true;
            }
            if !all_valid {
                ui.colored_label(
                    Color32::LIGHT_RED,
                    "Every clip needs a non-empty range and a size limit of at least 1 MB.",
                );
            }
        });

        if render {
            self.stop();
            return Some(TrimmerAction::Render(
                self.clips.iter().map(TrimmerClip::to_job).collect(),
            ));
        }
        None
    }
}

fn remove_playback(clips: &mut [TrimmerClip], active: usize) {
    if let Some(clip) = clips.get_mut(active) {
        clip.stop_preview();
    }
}

fn timeline(ui: &mut egui::Ui, clip: &mut TrimmerClip) {
    let desired = Vec2::new(ui.available_width(), 92.0);
    let (rect, response) = ui.allocate_exact_size(desired, Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    if clip.thumbnails.is_empty() {
        painter.rect_filled(rect, 4.0, Color32::from_gray(28));
    } else {
        let width = rect.width() / clip.thumbnails.len() as f32;
        for (index, texture) in clip.thumbnails.iter().enumerate() {
            let cell = Rect::from_min_max(
                Pos2::new(rect.left() + index as f32 * width, rect.top()),
                Pos2::new(rect.left() + (index + 1) as f32 * width, rect.bottom()),
            );
            painter.image(
                texture.id(),
                cell,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }
    }

    let x_for = |seconds: f64| rect.left() + rect.width() * (seconds / clip.duration) as f32;
    let start_x = x_for(clip.start);
    let end_x = x_for(clip.end);
    let playhead_x = x_for(clip.playhead);
    painter.rect_filled(
        Rect::from_min_max(rect.min, Pos2::new(start_x, rect.bottom())),
        0.0,
        Color32::from_black_alpha(170),
    );
    painter.rect_filled(
        Rect::from_min_max(Pos2::new(end_x, rect.top()), rect.max),
        0.0,
        Color32::from_black_alpha(170),
    );
    painter.line_segment(
        [
            Pos2::new(start_x, rect.top()),
            Pos2::new(start_x, rect.bottom()),
        ],
        Stroke::new(3.0, Color32::LIGHT_GREEN),
    );
    painter.line_segment(
        [
            Pos2::new(end_x, rect.top()),
            Pos2::new(end_x, rect.bottom()),
        ],
        Stroke::new(3.0, Color32::LIGHT_RED),
    );
    painter.line_segment(
        [
            Pos2::new(playhead_x, rect.top()),
            Pos2::new(playhead_x, rect.bottom()),
        ],
        Stroke::new(2.0, Color32::WHITE),
    );

    if response.drag_started() {
        if let Some(pointer) = response.interact_pointer_pos() {
            let distances = [
                (DragTarget::Start, (pointer.x - start_x).abs()),
                (DragTarget::End, (pointer.x - end_x).abs()),
                (DragTarget::Playhead, (pointer.x - playhead_x).abs()),
            ];
            clip.drag_target = distances
                .into_iter()
                .min_by(|left, right| left.1.total_cmp(&right.1))
                .map(|item| item.0);
            clip.resume_after_seek = clip.preview.is_playing();
            clip.preview.stop();
        }
    }
    if response.clicked() {
        if let Some(pointer) = response.interact_pointer_pos() {
            let seconds = clip.snap(
                ((pointer.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * clip.duration,
            );
            clip.seek(seconds);
        }
    } else if response.dragged() {
        if let Some(pointer) = response.interact_pointer_pos() {
            let seconds = clip.snap(
                ((pointer.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * clip.duration,
            );
            let target = clip.drag_target.unwrap_or(DragTarget::Playhead);
            match target {
                DragTarget::Start => {
                    clip.start = seconds.min(clip.end - clip.frame_duration()).max(0.0);
                    clip.playhead = clip.playhead.max(clip.start);
                }
                DragTarget::End => {
                    clip.end = seconds
                        .max(clip.start + clip.frame_duration())
                        .min(clip.duration);
                    clip.playhead = clip.playhead.min(clip.end);
                }
                DragTarget::Playhead => clip.playhead = seconds.clamp(clip.start, clip.end),
            }
        }
    }
    if response.drag_stopped() {
        if clip.resume_after_seek {
            clip.start_playback();
        }
        clip.resume_after_seek = false;
        clip.drag_target = None;
    } else if response.clicked() {
        clip.drag_target = None;
    }
}

fn read_preview_frames(stdout: &mut impl Read, sender: SyncSender<Vec<u8>>) {
    let frame_size = PREVIEW_WIDTH * PREVIEW_HEIGHT * 3;
    loop {
        let mut frame = vec![0_u8; frame_size];
        if stdout.read_exact(&mut frame).is_err() || sender.send(frame).is_err() {
            break;
        }
    }
}

fn decode_still(path: &Path, position: f64, width: usize, height: usize) -> Option<Vec<u8>> {
    let ffmpeg = which("ffmpeg").ok()?;
    let filter = format!(
        "scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2"
    );
    let output = quiet_command(&ffmpeg)
        .args(["-loglevel", "error", "-ss", &format_seconds(position), "-i"])
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-vf",
            &filter,
            "-pix_fmt",
            "rgb24",
            "-f",
            "rawvideo",
            "-",
        ])
        .output()
        .ok()?;
    let expected = width * height * 3;
    (output.status.success() && output.stdout.len() == expected).then_some(output.stdout)
}

fn quiet_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    command
}

fn pick_videos() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new()
        .add_filter("Video file", crate::VIDEO_EXTENSIONS)
        .set_title("Add clips to Smoothie")
        .pick_files()
}

fn parse_rational(value: &str) -> Option<f64> {
    if let Some((numerator, denominator)) = value.split_once('/') {
        let numerator = numerator.parse::<f64>().ok()?;
        let denominator = denominator.parse::<f64>().ok()?;
        (denominator != 0.0).then_some(numerator / denominator)
    } else {
        value.parse().ok()
    }
}

fn format_seconds(value: f64) -> String {
    format!("{:.6}", value.max(0.0))
}

fn format_time(value: f64) -> String {
    let total_ms = (value.max(0.0) * 1000.0).round() as u64;
    let hours = total_ms / 3_600_000;
    let minutes = total_ms / 60_000 % 60;
    let seconds = total_ms / 1000 % 60;
    let millis = total_ms % 1000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

fn parse_time(value: &str) -> Option<f64> {
    let parts: Vec<&str> = value.trim().split(':').collect();
    match parts.as_slice() {
        [seconds] => seconds.parse().ok(),
        [minutes, seconds] => {
            Some(minutes.parse::<f64>().ok()? * 60.0 + seconds.parse::<f64>().ok()?)
        }
        [hours, minutes, seconds] => Some(
            hours.parse::<f64>().ok()? * 3600.0
                + minutes.parse::<f64>().ok()? * 60.0
                + seconds.parse::<f64>().ok()?,
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{format_time, parse_rational, parse_time};

    #[test]
    fn time_round_trip() {
        let value = 3723.456;
        assert!((parse_time(&format_time(value)).unwrap() - value).abs() < 0.001);
        assert_eq!(parse_time("01:02.500"), Some(62.5));
    }

    #[test]
    fn rational_fps() {
        assert!((parse_rational("60000/1001").unwrap() - 59.94005994).abs() < 0.0001);
        assert_eq!(parse_rational("30"), Some(30.0));
    }
}
