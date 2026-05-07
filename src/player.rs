//! Custom audio player with EIO error handling for PipeWire restart scenarios
//!
//! This player provides:
//! - Basic playback control (play, pause, stop, seek, volume)
//! - EIO error detection and automatic recovery
//! - Gapless playback support
//! - Integration with qbz-audio backends

use rodio::mixer::Mixer;
use rodio::source::EmptyCallback;
use rodio::{Decoder, MixerDeviceSink, Player, Source};
use serde::Serialize;
use std::io::{BufReader, Cursor};
use std::num::NonZero;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::default::{get_codecs, get_probe};
use tokio::sync::mpsc;

/// Playback event for state updates
#[derive(Debug, Clone, Serialize)]
pub struct PlaybackEvent
{
    pub is_playing: bool,
    pub position: u64,
    pub duration: u64,
    pub track_id: u64,
    pub queue_item_id: u64,
    pub volume: f32,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub gapless_ready: bool,
    pub gapless_next_track_id: u64,
    pub gapless_next_queue_item_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shuffle: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalization_gain: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_perfect_mode: Option<String>,
}

/// Shared state for thread-safe access
#[derive(Clone)]
pub struct SharedState
{
    is_playing: Arc<AtomicBool>,
    position: Arc<AtomicU64>, // Base position in seconds (used when not playing)
    duration: Arc<AtomicU64>,
    track_id: Arc<AtomicU64>,
    queue_item_id: Arc<AtomicU64>,
    has_loaded_audio: Arc<AtomicBool>,
    volume: Arc<AtomicU32>, // 0-100 as u32
    stream_error: Arc<AtomicBool>,
    sample_rate: Arc<AtomicU32>,
    bit_depth: Arc<AtomicU32>,
    gapless_ready: Arc<AtomicBool>,
    gapless_next_track_id: Arc<AtomicU64>,
    gapless_next_queue_item_id: Arc<AtomicU64>,
    gapless_next_duration: Arc<AtomicU64>,
    pending_seek: Arc<AtomicU64>, // Pending seek position in seconds
    has_pending_seek: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    // Position tracking
    playback_start_time: Arc<std::sync::Mutex<Option<Instant>>>, // When playback started
}

