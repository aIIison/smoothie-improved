use crate::video::{probe_video, ClipSelection, GuiClipJob};
use eframe::egui::{
    self, Color32, ColorImage, Pos2, Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2,
};
use ffprobe::FfProbe;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};
use which::which;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const PREVIEW_WIDTH: usize = 768;
const PREVIEW_HEIGHT: usize = 432;
const PREVIEW_FPS: f64 = 30.0;
const THUMB_WIDTH: usize = 160;
const THUMB_HEIGHT: usize = 90;
const THUMB_COUNT: usize = 12;
// One worker per thumbnail; every thumb is an independent tiny ffmpeg seek,
// so saturating the machine with short-lived processes finishes in one round
// instead of three. Spawning a dozen processes costs far less than decoding
// twelve frames sequentially.
const STILL_DEBOUNCE: Duration = Duration::from_millis(25);
const MAX_IN_FLIGHT_STILLS: usize = 2;
const STILL_CACHE_SIZE: usize = 16;

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
    frames: Option<Receiver<(u64, Vec<u8>)>>,
    origin: f64,
    started: Option<Instant>,
}

/// Everything needed to start a playback session from a trimmed clip.
struct PlaybackSettings<'a> {
    ffmpeg: &'a Path,
    ffplay: Option<&'a Path>,
    path: &'a Path,
    position: f64,
    end: f64,
    audio_stream: Option<i64>,
    source_fps: f64,
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

    fn start(&mut self, settings: PlaybackSettings<'_>) -> Result<(), String> {
        self.stop();
        let PlaybackSettings {
            ffmpeg,
            ffplay,
            path,
            position,
            end,
            audio_stream,
            source_fps,
        } = settings;
        let duration = (end - position).max(0.0);
        if duration <= 0.0 {
            return Ok(());
        }

        // Decoding a 120 fps source frame-by-frame just to throw most frames
        // away at the fps=30 stage wastes CPU. Skip input frames ahead of time
        // whenever the source is comfortably faster than the preview.
        let skip = (source_fps / PREVIEW_FPS).round() as u64;
        let skip_filter = if skip > 1 {
            format!(",select='not(mod(n\\,{skip}))'")
        } else {
            String::new()
        };
        let filter = format!(
            "scale={PREVIEW_WIDTH}:{PREVIEW_HEIGHT}:force_original_aspect_ratio=decrease,pad={PREVIEW_WIDTH}:{PREVIEW_HEIGHT}:(ow-iw)/2:(oh-ih)/2{skip_filter},fps={PREVIEW_FPS}"
        );
        let mut command = quiet_command(ffmpeg);
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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Failed to start preview decoder: {error}"))?;

        let mut stdout = video.stdout.take().ok_or("Preview decoder had no output")?;
        // A single buffered frame keeps playback responsive instead of letting
        // decoded frames queue up behind the UI and making seeking feel delayed.
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || read_preview_frames(&mut stdout, sender));

        let audio = if let (Some(stream), Some(ffplay)) = (audio_stream, ffplay) {
            let mut audio_command = quiet_command(ffplay);
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

/// Everything derived from probing a file. Built on a worker thread so adding
/// clips never freezes the UI; the egui-dependent parts are filled in later on
/// the UI thread.
struct ClipDraft {
    path: PathBuf,
    probe: FfProbe,
    duration: f64,
    fps: f64,
    audio_tracks: Vec<AudioTrack>,
    preview_audio: Option<i64>,
}

pub struct TrimmerClip {
    path: PathBuf,
    probe: FfProbe,
    duration: f64,
    fps: f64,
    start: f64,
    end: f64,
    playhead: f64,
    displayed_position: f64,
    start_text: String,
    end_text: String,
    audio_tracks: Vec<AudioTrack>,
    preview_audio: Option<i64>,
    size_enabled: bool,
    size_mb: f64,
    preview: PreviewPlayer,
    preview_texture: Option<TextureHandle>,
    // All pending still decodes report through one long-lived channel so that
    // several can be in flight at once while the UI thread stays single.
    still_sender: Sender<(u64, u64, Option<Vec<u8>>)>,
    still_receiver: Receiver<(u64, u64, Option<Vec<u8>>)>,
    still_in_flight: usize,
    // (snapped frame index, rgb bytes) most-recently used; re-seeking to a
    // frame that was already shown is instant instead of re-decoding.
    still_cache: Vec<(u64, Vec<u8>)>,
    still_hit: Option<(u64, Vec<u8>)>,
    pending_still: Option<(Instant, f64)>,
    still_generation: u64,
    thumbnails_receiver: Option<Receiver<Vec<Vec<u8>>>>,
    thumbnails_started: bool,
    thumbnails: Vec<TextureHandle>,
    drag_target: Option<DragTarget>,
    error: Option<String>,
    last_preview_frame: Instant,
    resume_after_seek: bool,
    ffmpeg: Option<PathBuf>,
    ffplay: Option<PathBuf>,
}

impl TrimmerClip {
    /// Probe the file off the UI thread; cheap enough to run anywhere.
    fn probe(path: PathBuf) -> Result<ClipDraft, String> {
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

        Ok(ClipDraft {
            path,
            probe,
            duration,
            fps,
            audio_tracks,
            preview_audio,
        })
    }

    /// Assemble a clip on the UI thread from a probed draft.
    fn finish(draft: ClipDraft, ffmpeg: Option<PathBuf>, ffplay: Option<PathBuf>) -> Self {
        let (still_sender, still_receiver) = mpsc::channel();
        let mut clip = Self {
            path: draft.path,
            probe: draft.probe,
            duration: draft.duration,
            fps: draft.fps,
            start: 0.0,
            end: draft.duration,
            playhead: 0.0,
            displayed_position: 0.0,
            start_text: format_time(0.0),
            end_text: format_time(draft.duration),
            audio_tracks: draft.audio_tracks,
            preview_audio: draft.preview_audio,
            size_enabled: false,
            size_mb: 50.0,
            preview: PreviewPlayer::new(),
            preview_texture: None,
            still_sender,
            still_receiver,
            still_in_flight: 0,
            still_cache: Vec::with_capacity(STILL_CACHE_SIZE),
            still_hit: None,
            pending_still: None,
            still_generation: 0,
            thumbnails_receiver: None,
            thumbnails_started: false,
            thumbnails: Vec::new(),
            drag_target: None,
            error: None,
            last_preview_frame: Instant::now() - Duration::from_millis(34),
            resume_after_seek: false,
            ffmpeg,
            ffplay,
        };
        clip.request_still(0.0);
        clip
    }

    fn frame_duration(&self) -> f64 {
        1.0 / self.fps.max(1.0)
    }

    fn snap(&self, value: f64) -> f64 {
        ((value * self.fps).round() / self.fps).clamp(0.0, self.duration)
    }

    fn frame_index(&self, position: f64) -> u64 {
        (position.clamp(0.0, self.duration) * self.fps).round() as u64
    }

    fn request_still(&mut self, position: f64) {
        let position = position.clamp(0.0, self.duration);
        if let Some((frame, data)) = self
            .still_cache
            .iter()
            .find(|(frame, _)| *frame == self.frame_index(position))
        {
            self.pending_still = None;
            self.still_hit = Some((*frame, data.clone()));
            return;
        }
        self.pending_still = Some((Instant::now() + STILL_DEBOUNCE, position));
    }

    fn start_pending_still(&mut self, position: f64) {
        self.still_generation += 1;
        let generation = self.still_generation;
        let frame = self.frame_index(position);
        let path = self.path.clone();
        let ffmpeg = self.ffmpeg.clone();
        let sender = self.still_sender.clone();
        self.still_in_flight += 1;
        thread::spawn(move || {
            let result = ffmpeg.as_deref().and_then(|ffmpeg| {
                decode_still(ffmpeg, &path, position, PREVIEW_WIDTH, PREVIEW_HEIGHT)
            });
            let _ = sender.send((generation, frame, result));
        });
    }

    fn cache_still(&mut self, frame: u64, data: Vec<u8>) {
        if self.still_cache.iter().any(|(f, _)| *f == frame) {
            return;
        }
        if self.still_cache.len() >= STILL_CACHE_SIZE {
            self.still_cache.remove(0);
        }
        self.still_cache.push((frame, data));
    }

    fn request_thumbnails(&mut self) {
        self.thumbnails_started = true;
        let path = self.path.clone();
        let duration = self.duration;
        let ffmpeg = self.ffmpeg.clone();
        let (sender, receiver) = mpsc::channel();
        self.thumbnails_receiver = Some(receiver);
        thread::spawn(move || {
            let path = Arc::new(path);
            let next_index = Arc::new(AtomicUsize::new(0));
            let (frame_sender, frame_receiver) = mpsc::channel();
            let mut workers = Vec::with_capacity(THUMB_COUNT);
            for _ in 0..THUMB_COUNT {
                let path = Arc::clone(&path);
                let next_index = Arc::clone(&next_index);
                let frame_sender = frame_sender.clone();
                let ffmpeg = ffmpeg.clone();
                workers.push(thread::spawn(move || loop {
                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    if index >= THUMB_COUNT {
                        break;
                    }
                    let position = duration * (index as f64 + 0.5) / THUMB_COUNT as f64;
                    let frame = ffmpeg.as_deref().and_then(|ffmpeg| {
                        decode_still(ffmpeg, &path, position, THUMB_WIDTH, THUMB_HEIGHT)
                    });
                    let _ = frame_sender.send((index, frame));
                }));
            }
            drop(frame_sender);

            let mut frames = vec![None; THUMB_COUNT];
            for (index, frame) in frame_receiver {
                frames[index] = frame;
            }
            for worker in workers {
                let _ = worker.join();
            }
            let black = vec![0_u8; THUMB_WIDTH * THUMB_HEIGHT * 3];
            let frames = frames
                .into_iter()
                .map(|frame| frame.unwrap_or_else(|| black.clone()))
                .collect();
            let _ = sender.send(frames);
        });
    }

    fn poll_images(&mut self, ctx: &egui::Context, clip_index: usize) {
        if !self.thumbnails_started {
            self.request_thumbnails();
        }
        if self.preview.is_playing()
            && self.last_preview_frame.elapsed() >= Duration::from_millis(33)
        {
            let preview_frame = self
                .preview
                .frames
                .as_ref()
                .and_then(|receiver| receiver.try_recv().ok());
            if let Some((frame_index, frame)) = preview_frame {
                self.displayed_position =
                    self.snap(self.preview.origin + frame_index as f64 / PREVIEW_FPS);
                self.set_preview_texture(ctx, clip_index, frame);
                self.last_preview_frame = Instant::now();
            }
        }

        // A cache hit is applied directly without touching the decoder.
        if let Some((frame, data)) = self.still_hit.take() {
            if !self.preview.is_playing() {
                self.displayed_position = frame as f64 / self.fps;
                self.set_preview_texture(ctx, clip_index, data);
            }
        }

        let mut received = Vec::new();
        while let Ok(result) = self.still_receiver.try_recv() {
            received.push(result);
        }
        for (generation, frame, data) in received {
            self.still_in_flight = self.still_in_flight.saturating_sub(1);
            if self.preview.is_playing() {
                continue;
            }
            if let Some(data) = data {
                self.cache_still(frame, data.clone());
                if generation == self.still_generation {
                    self.displayed_position = frame as f64 / self.fps;
                    self.set_preview_texture(ctx, clip_index, data);
                }
            }
        }

        if !self.preview.is_playing() {
            if self.still_in_flight < MAX_IN_FLIGHT_STILLS {
                if let Some((deadline, position)) = self.pending_still {
                    if Instant::now() >= deadline {
                        self.pending_still = None;
                        self.start_pending_still(position);
                    } else {
                        ctx.request_repaint_after(
                            deadline.saturating_duration_since(Instant::now()),
                        );
                    }
                }
            } else if self.pending_still.is_some() {
                // All decoders are busy; retry as soon as one frees up.
                ctx.request_repaint_after(Duration::from_millis(16));
            }
        }

        // Keep the UI awake while decoders are still working so results are
        // shown the moment they arrive, without needing mouse input.
        if self.still_in_flight > 0
            || self.pending_still.is_some()
            || self.still_hit.is_some()
            || self.thumbnails_receiver.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(16));
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
        self.error = match self.ffmpeg.as_deref() {
            Some(ffmpeg) => self
                .preview
                .start(PlaybackSettings {
                    ffmpeg,
                    ffplay: self.ffplay.as_deref(),
                    path: &self.path,
                    position: self.playhead,
                    end: self.end,
                    audio_stream: self.preview_audio,
                    source_fps: self.fps,
                })
                .err(),
            None => Some("FFmpeg was not found in PATH".to_owned()),
        };
        self.last_preview_frame = Instant::now() - Duration::from_millis(34);
    }

    fn toggle_playback(&mut self) {
        if self.preview.is_playing() {
            self.preview.stop();
            self.playhead = self.displayed_position.clamp(self.start, self.end);
            self.preview.origin = self.playhead;
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
    ffmpeg: Option<PathBuf>,
    ffplay: Option<PathBuf>,
    loading_receiver: Receiver<(PathBuf, Result<ClipDraft, String>)>,
    loading_sender: Sender<(PathBuf, Result<ClipDraft, String>)>,
    pending_loads: usize,
}

impl Trimmer {
    pub fn new() -> Self {
        let (loading_sender, loading_receiver) = mpsc::channel();
        Self {
            clips: Vec::new(),
            active: 0,
            message: None,
            ffmpeg: which("ffmpeg").ok(),
            ffplay: which("ffplay").ok(),
            loading_receiver,
            loading_sender,
            pending_loads: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.clips.is_empty() && self.pending_loads == 0
    }

    pub fn add_paths(&mut self, paths: Vec<PathBuf>) {
        for path in paths {
            self.pending_loads += 1;
            let sender = self.loading_sender.clone();
            thread::spawn(move || {
                let _ = sender.send((path.clone(), TrimmerClip::probe(path)));
            });
        }
    }

    /// Collects clips whose probing finished since the last frame.
    fn poll_loading(&mut self) {
        loop {
            match self.loading_receiver.try_recv() {
                Ok((_path, Ok(draft))) => {
                    self.pending_loads = self.pending_loads.saturating_sub(1);
                    let clip = TrimmerClip::finish(draft, self.ffmpeg.clone(), self.ffplay.clone());
                    self.clips.push(clip);
                }
                Ok((path, Err(error))) => {
                    self.pending_loads = self.pending_loads.saturating_sub(1);
                    self.message = Some(format!("{}: {error}", path.display()));
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.pending_loads = 0;
                    break;
                }
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
        self.poll_loading();
        if self.clips.is_empty() {
            if self.pending_loads > 0 {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Probing clips…");
                });
            }
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
                clip.start = clip
                    .snap(clip.displayed_position)
                    .min(clip.end - clip.frame_duration());
                clip.start_text = format_time(clip.start);
            }
            if ctx.input(|input| input.key_pressed(egui::Key::O)) {
                clip.end = clip
                    .snap(clip.displayed_position)
                    .max(clip.start + clip.frame_duration());
                clip.end_text = format_time(clip.end);
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
            if self.ffplay.is_none() {
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

fn read_preview_frames(stdout: &mut impl Read, sender: SyncSender<(u64, Vec<u8>)>) {
    let frame_size = PREVIEW_WIDTH * PREVIEW_HEIGHT * 3;
    let mut frame_index = 0_u64;
    loop {
        let mut frame = vec![0_u8; frame_size];
        if stdout.read_exact(&mut frame).is_err() || sender.send((frame_index, frame)).is_err() {
            break;
        }
        frame_index += 1;
    }
}

fn decode_still(
    ffmpeg: &Path,
    path: &Path,
    position: f64,
    width: usize,
    height: usize,
) -> Option<Vec<u8>> {
    let filter = format!(
        "scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2"
    );
    let output = quiet_command(ffmpeg)
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
    use super::{decode_still, format_time, parse_rational, parse_time};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

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

    #[test]
    fn still_decodes_letterboxed_preview_frame() {
        let Ok(ffmpeg) = which::which("ffmpeg") else {
            return;
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("smoothie-still-test-{nonce}.mp4"));
        let generated = Command::new(&ffmpeg)
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=1920x1080:rate=30:duration=2",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert!(generated.success());

        let frame = decode_still(&ffmpeg, &source, 1.0, 768, 432);
        assert!(frame.is_some(), "still decode failed");
        assert_eq!(frame.unwrap().len(), 768 * 432 * 3);

        // Positions past the end of the clip yield no frame.
        assert!(decode_still(&ffmpeg, &source, 60.0, 768, 432).is_none());

        let _ = std::fs::remove_file(&source);
    }
}