use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::transport::queue::QueueItem;

pub use crate::transport::queue::QueueVersion;
pub use crate::transport::protocol::{QueueCommandType, RendererReport, RendererReportType};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QConnectQueueState {
    pub version: QueueVersion,
    pub queue_items: Vec<QueueItem>,
    pub shuffle_mode: bool,
    pub autoplay_mode: bool,
    pub shuffle_order: Option<Vec<usize>>,
}

impl Default for QConnectQueueState {
    fn default() -> Self {
        Self {
            version: QueueVersion::default(),
            queue_items: Vec::new(),
            shuffle_mode: false,
            autoplay_mode: false,
            shuffle_order: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QConnectRendererState {
    pub current_track: Option<QueueItem>,
    pub next_track: Option<QueueItem>,
    pub current_position_ms: Option<u64>,
    pub playing_state: Option<i32>,
}

impl Default for QConnectRendererState {
    fn default() -> Self {
        Self {
            current_track: None,
            next_track: None,
            current_position_ms: None,
            playing_state: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", content = "payload")]
pub enum RendererCommand {
    SetState {
        playing_state: Option<i32>,
        current_position_ms: Option<u64>,
        current_track: Option<QueueItem>,
        next_track: Option<QueueItem>,
    },
    SetVolume {
        volume: Option<i32>,
        volume_delta: Option<i32>,
    },
    MuteVolume {
        value: bool,
    },
    SetMaxAudioQuality {
        max_audio_quality: i32,
    },
    SetActive {
        active: bool,
    },
    SetLoopMode {
        loop_mode: i32,
    },
    SetShuffleMode {
        shuffle_mode: bool,
    },
}

#[derive(Debug, Clone)]
pub enum QconnectAppEvent {
    QueueUpdated(QConnectQueueState),
    RendererCommandApplied { command: RendererCommand },
    RendererUpdated(QConnectRendererState),
    SessionManagementEvent { message_type: String, payload: Value },
}

use async_trait::async_trait;

#[async_trait]
pub trait QconnectEventSink: Send + Sync {
    async fn on_event(&self, event: QconnectAppEvent);
}