impl SharedState
{
    pub fn new() -> Self
    {
        Self {
            is_playing: Arc::new(AtomicBool::new(false)),
            position: Arc::new(AtomicU64::new(0)),
            duration: Arc::new(AtomicU64::new(0)),
            track_id: Arc::new(AtomicU64::new(0)),
            queue_item_id: Arc::new(AtomicU64::new(0)),
            has_loaded_audio: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(75)), // Default 75%
            stream_error: Arc::new(AtomicBool::new(false)),
            sample_rate: Arc::new(AtomicU32::new(0)),
            bit_depth: Arc::new(AtomicU32::new(0)),
            gapless_ready: Arc::new(AtomicBool::new(false)),
            gapless_next_track_id: Arc::new(AtomicU64::new(0)),
            gapless_next_queue_item_id: Arc::new(AtomicU64::new(0)),
            gapless_next_duration: Arc::new(AtomicU64::new(0)),
            pending_seek: Arc::new(AtomicU64::new(0)),
            has_pending_seek: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
            playback_start_time: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn set_stream_error(&self, error: bool)
    {
        self.stream_error.store(error, Ordering::SeqCst);
    }

    pub fn has_stream_error(&self) -> bool
    {
        self.stream_error.load(Ordering::SeqCst)
    }

    pub fn set_stream_quality(&self, sample_rate: u32, bit_depth: u32)
    {
        self.sample_rate.store(sample_rate, Ordering::SeqCst);
        self.bit_depth.store(bit_depth, Ordering::SeqCst);
    }

    pub fn set_pending_seek(&self, position_secs: u64)
    {
        self.pending_seek.store(position_secs, Ordering::SeqCst);
        self.has_pending_seek.store(true, Ordering::SeqCst);
    }

    pub fn take_pending_seek(&self) -> Option<u64>
    {
        if self.has_pending_seek.swap(false, Ordering::SeqCst)
        {
            Some(self.pending_seek.load(Ordering::SeqCst))
        }
        else
        {
            None
        }
    }

    fn next_generation(&self) -> u64
    {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn current_generation(&self) -> u64
    {
        self.generation.load(Ordering::SeqCst)
    }

    fn set_current_track(&self, track_id: u64, queue_item_id: u64, duration_secs: u64)
    {
        self.track_id.store(track_id, Ordering::SeqCst);
        self.queue_item_id.store(queue_item_id, Ordering::SeqCst);
        self.duration.store(duration_secs, Ordering::SeqCst);
    }

    fn set_gapless_next(&self, track_id: u64, queue_item_id: u64, duration_secs: u64)
    {
        self.gapless_next_track_id.store(track_id, Ordering::SeqCst);
        self.gapless_next_queue_item_id
            .store(queue_item_id, Ordering::SeqCst);
        self.gapless_next_duration
            .store(duration_secs, Ordering::SeqCst);
    }

    fn clear_gapless_next(&self)
    {
        self.gapless_next_track_id.store(0, Ordering::SeqCst);
        self.gapless_next_queue_item_id.store(0, Ordering::SeqCst);
        self.gapless_next_duration.store(0, Ordering::SeqCst);
    }

    fn stop_playback_tracking(&self, clear_track: bool)
    {
        let mut start_time = self.playback_start_time.lock().unwrap();
        *start_time = None;
        self.position.store(0, Ordering::SeqCst);
        self.is_playing.store(false, Ordering::SeqCst);
        self.has_loaded_audio.store(false, Ordering::SeqCst);
        self.gapless_ready.store(false, Ordering::SeqCst);
        self.clear_gapless_next();
        if clear_track
        {
            self.track_id.store(0, Ordering::SeqCst);
            self.queue_item_id.store(0, Ordering::SeqCst);
            self.duration.store(0, Ordering::SeqCst);
        }
    }

    /// Pause playback position tracking
    pub fn pause_playback(&self)
    {
        let position = self.current_position();
        self.position.store(position, Ordering::SeqCst);
        let mut start_time = self.playback_start_time.lock().unwrap();
        *start_time = None;
        self.is_playing.store(false, Ordering::SeqCst);
    }

    /// Resume playback position tracking
    pub fn resume_playback(&self)
    {
        let mut start_time = self.playback_start_time.lock().unwrap();
        *start_time = Some(Instant::now());
        self.is_playing.store(true, Ordering::SeqCst);
    }

    /// Reset playback tracking (for new track or seek)
    pub fn reset_playback_tracking(&self, initial_position_secs: u64)
    {
        let mut start_time = self.playback_start_time.lock().unwrap();
        *start_time = Some(Instant::now());
        self.position.store(initial_position_secs, Ordering::SeqCst);

        // Set playing state to true
        self.is_playing.store(true, Ordering::SeqCst);

        log::info!("reset_playback_tracking: initial_position={}s", initial_position_secs);
    }

    fn mark_track_ended(&self, ended_queue_item_id: u64, generation: u64)
    {
        if self.current_generation() != generation
        {
            log::debug!("Ignoring stale track-end callback for queue item {}", ended_queue_item_id);
            return;
        }

        if self.queue_item_id.load(Ordering::SeqCst) != ended_queue_item_id
        {
            log::debug!(
                "Ignoring track-end callback for non-current queue item {}",
                ended_queue_item_id
            );
            return;
        }

        let duration = self.duration.load(Ordering::SeqCst);
        let next_track_id = self.gapless_next_track_id.load(Ordering::SeqCst);
        let next_queue_item_id = self.gapless_next_queue_item_id.load(Ordering::SeqCst);
        if next_track_id > 0 && next_queue_item_id > 0
        {
            let next_duration = self.gapless_next_duration.load(Ordering::SeqCst);
            self.set_current_track(next_track_id, next_queue_item_id, next_duration);
            self.clear_gapless_next();
            self.reset_playback_tracking(0);
            self.gapless_ready.store(true, Ordering::SeqCst);
            log::info!(
                "Track advanced to queue item {} (track {}), position reset to 0",
                next_queue_item_id,
                next_track_id
            );
        }
        else
        {
            let mut start_time = self.playback_start_time.lock().unwrap();
            *start_time = None;
            self.position.store(duration, Ordering::SeqCst);
            self.is_playing.store(false, Ordering::SeqCst);
            self.has_loaded_audio.store(false, Ordering::SeqCst);
            self.gapless_ready.store(false, Ordering::SeqCst);
            log::info!(
                "Track ended at queue item {} with no gapless next track queued",
                ended_queue_item_id
            );
        }
    }

    pub fn current_position(&self) -> u64
    {
        if !self.is_playing.load(Ordering::SeqCst)
        {
            // When paused or stopped, return the stored position
            return self.position.load(Ordering::SeqCst);
        }

        // When playing, calculate position based on start time and paused time
        let start_time = self.playback_start_time.lock().unwrap();
        if let Some(start) = *start_time
        {
            let base_position = self.position.load(Ordering::SeqCst);
            let elapsed_secs = start.elapsed().as_secs();
            let position = base_position.saturating_add(elapsed_secs);
            let duration = self.duration.load(Ordering::SeqCst);
            if duration > 0
            {
                position.min(duration)
            }
            else
            {
                position
            }
        }
        else
        {
            // No start time recorded, return stored position
            self.position.load(Ordering::SeqCst)
        }
    }
}

impl Default for SharedState
{
    fn default() -> Self
    {
        Self::new()
    }
}

/// Audio player with EIO error recovery
pub struct AudioPlayer
{
    /// Command sender to audio thread
    tx: mpsc::Sender<AudioCommand>,
    /// EIO error signal sender (for logger to notify us of EIO errors)
    eio_signal_tx: mpsc::Sender<()>,
    /// Shared state
    pub state: SharedState,
}

/// Commands sent to audio thread
enum AudioCommand
{
    Play
    {
        data: Vec<u8>,
        track_id: u64,
        queue_item_id: u64,
        duration_secs: u64,
    },
    PlayNext
    {
        data: Vec<u8>,
        track_id: u64,
        queue_item_id: u64,
        duration_secs: u64,
    },
    TrackEnded
    {
        queue_item_id: u64,
        generation: u64,
    },
    Pause,
    Resume,
    Stop,
    SetVolume(f32),
    Seek(u64),
    RecoverFromEio,
}

#[derive(Clone)]
struct QueuedTrack
{
    data: Vec<u8>,
    track_id: u64,
    queue_item_id: u64,
    duration_secs: u64,
}

/// Cursor media source for symphonia
struct CursorMediaSource
{
    inner: Cursor<Vec<u8>>,
    len: u64,
}

impl CursorMediaSource
{
    fn new(data: Vec<u8>) -> Self
    {
        let len = data.len() as u64;
        Self {
            inner: Cursor::new(data),
            len,
        }
    }
}

impl std::io::Read for CursorMediaSource
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>
    {
        self.inner.read(buf)
    }
}

impl std::io::Seek for CursorMediaSource
{
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64>
    {
        self.inner.seek(pos)
    }
}

impl symphonia::core::io::MediaSource for CursorMediaSource
{
    fn is_seekable(&self) -> bool
    {
        true
    }

