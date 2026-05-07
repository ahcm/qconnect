use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};
use serde_json::Value;
use uuid::Uuid;
use anyhow::{Result, anyhow};

use crate::app_types::*;
use crate::transport::{TransportEvent, WsTransportConfig, WsTransport};
use crate::transport::protocol::{
    build_qconnect_outbound_envelope, build_qconnect_renderer_outbound_envelope,
    QueueCommand, QueueEventType, RendererCommandType, RendererServerCommand,
};
use crate::transport::queue::{QueueItem, QueueVersion};

pub struct QconnectApp<T, S> {
    transport: Arc<T>,
    sink: Arc<S>,
    queue_state: Mutex<QConnectQueueState>,
    renderer_state: Mutex<QConnectRendererState>,
}

impl<T, S> QconnectApp<T, S>
where
    T: WsTransport + Send + Sync + 'static,
    S: QconnectEventSink + 'static,
{
    pub fn new(transport: Arc<T>, sink: Arc<S>) -> Self {
        Self {
            transport,
            sink,
            queue_state: Mutex::new(QConnectQueueState::default()),
            renderer_state: Mutex::new(QConnectRendererState::default()),
        }
    }

    pub async fn connect(&self, config: WsTransportConfig) -> Result<()> {
        self.transport.connect(config).await.map_err(|e| anyhow!("{}", e))
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.transport.disconnect().await.map_err(|e| anyhow!("{}", e))
    }

    pub fn subscribe_transport_events(&self) -> broadcast::Receiver<TransportEvent> {
        self.transport.subscribe()
    }

    pub async fn queue_state_snapshot(&self) -> QConnectQueueState {
        self.queue_state.lock().await.clone()
    }

    pub async fn renderer_state_snapshot(&self) -> QConnectRendererState {
        self.renderer_state.lock().await.clone()
    }

    pub async fn build_queue_command(&self, command_type: QueueCommandType, payload: Value) -> QueueCommand {
        let action_uuid = Uuid::new_v4().to_string();
        QueueCommand::new(command_type, action_uuid, QueueVersion::default(), payload)
    }

    pub async fn send_queue_command(&self, command: QueueCommand) -> Result<String> {
        let action_uuid = command.action_uuid.clone();
        let envelope = build_qconnect_outbound_envelope(command).map_err(|e| anyhow!("{}", e))?;
        self.transport.send(envelope).await.map_err(|e| anyhow!("{}", e))?;
        Ok(action_uuid)
    }

    pub async fn send_renderer_report_command(&self, report: RendererReport) -> Result<()> {
        let envelope = build_qconnect_renderer_outbound_envelope(report).map_err(|e| anyhow!("{}", e))?;
        self.transport.send(envelope).await.map_err(|e| anyhow!("{}", e))
    }

    pub async fn handle_transport_event(&self, event: TransportEvent) -> Result<()> {
        match event {
            TransportEvent::InboundRendererServerCommand(cmd) => {
                let command = map_renderer_server_command(&cmd);
                self.sink.on_event(QconnectAppEvent::RendererCommandApplied {
                    command,
                }).await;
            }
            TransportEvent::InboundQueueServerEvent(evt) => {
                match evt.event_type {
                    QueueEventType::SrvrCtrlQueueState => {
                        let version = evt.queue_version.unwrap_or_default();
                        let queue_items: Vec<QueueItem> = serde_json::from_value(
                            evt.payload.get("tracks").cloned().unwrap_or_default()
                        ).unwrap_or_default();
                        let shuffle_mode = evt.payload.get("shuffle_mode").and_then(Value::as_bool).unwrap_or(false);
                        let autoplay_mode = evt.payload.get("autoplay_mode").and_then(Value::as_bool).unwrap_or(false);
                        let new_queue = QConnectQueueState {
                            version,
                            queue_items,
                            shuffle_mode,
                            autoplay_mode,
                            shuffle_order: None,
                        };
                        *self.queue_state.lock().await = new_queue.clone();
                        self.sink.on_event(QconnectAppEvent::QueueUpdated(new_queue)).await;
                    }
                    QueueEventType::SrvrCtrlQueueTracksLoaded => {
                        let version = evt.queue_version.unwrap_or_default();
                        let queue_items: Vec<QueueItem> = serde_json::from_value(
                            evt.payload.get("tracks").cloned().unwrap_or_default()
                        ).unwrap_or_default();
                        let shuffle_mode = evt.payload.get("shuffle_mode").and_then(Value::as_bool).unwrap_or(false);
                        let autoplay_mode = self.queue_state.lock().await.autoplay_mode;
                        let new_queue = QConnectQueueState {
                            version,
                            queue_items,
                            shuffle_mode,
                            autoplay_mode,
                            shuffle_order: None,
                        };
                        *self.queue_state.lock().await = new_queue.clone();
                        self.sink.on_event(QconnectAppEvent::QueueUpdated(new_queue)).await;
                    }
                    QueueEventType::SrvrCtrlQueueTracksAdded
                    | QueueEventType::SrvrCtrlQueueTracksInserted => {
                        let version = evt.queue_version.unwrap_or_default();
                        let new_items: Vec<QueueItem> = serde_json::from_value(
                            evt.payload.get("tracks").cloned().unwrap_or_default()
                        ).unwrap_or_default();
                        let mut queue = self.queue_state.lock().await;
                        queue.version = version;
                        queue.queue_items.extend(new_items);
                        let new_queue = queue.clone();
                        drop(queue);
                        self.sink.on_event(QconnectAppEvent::QueueUpdated(new_queue)).await;
                    }
                    QueueEventType::SrvrCtrlQueueTracksRemoved => {
                        let version = evt.queue_version.unwrap_or_default();
                        let remove_ids: Vec<u64> = serde_json::from_value(
                            evt.payload.get("queue_item_ids").cloned().unwrap_or_default()
                        ).unwrap_or_default();
                        let mut queue = self.queue_state.lock().await;
                        queue.version = version;
                        queue.queue_items.retain(|it| !remove_ids.contains(&it.queue_item_id));
                        let new_queue = queue.clone();
                        drop(queue);
                        self.sink.on_event(QconnectAppEvent::QueueUpdated(new_queue)).await;
                    }
                    QueueEventType::SrvrCtrlQueueCleared => {
                        let version = evt.queue_version.unwrap_or_default();
                        let mut queue = self.queue_state.lock().await;
                        queue.version = version;
                        queue.queue_items.clear();
                        let new_queue = queue.clone();
                        drop(queue);
                        self.sink.on_event(QconnectAppEvent::QueueUpdated(new_queue)).await;
                    }
                    QueueEventType::SrvrCtrlRendererStateUpdated => {
                        let player_state = evt.payload.get("player_state");
                        let current_queue_item_id = player_state.and_then(|s| s.get("current_queue_item_id")).and_then(|v| v.as_u64());
                        let next_queue_item_id = player_state.and_then(|s| s.get("next_queue_item_id")).and_then(|v| v.as_u64());
                        let current_position_ms = player_state.and_then(|s| s.get("current_position")).and_then(|v| v.as_u64());
                        let playing_state = player_state.and_then(|s| s.get("playing_state")).and_then(|v| v.as_i64()).map(|v| v as i32);

                        log::debug!("RENDERER_STATE_UPDATED: current_id={:?}, next_id={:?}", current_queue_item_id, next_queue_item_id);

                        let queue = self.queue_state.lock().await;
                        let current_track = current_queue_item_id.and_then(|id| {
                            queue.queue_items.iter().find(|it| it.queue_item_id == id).cloned()
                        });
                        let next_track = next_queue_item_id.and_then(|id| {
                            queue.queue_items.iter().find(|it| it.queue_item_id == id).cloned()
                        });

                        if current_queue_item_id.is_some() && current_track.is_none() {
                            log::warn!("Could not find current_track for queue_item_id {:?}", current_queue_item_id);
                        }

                        let new_renderer = QConnectRendererState {
                            current_track,
                            next_track,
                            current_position_ms,
                            playing_state,
                        };
                        *self.renderer_state.lock().await = new_renderer.clone();
                        self.sink.on_event(QconnectAppEvent::RendererUpdated(new_renderer)).await;
                    }
                    QueueEventType::SrvrCtrlLoopModeSet => {
                        let loop_mode = evt.payload.get("loop_mode").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                        self.sink.on_event(QconnectAppEvent::RendererCommandApplied {
                            command: RendererCommand::SetLoopMode { loop_mode },
                        }).await;
                    }
                    QueueEventType::SrvrCtrlShuffleModeSet => {
                        let shuffle_mode = evt.payload.get("shuffle_mode").and_then(|v| v.as_bool()).unwrap_or_default();
                        self.sink.on_event(QconnectAppEvent::RendererCommandApplied {
                            command: RendererCommand::SetShuffleMode { shuffle_mode },
                        }).await;
                    }
                    _ => {
                        self.sink.on_event(QconnectAppEvent::SessionManagementEvent {
                            message_type: evt.message_type().to_string(),
                            payload: evt.payload,
                        }).await;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn state_handle(&self) -> Arc<Mutex<AppState>> {
        Arc::new(Mutex::new(AppState {
            pending: PendingActions {},
        }))
    }
}

fn map_renderer_server_command(cmd: &RendererServerCommand) -> RendererCommand {
    match cmd.command_type {
        RendererCommandType::SrvrRndrSetState => {
            let playing_state = cmd.payload.get("playing_state").and_then(|v| v.as_i64()).map(|v| v as i32);
            let current_position_ms = cmd.payload.get("current_position").and_then(|v| v.as_u64());
            let current_track = cmd.payload.get("current_track")
                .filter(|v| v.is_object())
                .and_then(|v| serde_json::from_value::<QueueItem>(v.clone()).ok());
            let next_track = cmd.payload.get("next_track")
                .filter(|v| v.is_object())
                .and_then(|v| serde_json::from_value::<QueueItem>(v.clone()).ok());
            RendererCommand::SetState { playing_state, current_position_ms, current_track, next_track }
        }
        RendererCommandType::SrvrRndrSetVolume => {
            let volume = cmd.payload.get("volume").and_then(|v| v.as_i64()).map(|v| v as i32);
            let volume_delta = cmd.payload.get("volume_delta").and_then(|v| v.as_i64()).map(|v| v as i32);
            RendererCommand::SetVolume { volume, volume_delta }
        }
        RendererCommandType::SrvrRndrSetActive => {
            let active = cmd.payload.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
            RendererCommand::SetActive { active }
        }
        RendererCommandType::SrvrRndrSetMaxAudioQuality => {
            let max_audio_quality = cmd.payload.get("max_audio_quality").and_then(|v| v.as_i64()).unwrap_or(4) as i32;
            RendererCommand::SetMaxAudioQuality { max_audio_quality }
        }
        RendererCommandType::SrvrRndrSetLoopMode => {
            let loop_mode = cmd.payload.get("loop_mode").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            RendererCommand::SetLoopMode { loop_mode }
        }
        RendererCommandType::SrvrRndrSetShuffleMode => {
            let shuffle_mode = cmd.payload.get("shuffle_mode").and_then(|v| v.as_bool()).unwrap_or_default();
            RendererCommand::SetShuffleMode { shuffle_mode }
        }
        RendererCommandType::SrvrRndrMuteVolume => {
            let value = cmd.payload.get("value").and_then(|v| v.as_bool()).unwrap_or_default();
            RendererCommand::MuteVolume { value }
        }
    }
}

pub struct AppState {
    pub pending: PendingActions,
}

pub struct PendingActions {}

impl PendingActions {
    pub fn current(&self) -> Option<PendingAction> { None }
    pub fn clear(&mut self) {}
}

pub struct PendingAction {
    pub uuid: String,
}