    fn byte_len(&self) -> Option<u64>
    {
        Some(self.len)
    }
}

#[derive(Debug, Clone, Copy)]
struct AudioMetadata
{
    sample_rate: u32,
    channels: u16,
    bit_depth: Option<u32>,
    duration_secs: Option<u64>,
}

/// Extract audio metadata using symphonia
fn extract_audio_metadata(data: &[u8]) -> Result<AudioMetadata, String>
{
    let source = Box::new(CursorMediaSource::new(data.to_vec()))
        as Box<dyn symphonia::core::io::MediaSource>;
    let mss = symphonia::core::io::MediaSourceStream::new(source, Default::default());

    let mut hint = Hint::new();
    hint.with_extension("m4a");

    let format_opts = FormatOptions {
        enable_gapless: true,
        ..Default::default()
    };

    let metadata_opts: MetadataOptions = Default::default();

    let probed = get_probe()
        .format(&hint, mss, &format_opts, &metadata_opts)
        .map_err(|e| format!("Symphonia probe failed: {}", e))?;

    let track = probed
        .format
        .default_track()
        .ok_or_else(|| "No supported audio tracks".to_string())?;

    let codec_params = track.codec_params.clone();

    let sample_rate = codec_params
        .sample_rate
        .ok_or_else(|| "No sample rate in codec params".to_string())?;

    let channels = codec_params.channels.map(|c| c.count() as u16).unwrap_or(2);

    let bit_depth = codec_params.bits_per_sample;
    let duration_secs = codec_params
        .time_base
        .as_ref()
        .zip(codec_params.n_frames)
        .map(|(time_base, n_frames)| {
            let duration = time_base.calc_time(n_frames);
            duration.seconds + u64::from(duration.frac > 0.0)
        })
        .or_else(|| decoder_duration_secs(data));

    Ok(AudioMetadata {
        sample_rate,
        channels,
        bit_depth,
        duration_secs,
    })
}

fn decoder_duration_secs(data: &[u8]) -> Option<u64>
{
    let cursor = Cursor::new(data.to_vec());
    Decoder::new(BufReader::new(cursor))
        .ok()
        .and_then(|decoder| decoder.total_duration())
        .map(duration_secs_ceil)
}

fn duration_secs_ceil(duration: Duration) -> u64
{
    duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() > 0))
}

impl AudioPlayer
{
    /// Create a new player
    pub fn new(_device_name: Option<String>, _visualizer_tap: Option<Arc<()>>) -> Self
    {
        let (tx, mut rx) = mpsc::channel(32);
        let (eio_signal_tx, mut eio_signal_rx) = mpsc::channel::<()>(1);
        let tx_for_eio = tx.clone(); // Clone for EIO signal handler
        let tx_for_return = tx.clone(); // Clone to return in struct
        let tx_for_track_ended = tx.clone();

        let state = SharedState::new();
        let thread_state = state.clone();

        // Spawn audio thread (using std thread instead of tokio because MixerDeviceSink is not Send)
        std::thread::spawn(move || {
            log::info!("Audio thread starting...");

            let rt = tokio::runtime::Runtime::new()
                .map_err(|e| format!("Failed to create runtime: {}", e))
                .unwrap();

            log::info!("Audio thread runtime created");

            // Attempt to initialize output sink
            let mut player: Option<Player> = None;
            let mut sink_device: Option<MixerDeviceSink> = None;

            match Self::init_output_stream()
            {
                Ok(s) =>
                {
                    log::info!("Audio output initialized");
                    sink_device = Some(s);
                }
                Err(e) =>
                {
                    log::error!("Failed to initialize audio output: {}", e);
                }
            }
            let mut current_track_id: u64 = 0;
            let mut current_queue_item_id: u64 = 0;
            let mut current_data: Option<Vec<u8>> = None;
            let mut queued_next: Option<QueuedTrack> = None;
            let is_recovering = Arc::new(AtomicBool::new(false));

            // Process commands
            let _ = rt.block_on(async {
                // Listen for EIO signals from the logger
                let recovering_clone = is_recovering.clone();
                tokio::spawn(async move {
                    loop {
                        match eio_signal_rx.recv().await {
                            Some(()) => {
                                if !recovering_clone.load(Ordering::SeqCst) {
                                    log::error!("EIO error signal from logger, triggering recovery...");
                                    recovering_clone.store(true, Ordering::SeqCst);
                                    let _ = tx_for_eio.try_send(AudioCommand::RecoverFromEio);
                                }
                            }
                            None => break,
                        }
                    }
                });

                while let Some(cmd) = rx.recv().await
                {
                    match cmd
                    {
                        AudioCommand::Play {
                            data,
                            track_id,
                            queue_item_id,
                            duration_secs,
                        } =>
                        {
                            log::info!("Play command received: track_id={}, bytes={}, duration={}s",
                                track_id, data.len(), duration_secs);

                            // Extract metadata
                            match extract_audio_metadata(&data)
                            {
                                Ok(metadata) =>
                                {
                                    log::info!(
                                        "Audio format: {}Hz, {}ch, {:?}bit",
                                        metadata.sample_rate,
                                        metadata.channels,
                                        metadata.bit_depth
                                    );
                                    thread_state.set_stream_quality(
                                        metadata.sample_rate,
                                        metadata.bit_depth.unwrap_or(16),
                                    );
                                }
                                Err(e) =>
                                {
                                    log::warn!("Failed to extract audio metadata: {}", e);
                                }
                            }

                            current_track_id = track_id;
                            current_queue_item_id = queue_item_id;
                            current_data = Some(data.clone());
                            queued_next = None;
                            thread_state.next_generation();
                            let generation = thread_state.current_generation();
                            player.take();
                            thread_state.set_current_track(track_id, queue_item_id, duration_secs);
                            thread_state.clear_gapless_next();

                            // Ensure we have a sink and mixer
                            if sink_device.is_none()
                            {
                                match Self::init_output_stream()
                                {
                                    Ok(s) =>
                                    {
                                        sink_device = Some(s);
                                    }
                                    Err(e) =>
                                    {
                                        log::error!("Failed to init sink: {}", e);
                                        continue;
                                    }
                                }
                            }

                            // Try to play, handling EIO errors
                            let mixer_opt = sink_device.as_ref().map(|s| s.mixer());
                            log::info!("Calling play_audio with mixer_opt={}", mixer_opt.is_some());
                            match Self::play_audio(
                                mixer_opt,
                                &data,
                                &thread_state,
                                track_id,
                                queue_item_id,
                                generation,
                                tx_for_track_ended.clone(),
                            )
                            {
                                Ok(s) =>
                                {
                                    log::info!("play_audio succeeded, creating Player");
                                    player = Some(s);
                                    thread_state.set_stream_error(false);
                                    thread_state.has_loaded_audio.store(true, Ordering::SeqCst);
                                    thread_state.reset_playback_tracking(0);
                                    thread_state.gapless_ready.store(true, Ordering::SeqCst);
                                    thread_state.clear_gapless_next();
                                    log::info!("Player state: is_playing={}, has_loaded_audio={}, gapless_ready={}",
                                        thread_state.is_playing.load(Ordering::SeqCst),
                                        thread_state.has_loaded_audio.load(Ordering::SeqCst),
                                        thread_state.gapless_ready.load(Ordering::SeqCst));

                                    // Check for pending seek
                                    if let Some(seek_secs) = thread_state.take_pending_seek()
                                    {
                                        log::info!("Applying pending seek to {} seconds", seek_secs);
                                        if let Some(ref sink) = sink_device
                                        {
                                            let mixer = sink.mixer();
                                            player.take();
                                            let generation = thread_state.next_generation();
                                            thread_state.reset_playback_tracking(seek_secs);
                                            match Self::play_audio_from_mixer_with_seek(
                                                &mixer,
                                                &data,
                                                &thread_state,
                                                seek_secs,
                                                track_id,
                                                queue_item_id,
                                                generation,
                                                tx_for_track_ended.clone(),
                                            )
                                            {
                                                Ok(new_player) =>
                                                {
                                                    player = Some(new_player);
                                                    log::info!("Pending seek applied to {} seconds", seek_secs);
                                                }
                                                Err(e) =>
                                                {
                                                    log::error!("Failed to apply pending seek: {}", e);
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) =>
                                {
                                    let error_msg = e.to_string();
                                    log::error!("Playback error: {}", error_msg);

                                    if error_msg.contains("EIO")
                                        || error_msg.contains("I/O error")
                                        || error_msg.contains("snd_pcm_poll_descriptors")
                                        || error_msg.contains("backend-specific error")
                                        || error_msg.contains("Unknown errno")
                                        || error_msg.contains("(-5)")
                                    {
                                        log::error!("EIO error detected, attempting recovery...");

                                        thread_state.set_stream_error(true);

                                        // Reinitialize stream
                                        player.take();
                                        drop(sink_device.take());

                                        // Give PipeWire time to recover
                                        tokio::time::sleep(Duration::from_millis(500)).await;

                                        match Self::init_output_stream()
                                        {
                                            Ok(s) =>
                                            {
                                                sink_device = Some(s);

                                                // Retry playback
                                                let mixer_opt = sink_device.as_ref().map(|s| s.mixer());
                                                match Self::play_audio(
                                                    mixer_opt,
                                                    &data,
                                                    &thread_state,
                                                    track_id,
                                                    queue_item_id,
                                                    generation,
                                                    tx_for_track_ended.clone(),
                                                )
                                                {
                                                    Ok(s) =>
                                                    {
                                                        player = Some(s);
                                                        thread_state.set_stream_error(false);
                                                        thread_state
                                                            .has_loaded_audio
                                                            .store(true, Ordering::SeqCst);
                                                        thread_state.reset_playback_tracking(0);
                                                        thread_state.gapless_ready.store(true, Ordering::SeqCst);
                                                        log::info!(
                                                            "Recovery successful, playback resumed"
                                                        );
                                                    }
                                                    Err(e) =>
                                                    {
                                                        log::error!("Recovery failed: {}", e);
                                                        thread_state.set_stream_error(true);
                                                    }
                                                }
                                            }
                                            Err(e) =>
                                            {
                                                log::error!("Failed to reinitialize audio: {}", e);
                                                thread_state.set_stream_error(true);
                                            }
                                        }
                                    }
                                    else
                                    {
                                        log::error!("Non-recoverable playback error: {}", e);
                                        thread_state.set_stream_error(true);
                                    }
                                }
                            }
                        }

                        AudioCommand::PlayNext {
                            data,
                            track_id,
                            queue_item_id,
                            duration_secs,
                        } =>
                        {
                            log::info!(
                                "Queueing next track {} (queue item {}) for gapless playback",
                                track_id,
                                queue_item_id
                            );

                            if let Some(ref player) = player
                            {
                                // Try to append for gapless playback
                                match Self::create_source(&data)
                                {
                                    Ok(source) =>
                                    {
                                        let generation = thread_state.current_generation();
                                        Self::append_track_with_callback(
                                            player,
                                            source,
                                            &thread_state,
                                            queue_item_id,
                                            generation,
                                            tx_for_track_ended.clone(),
                                        );
                                        thread_state.set_gapless_next(
                                            track_id,
                                            queue_item_id,
                                            duration_secs,
                                        );
                                        queued_next = Some(QueuedTrack {
                                            data,
                                            track_id,
                                            queue_item_id,
                                            duration_secs,
                                        });
                                        log::info!(
                                            "Gapless: track {} (queue item {}) queued",
                                            track_id,
                                            queue_item_id
                                        );
                                    }
                                    Err(e) =>
                                    {
                                        log::warn!("Gapless queue failed: {}", e);
                                    }
                                }
                            }
                        }

                        AudioCommand::TrackEnded {
                            queue_item_id,
                            generation,
                        } =>
                        {
                            if thread_state.current_generation() != generation
                                || current_queue_item_id != queue_item_id
                            {
                                log::debug!(
                                    "Ignoring stale TrackEnded for queue item {}",
                                    queue_item_id
                                );
                                continue;
                            }

                            if let Some(next) = queued_next.take()
                            {
                                current_track_id = next.track_id;
                                current_queue_item_id = next.queue_item_id;
                                current_data = Some(next.data);
                                log::info!(
                                    "Promoted gapless next track {} (queue item {}, duration {}s)",
                                    next.track_id,
                                    next.queue_item_id,
                                    next.duration_secs
                                );
                            }
                            else
                            {
                                player.take();
                                current_track_id = 0;
                                current_queue_item_id = 0;
                                current_data = None;
                            }
                        }

                        AudioCommand::Pause =>
                        {
                            if let Some(ref player) = player
                            {
                                player.pause();
                                thread_state.pause_playback();
                                thread_state.gapless_ready.store(false, Ordering::SeqCst);
                                log::info!("Paused");
                            }
                        }

                        AudioCommand::Resume =>
                        {
                            // Check if already playing to avoid bogus resume
                            if thread_state.is_playing.load(Ordering::SeqCst)
                            {
                                log::warn!("Resume command received but already playing, ignoring");
                            }
                            else if let Some(ref player) = player
                            {
                                player.play();
                                thread_state.resume_playback();
                                thread_state.gapless_ready.store(true, Ordering::SeqCst);
                                log::info!("Resumed");
                            }
                            else if let Some(data) = &current_data
                            {
                                // Resume from data
                                if let Some(ref sink) = sink_device
                                {
                                    let mixer = sink.mixer();
                                    let current_pos = thread_state.current_position();
                                    let generation = thread_state.next_generation();
                                    thread_state.clear_gapless_next();
                                    queued_next = None;
                                    match Self::play_audio_from_mixer_with_seek(
                                        &mixer,
                                        data,
                                        &thread_state,
                                        current_pos,
                                        current_track_id,
                                        current_queue_item_id,
                                        generation,
                                        tx_for_track_ended.clone(),
                                    )
                                    {
                                        Ok(s) =>
                                        {
                                            player = Some(s);
                                            thread_state.reset_playback_tracking(current_pos);
                                            thread_state.gapless_ready.store(true, Ordering::SeqCst);

                                            // Check for pending seek
                                            if let Some(seek_secs) = thread_state.take_pending_seek()
                                            {
                                                log::info!("Applying pending seek to {} seconds", seek_secs);
                                                thread_state.reset_playback_tracking(seek_secs);
                                                let generation = thread_state.next_generation();
                                                match Self::play_audio_from_mixer_with_seek(
                                                    &mixer,
                                                    data,
                                                    &thread_state,
                                                    seek_secs,
                                                    current_track_id,
                                                    current_queue_item_id,
                                                    generation,
                                                    tx_for_track_ended.clone(),
                                                )
                                                {
                                                    Ok(new_player) =>
                                                    {
                                                        player = Some(new_player);
                                                        log::info!("Pending seek applied to {} seconds", seek_secs);
                                                    }
                                                    Err(e) =>
                                                    {
                                                        log::error!("Failed to apply pending seek: {}", e);
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) =>
                                        {
                                            log::error!("Resume failed: {}", e);
                                        }
                                    }
                                }
                            }
                        }

                        AudioCommand::Stop =>
                        {
                            thread_state.next_generation();
                            thread_state.stop_playback_tracking(true);
                            player.take();
                            current_track_id = 0;
                            current_queue_item_id = 0;
                            current_data = None;
                            queued_next = None;
                            log::info!("Stopped");
                        }

                        AudioCommand::SetVolume(vol) =>
                        {
                            thread_state
                                .volume
                                .store((vol * 100.0) as u32, Ordering::SeqCst);
                            if let Some(ref player) = player
                            {
                                player.set_volume(vol);
                                log::info!("Volume set to {:.2}", vol);
                            }
                        }

                        AudioCommand::Seek(pos_secs) =>
                        {
                            log::info!("Seeking to {} seconds", pos_secs);

                            // We need current_data to recreate the source
                            if let Some(ref data) = current_data
                            {
                                // Stop current playback
                                let generation = thread_state.next_generation();
                                player.take();
                                queued_next = None;
                                thread_state.clear_gapless_next();

                                // Reset tracking BEFORE starting new playback
                                thread_state.reset_playback_tracking(pos_secs);

                                // Create new source and seek to position
                                if let Some(ref sink) = sink_device
                                {
                                    let mixer = sink.mixer();
                                    match Self::play_audio_from_mixer_with_seek(
                                        &mixer,
                                        data,
                                        &thread_state,
                                        pos_secs,
                                        current_track_id,
                                        current_queue_item_id,
                                        generation,
                                        tx_for_track_ended.clone(),
                                    )
                                    {
                                        Ok(s) =>
                                        {
                                            player = Some(s);
                                            thread_state.has_pending_seek.store(false, Ordering::SeqCst);
                                            log::info!("Seeked to {} seconds", pos_secs);
                                        }
                                        Err(e) =>
                                        {
                                            log::error!("Seek failed: {}", e);
                                        }
                                    }
                                }
                            }
                            else
                            {
                                // No audio data available, store pending seek
                                log::info!("Storing pending seek to {} seconds", pos_secs);
                                thread_state.set_pending_seek(pos_secs);
                            }
                        }

                        AudioCommand::RecoverFromEio =>
                        {
                            log::error!("Recovering from EIO error...");
                            let current_pos = thread_state.current_position();
                            let recovery_generation = thread_state.next_generation();

                            // Stop playback
                            player.take();
                            queued_next = None;
                            thread_state.clear_gapless_next();

                            // Drop stream to release the device
                            drop(sink_device.take());

                            thread_state.is_playing.store(false, Ordering::SeqCst);
                            thread_state.position.store(current_pos, Ordering::SeqCst);

                            // Give PipeWire time to recover
                            tokio::time::sleep(Duration::from_millis(500)).await;

                            // Reinitialize stream
                            match Self::init_output_stream()
                            {
                                Ok(s) =>
                                {
                                    sink_device = Some(s);
                                    log::info!("Audio output reinitialized after EIO");

                                    // If we have current data, restart playback
                                    if let Some(data) = &current_data
                                    {
                                        let Some(ref sink) = sink_device
                                        else
                                        {
                                            continue;
                                        };
                                        let mixer = sink.mixer();
                                        match Self::play_audio_from_mixer_with_seek(
                                            &mixer,
                                            data,
                                            &thread_state,
                                            current_pos,
                                            current_track_id,
                                            current_queue_item_id,
                                            recovery_generation,
                                            tx_for_track_ended.clone(),
                                        )
                                        {
                                            Ok(s) =>
                                            {
                                                player = Some(s);
                                                thread_state.set_stream_error(false);
                                                thread_state.has_loaded_audio.store(true, Ordering::SeqCst);
                                                // Preserve current position when recovering
                                                thread_state.reset_playback_tracking(current_pos);
                                                thread_state.gapless_ready.store(true, Ordering::SeqCst);

                                                // Check for pending seek and apply it
                                                if let Some(seek_secs) = thread_state.take_pending_seek()
                                                {
                                                    log::info!("Applying pending seek after recovery to {} seconds", seek_secs);
                                                    if let Some(ref sink) = sink_device
                                                    {
                                                        let mixer = sink.mixer();
                                                        thread_state.reset_playback_tracking(seek_secs);
                                                        let generation = thread_state.next_generation();
                                                        match Self::play_audio_from_mixer_with_seek(
                                                            &mixer,
                                                            data,
                                                            &thread_state,
                                                            seek_secs,
                                                            current_track_id,
                                                            current_queue_item_id,
                                                            generation,
                                                            tx_for_track_ended.clone(),
                                                        )
                                                        {
                                                            Ok(new_player) =>
                                                            {
                                                                player = Some(new_player);
                                                                log::info!("Pending seek applied after recovery");
                                                            }
                                                            Err(e) =>
                                                            {
                                                                log::error!("Failed to apply pending seek after recovery: {}", e);
                                                            }
                                                        }
                                                    }
                                                }

                                                log::info!("Playback recovered successfully");
                                            }
                                            Err(e) =>
                                            {
                                                log::error!("Failed to recover playback: {}", e);
                                                thread_state.set_stream_error(true);
                                            }
                                        }
                                    }
                                }
                                Err(e) =>
                                {
                                    log::error!("Failed to reinitialize audio after EIO: {}", e);
                                    thread_state.set_stream_error(true);
                                }
                            }

                            // Clear recovery flag
                            is_recovering.store(false, Ordering::SeqCst);
                        }

                    }
                }
                Result::<(), Box<dyn std::error::Error>>::Ok(())
            });

            log::info!("Audio thread exiting");
        });

        Self {
            tx: tx_for_return,
            eio_signal_tx,
            state,
        }
    }

    /// Initialize output stream
    fn init_output_stream() -> Result<MixerDeviceSink, String>
    {
        let sink = rodio::DeviceSinkBuilder::open_default_sink()
            .map_err(|e| format!("Failed to create output sink: {}", e))?;
        Ok(sink)
    }

    /// Get the EIO signal sender for use with the logger
    pub fn eio_signal_sender(&self) -> mpsc::Sender<()>
    {
        self.eio_signal_tx.clone()
    }

    /// Play audio data
    fn play_audio(
        mixer: Option<&Mixer>,
        data: &[u8],
        state: &SharedState,
        track_id: u64,
        queue_item_id: u64,
        generation: u64,
        ended_tx: mpsc::Sender<AudioCommand>,
    ) -> Result<Player, String>
    {
        let m = mixer.ok_or_else(|| "No audio output available".to_string())?;

        Self::play_audio_from_mixer(m, data, state, track_id, queue_item_id, generation, ended_tx)
    }

    fn play_audio_from_mixer(
        mixer: &Mixer,
        data: &[u8],
        state: &SharedState,
        _track_id: u64,
        queue_item_id: u64,
        generation: u64,
        ended_tx: mpsc::Sender<AudioCommand>,
    ) -> Result<Player, String>
    {
        let source = Self::create_source(data)?;

        let player = Player::connect_new(mixer);

        // Set initial volume
        let volume = state.volume.load(Ordering::SeqCst) as f32 / 100.0;
        player.set_volume(volume);

        Self::append_track_with_callback(
            &player,
            source,
            state,
            queue_item_id,
            generation,
            ended_tx,
        );
        player.play();

        Ok(player)
    }

    fn play_audio_from_mixer_with_seek(
        mixer: &Mixer,
        data: &[u8],
        state: &SharedState,
        seek_secs: u64,
        track_id: u64,
        queue_item_id: u64,
        generation: u64,
        ended_tx: mpsc::Sender<AudioCommand>,
    ) -> Result<Player, String>
    {
        let source = Self::create_source(data)?;

        // Skip to the desired position
        let seek_duration = Duration::from_secs(seek_secs);
        let skipped_source = source.skip_duration(seek_duration);

        let player = Player::connect_new(mixer);

        // Set initial volume
        let volume = state.volume.load(Ordering::SeqCst) as f32 / 100.0;
        player.set_volume(volume);

        let _ = track_id;
        Self::append_track_with_callback(
            &player,
            Box::new(skipped_source),
            state,
            queue_item_id,
            generation,
            ended_tx,
        );
        player.play();

        Ok(player)
    }

    fn append_track_with_callback(
        player: &Player,
        source: Box<dyn Source<Item = f32> + Send>,
        state: &SharedState,
        queue_item_id: u64,
        generation: u64,
        ended_tx: mpsc::Sender<AudioCommand>,
    )
    {
        player.append(source);
        let callback_state = state.clone();
        player.append(EmptyCallback::new(Box::new(move || {
            callback_state.mark_track_ended(queue_item_id, generation);
            let _ = ended_tx.try_send(AudioCommand::TrackEnded {
                queue_item_id,
                generation,
            });
        })));
    }

    /// Create audio source from data
    fn create_source(data: &[u8]) -> Result<Box<dyn Source<Item = f32> + Send>, String>
    {
        // Try rodio decoder first
        let cursor = Cursor::new(data.to_vec());

        match Decoder::new(BufReader::new(cursor))
        {
            Ok(decoder) =>
            {
                // Decoder now returns f32 directly in rodio 0.22
                Ok(Box::new(decoder))
            }
            Err(_) =>
            {
                // Fallback to symphonia for formats rodio doesn't handle
                Self::create_source_symphonia(data)
            }
        }
    }

    /// Create source using symphonia (for MP4/AAC, etc.)
    fn create_source_symphonia(data: &[u8]) -> Result<Box<dyn Source<Item = f32> + Send>, String>
    {
        let source = Box::new(CursorMediaSource::new(data.to_vec()))
            as Box<dyn symphonia::core::io::MediaSource>;
        let mss = symphonia::core::io::MediaSourceStream::new(source, Default::default());

        let mut hint = Hint::new();
        hint.with_extension("m4a");

        let format_opts = FormatOptions {
            enable_gapless: true,
            ..Default::default()
        };

        let metadata_opts: MetadataOptions = Default::default();

        let mut probed = get_probe()
            .format(&hint, mss, &format_opts, &metadata_opts)
            .map_err(|e| format!("Symphonia probe failed: {}", e))?;

        let track = probed
            .format
            .default_track()
            .ok_or_else(|| "No supported audio tracks".to_string())?;

        let track_id = track.id;
        let codec_params = track.codec_params.clone();

        let mut decoder = get_codecs()
            .make(&codec_params, &DecoderOptions::default())
            .map_err(|e| format!("Decoder init failed: {}", e))?;

        let mut samples: Vec<f32> = Vec::new();

        loop
        {
            match probed.format.next_packet()
            {
                Ok(packet) =>
                {
                    // Use track.id directly
                    if packet.track_id() != track_id
                    {
                        continue;
                    }

                    match decoder.decode(&packet)
                    {
                        Ok(audio_buf) =>
                        {
                            let spec = *audio_buf.spec();
                            let mut sample_buf = symphonia::core::audio::SampleBuffer::new(
                                audio_buf.frames() as u64,
                                spec,
                            );
                            sample_buf.copy_interleaved_ref(audio_buf);
                            samples.extend_from_slice(sample_buf.samples());
                        }
                        Err(SymphoniaError::DecodeError(_)) => continue,
                        Err(SymphoniaError::ResetRequired) =>
                        {
                            decoder.reset();
                            continue;
                        }
                        Err(e) => return Err(format!("Decode error: {}", e)),
                    }
                }
                Err(SymphoniaError::IoError(_)) => break,
                Err(e) => return Err(format!("Read error: {}", e)),
            }
        }

        if samples.is_empty()
        {
            return Err("No audio samples decoded".to_string());
        }

        let spec = decoder.codec_params();
        let channels = NonZero::new(spec.channels.map(|c| c.count()).unwrap_or(2) as u16)
            .ok_or_else(|| "Invalid channel count".to_string())?;
        let sample_rate = NonZero::new(spec.sample_rate.unwrap_or(48000))
            .ok_or_else(|| "Invalid sample rate".to_string())?;

        Ok(Box::new(rodio::buffer::SamplesBuffer::new(channels, sample_rate, samples)))
    }

    /// Play audio data
    pub fn play_data(&self, data: Vec<u8>, track_id: u64, queue_item_id: u64)
    -> Result<(), String>
    {
        // Extract metadata first
        let metadata = extract_audio_metadata(&data).unwrap_or(AudioMetadata {
            sample_rate: 48000,
            channels: 2,
            bit_depth: Some(16),
            duration_secs: decoder_duration_secs(&data),
        });

        self.state
            .set_stream_quality(metadata.sample_rate, metadata.bit_depth.unwrap_or(16));

        self.tx
            .try_send(AudioCommand::Play {
                data,
                track_id,
                queue_item_id,
                duration_secs: metadata.duration_secs.unwrap_or(0),
            })
            .map_err(|e| format!("Failed to send play command: {}", e))
    }

    /// Queue next track for gapless playback
    pub fn play_next(&self, data: Vec<u8>, track_id: u64, queue_item_id: u64)
    -> Result<(), String>
    {
        let duration_secs = extract_audio_metadata(&data)
            .ok()
            .and_then(|metadata| metadata.duration_secs)
            .or_else(|| decoder_duration_secs(&data))
            .unwrap_or(0);

        self.tx
            .try_send(AudioCommand::PlayNext {
                data,
                track_id,
                queue_item_id,
                duration_secs,
            })
            .map_err(|e| format!("Failed to send play_next command: {}", e))
    }

    /// Pause playback
    pub fn pause(&self) -> Result<(), String>
    {
        self.tx
            .try_send(AudioCommand::Pause)
            .map_err(|e| format!("Failed to send pause command: {}", e))
    }

    /// Resume playback
    pub fn resume(&self) -> Result<(), String>
    {
        self.tx
            .try_send(AudioCommand::Resume)
            .map_err(|e| format!("Failed to send resume command: {}", e))
    }

    /// Stop playback
    pub fn stop(&self) -> Result<(), String>
    {
        self.tx
            .try_send(AudioCommand::Stop)
            .map_err(|e| format!("Failed to send stop command: {}", e))
    }

    /// Seek to position
    pub fn seek(&self, position_secs: u64) -> Result<(), String>
    {
        self.tx
            .try_send(AudioCommand::Seek(position_secs))
            .map_err(|e| format!("Failed to send seek command: {}", e))
    }

    /// Set volume
    pub fn set_volume(&self, volume: f32) -> Result<(), String>
    {
        let clamped = volume.clamp(0.0, 1.0);
        self.tx
            .try_send(AudioCommand::SetVolume(clamped))
            .map_err(|e| format!("Failed to send set_volume command: {}", e))
    }

    /// Check if audio is loaded
    pub fn has_loaded_audio(&self) -> bool
    {
        self.state.has_loaded_audio.load(Ordering::SeqCst)
    }

    /// Get playback event
    pub fn get_playback_event(&self) -> PlaybackEvent
    {
        let position = self.state.current_position();
        let is_playing = self.state.is_playing.load(Ordering::SeqCst);
        log::debug!(
            "get_playback_event: is_playing={}, position={}s, track_id={}",
            is_playing,
            position,
            self.state.track_id.load(Ordering::SeqCst)
        );

        PlaybackEvent {
            is_playing,
            position,
            duration: self.state.duration.load(Ordering::SeqCst),
            track_id: self.state.track_id.load(Ordering::SeqCst),
            queue_item_id: self.state.queue_item_id.load(Ordering::SeqCst),
            volume: self.state.volume.load(Ordering::SeqCst) as f32 / 100.0,
            sample_rate: {
                let sr = self.state.sample_rate.load(Ordering::SeqCst);
                if sr > 0 { Some(sr) } else { None }
            },
            bit_depth: {
                let bd = self.state.bit_depth.load(Ordering::SeqCst);
                if bd > 0 { Some(bd) } else { None }
            },
            gapless_ready: self.state.gapless_ready.load(Ordering::SeqCst),
            gapless_next_track_id: self.state.gapless_next_track_id.load(Ordering::SeqCst),
            gapless_next_queue_item_id: self
                .state
                .gapless_next_queue_item_id
                .load(Ordering::SeqCst),
            repeat: None,
            shuffle: None,
            normalization_gain: None,
            bit_perfect_mode: None,
        }
    }
}
