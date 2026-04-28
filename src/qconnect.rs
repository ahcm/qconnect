use std::{
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use qbz_audio::{AudioDiagnostic, AudioSettings};
use qbz_models::{Quality, Track};
use qbz_player::Player;
use qbz_qobuz::QobuzClient;
use qconnect_app::{
    QConnectQueueState, QConnectRendererState, QconnectApp, QconnectAppEvent, QconnectEventSink,
    QueueCommandType, RendererCommand, RendererReport, RendererReportType,
};
use qconnect_core::QueueItem;
use qconnect_transport_ws::{NativeWsTransport, TransportEvent, WsTransportConfig};
use serde_json::{Value, json};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinHandle;
use uuid::Uuid;

const PLAYING_STATE_STOPPED: i32 = 1;
const PLAYING_STATE_PLAYING: i32 = 2;
const PLAYING_STATE_PAUSED: i32 = 3;
const BUFFER_STATE_OK: i32 = 2;
const JOIN_SESSION_REASON_CONTROLLER_REQUEST: i32 = 1;
const AUDIO_QUALITY_MP3: i32 = 1;
const AUDIO_QUALITY_HIRES_LEVEL2: i32 = 4;
const VOLUME_REMOTE_CONTROL_ALLOWED: i32 = 2;

type App = QconnectApp<NativeWsTransport, CliEventSink>;

pub async fn browser_login(
    bind: &str,
    timeout: Duration,
    print_token: bool,
    no_browser: bool,
) -> Result<()>
{
    println!("Initializing Qobuz OAuth client...");
    let client = QobuzClient::new().context("create Qobuz API client")?;
    client
        .init()
        .await
        .context("extract Qobuz web app credentials")?;
    let app_id = client.app_id().await.context("read Qobuz app id")?;

    let listener = tokio::net::TcpListener::bind((bind, 0))
        .await
        .with_context(|| format!("bind OAuth callback listener on {bind}:0"))?;
    let port = listener
        .local_addr()
        .context("read OAuth callback listener address")?
        .port();
    let redirect_host = redirect_host_for_bind(bind);
    let redirect_url = format!("http://{redirect_host}:{port}");
    let oauth_url = format!(
        "https://www.qobuz.com/signin/oauth?ext_app_id={}&redirect_url={}",
        app_id,
        urlencoding::encode(&redirect_url),
    );

    println!("Open this URL to authorize qconnect:");
    println!("{oauth_url}");
    println!("Callback listening on {redirect_url}");

    if !no_browser
    {
        match open::that(&oauth_url)
        {
            Ok(()) => println!("Opened the system browser."),
            Err(err) => println!("Could not open browser automatically: {err}"),
        }
    }

    let code = tokio::time::timeout(timeout, receive_oauth_code(listener))
        .await
        .with_context(|| format!("OAuth login timed out after {}s", timeout.as_secs()))??;

    println!("Authorization code received. Exchanging for Qobuz session...");
    let session = client
        .login_with_oauth_code(&code)
        .await
        .context("exchange OAuth code for Qobuz session")?;

    save_app_id(&app_id)?;
    save_user_auth_token(&session.user_auth_token)?;

    println!("Logged in as {} (user_id: {})", session.display_name, session.user_id);
    println!("Subscription: {}", session.subscription_label);
    if let Some(valid_until) = session.subscription_valid_until.as_deref()
    {
        println!("Subscription valid until: {valid_until}");
    }
    println!("Saved token to {}", user_auth_token_path().display());

    if print_token
    {
        println!("QOBUZ_USER_AUTH_TOKEN={}", session.user_auth_token);
    }

    Ok(())
}

pub fn manual_login(app_id: Option<String>, user_auth_token: Option<String>) -> Result<()>
{
    let app_id = match app_id
    {
        Some(value) => value,
        None => prompt_required("QOBUZ_APP_ID")?,
    };
    let user_auth_token = match user_auth_token
    {
        Some(value) => value,
        None => prompt_required("QOBUZ_USER_AUTH_TOKEN")?,
    };

    save_app_id(&app_id)?;
    save_user_auth_token(&user_auth_token)?;

    println!("Saved app id to {}", app_id_path().display());
    println!("Saved token to {}", user_auth_token_path().display());
    Ok(())
}

pub fn load_saved_app_id() -> Result<Option<String>>
{
    read_optional_secret_file(&app_id_path())
}

pub fn load_saved_user_auth_token() -> Result<Option<String>>
{
    read_optional_secret_file(&user_auth_token_path())
}

#[derive(Debug, Clone)]
pub struct ClientOptions
{
    pub endpoint: Option<String>,
    pub jwt_qws: Option<String>,
    pub app_id: Option<String>,
    pub user_auth_token: Option<String>,
    pub device_name: Option<String>,
    pub device_uuid: Option<String>,
    pub subscribe_channels_hex: Vec<String>,
    pub connect_timeout_ms: u64,
    pub reconnect_backoff_ms: u64,
    pub reconnect_backoff_max_ms: u64,
    pub keepalive_interval_ms: u64,
    pub qcloud_proto: u32,
    pub json: bool,
    pub audio_device: Option<String>,
    pub audio_quality: Quality,
    pub enable_mpris: bool,
}

pub struct QconnectClient
{
    app: Arc<App>,
    sink: Arc<CliEventSink>,
}

impl QconnectClient
{
    pub async fn connect(
        options: ClientOptions,
        enable_renderer: bool,
        enable_audio: bool,
    ) -> Result<Self>
    {
        let device_name = resolve_device_name(options.device_name.as_deref());
        let device_uuid = resolve_device_uuid(options.device_uuid.as_deref())?;
        let printer = Printer::new(options.json);
        let config = resolve_transport_config(&options).await?;
        let (mpris_tx, mpris_rx) = if options.enable_mpris
        {
            let (tx, rx) = mpsc::unbounded_channel();
            (Some(tx), Some(rx))
        }
        else
        {
            (None, None)
        };
        let audio = if enable_audio
        {
            Some(
                AudioPlayback::new(&options, printer.clone(), mpris_tx)
                    .await
                    .context("initialize audio playback")?,
            )
        }
        else
        {
            None
        };

        printer.event(
            "connecting",
            json!({
                "endpoint": config.endpoint_url,
                "device_name": device_name,
                "renderer": enable_renderer,
                "audio": audio.is_some(),
                "mpris": options.enable_mpris
            }),
        );

        let transport = Arc::new(NativeWsTransport::new());
        let sink = Arc::new(CliEventSink::new(printer.clone(), audio));
        let app = Arc::new(QconnectApp::new(transport, Arc::clone(&sink)));
        let transport_rx = app.subscribe_transport_events();

        if let Some(rx) = mpris_rx
        {
            spawn_mpris_command_loop(Arc::clone(&app), rx, printer.clone());
        }

        spawn_transport_event_loop(
            Arc::clone(&app),
            transport_rx,
            printer.clone(),
            device_name.clone(),
            device_uuid.clone(),
            enable_renderer,
        );

        app.connect(config).await?;
        bootstrap_remote_presence(&app, &device_name, &device_uuid).await?;

        Ok(Self { app, sink })
    }

    pub async fn disconnect(&self)
    {
        if let Err(err) = self.app.disconnect().await
        {
            log::debug!("disconnect failed: {err}");
        }
    }

    pub async fn ask_queue_state(&self) -> Result<String>
    {
        self.send_queue_command(QueueCommandType::CtrlSrvrAskForQueueState, json!({}))
            .await
    }

    pub async fn send_queue_command(
        &self,
        command_type: QueueCommandType,
        payload: Value,
    ) -> Result<String>
    {
        let command = self.app.build_queue_command(command_type, payload).await;
        let action_uuid = self.app.send_queue_command(command).await?;
        clear_pending_if_matches(&self.app, &action_uuid).await;
        self.sink.printer.event(
            "command_sent",
            json!({
                "message_type": command_type.as_message_type(),
                "action_uuid": action_uuid
            }),
        );
        Ok(action_uuid)
    }

    pub async fn queue_state(&self) -> QConnectQueueState
    {
        self.app.queue_state_snapshot().await
    }

    pub async fn renderer_state(&self) -> QConnectRendererState
    {
        self.app.renderer_state_snapshot().await
    }

    pub fn spawn_state_reporter(self: &Arc<Self>, interval: Duration)
    {
        let app = Arc::clone(&self.app);
        let sink = Arc::clone(&self.sink);
        tokio::spawn(async move {
            let mut advanced_from_queue_item_id: Option<u64> = None;
            loop
            {
                tokio::time::sleep(interval).await;
                let queue_version = app.queue_state_snapshot().await.version;
                let playback = sink.playback_snapshot().await;
                if playback.finished
                {
                    let renderer = app.renderer_state_snapshot().await;
                    let current_queue_item_id = renderer
                        .current_track
                        .as_ref()
                        .map(|track| track.queue_item_id);
                    if current_queue_item_id.is_some()
                        && current_queue_item_id != advanced_from_queue_item_id
                    {
                        advanced_from_queue_item_id = current_queue_item_id;
                        let target = adjacent_queue_item(&app, 1).await;
                        if target.is_some()
                        {
                            if let Err(err) =
                                send_mpris_player_state(&app, PLAYING_STATE_PLAYING, target).await
                            {
                                log::debug!("automatic next-track command failed: {err}");
                            }
                            continue;
                        }
                    }
                }
                else if playback.playing_state == PLAYING_STATE_PLAYING
                {
                    advanced_from_queue_item_id = None;
                }

                let payload = json!({
                    "playing_state": playback.playing_state,
                    "buffer_state": BUFFER_STATE_OK,
                    "current_position": playback.current_position_ms,
                    "duration": playback.duration_ms,
                    "queue_version": {
                        "major": queue_version.major,
                        "minor": queue_version.minor
                    }
                });
                let report = RendererReport::new(
                    RendererReportType::RndrSrvrStateUpdated,
                    Uuid::new_v4().to_string(),
                    queue_version,
                    payload,
                );
                if let Err(err) = app.send_renderer_report_command(report).await
                {
                    log::debug!("periodic renderer report failed: {err}");
                }
            }
        });
    }
}

fn spawn_transport_event_loop(
    app: Arc<App>,
    mut transport_rx: broadcast::Receiver<TransportEvent>,
    printer: Printer,
    device_name: String,
    device_uuid: String,
    enable_renderer: bool,
)
{
    tokio::spawn(async move {
        let mut renderer_joined = false;
        let mut has_disconnected = false;

        loop
        {
            match transport_rx.recv().await
            {
                Ok(event) =>
                {
                    let session_uuid = match &event
                    {
                        TransportEvent::InboundQueueServerEvent(evt)
                            if enable_renderer
                                && !renderer_joined
                                && evt.message_type() == "MESSAGE_TYPE_SRVR_CTRL_SESSION_STATE" =>
                        {
                            evt.payload
                                .get("session_uuid")
                                .and_then(Value::as_str)
                                .map(ToString::to_string)
                        }
                        _ => None,
                    };
                    if let Some(session_uuid) = session_uuid
                    {
                        renderer_joined = true;
                        deferred_renderer_join(&app, &session_uuid, &device_name, &device_uuid)
                            .await;
                    }

                    match &event
                    {
                        TransportEvent::Connected =>
                        {
                            printer.event("transport_connected", json!({}));
                        }
                        TransportEvent::Disconnected =>
                        {
                            printer.event("transport_disconnected", json!({}));
                            renderer_joined = false;
                            has_disconnected = true;
                        }
                        TransportEvent::Authenticated =>
                        {
                            printer.event("transport_authenticated", json!({}));
                        }
                        TransportEvent::Subscribed =>
                        {
                            printer.event("transport_subscribed", json!({}));
                            if has_disconnected
                            {
                                has_disconnected = false;
                                if let Err(err) =
                                    bootstrap_remote_presence(&app, &device_name, &device_uuid)
                                        .await
                                {
                                    printer.event(
                                        "bootstrap_failed",
                                        json!({ "error": err.to_string() }),
                                    );
                                }
                            }
                        }
                        TransportEvent::ReconnectScheduled {
                            attempt,
                            backoff_ms,
                            reason,
                        } =>
                        {
                            printer.event(
                                "reconnect_scheduled",
                                json!({
                                    "attempt": attempt,
                                    "backoff_ms": backoff_ms,
                                    "reason": reason
                                }),
                            );
                        }
                        TransportEvent::TransportError { stage, message } =>
                        {
                            printer.event(
                                "transport_error",
                                json!({
                                    "stage": stage,
                                    "message": message
                                }),
                            );
                        }
                        _ =>
                        {}
                    }

                    if let Err(err) = app.handle_transport_event(event).await
                    {
                        printer.event("app_event_error", json!({ "error": err.to_string() }));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) =>
                {
                    printer.event("transport_event_lagged", json!({ "count": n }));
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

async fn bootstrap_remote_presence(
    app: &Arc<App>,
    device_name: &str,
    device_uuid: &str,
) -> Result<()>
{
    let device_info = build_device_info(device_name, device_uuid);

    let join_payload = json!({
        "session_uuid": null,
        "device_info": device_info,
    });
    let join_command = app
        .build_queue_command(QueueCommandType::CtrlSrvrJoinSession, join_payload)
        .await;
    let join_uuid = app.send_queue_command(join_command).await?;
    clear_pending_if_matches(app, &join_uuid).await;

    let ask_command = app
        .build_queue_command(QueueCommandType::CtrlSrvrAskForQueueState, json!({}))
        .await;
    let ask_uuid = app.send_queue_command(ask_command).await?;
    clear_pending_if_matches(app, &ask_uuid).await;

    Ok(())
}

async fn deferred_renderer_join(
    app: &Arc<App>,
    session_uuid: &str,
    device_name: &str,
    device_uuid: &str,
)
{
    let device_info = build_device_info(device_name, device_uuid);
    let queue_version = app.queue_state_snapshot().await.version;

    let join_report = RendererReport::new(
        RendererReportType::RndrSrvrJoinSession,
        Uuid::new_v4().to_string(),
        queue_version,
        json!({
            "session_uuid": session_uuid,
            "device_info": device_info,
            "is_active": true,
            "reason": JOIN_SESSION_REASON_CONTROLLER_REQUEST,
            "initial_state": {
                "playing_state": PLAYING_STATE_STOPPED,
                "buffer_state": BUFFER_STATE_OK,
                "current_position": 0,
                "duration": 0,
                "queue_version": {
                    "major": queue_version.major,
                    "minor": queue_version.minor
                }
            }
        }),
    );
    if let Err(err) = app.send_renderer_report_command(join_report).await
    {
        log::error!("renderer join failed: {err}");
        return;
    }

    let state_report = RendererReport::new(
        RendererReportType::RndrSrvrStateUpdated,
        Uuid::new_v4().to_string(),
        queue_version,
        json!({
            "playing_state": PLAYING_STATE_STOPPED,
            "buffer_state": BUFFER_STATE_OK,
            "current_position": 0,
            "duration": 0,
            "queue_version": {
                "major": queue_version.major,
                "minor": queue_version.minor
            }
        }),
    );
    let _ = app.send_renderer_report_command(state_report).await;

    let volume_report = RendererReport::new(
        RendererReportType::RndrSrvrVolumeChanged,
        Uuid::new_v4().to_string(),
        queue_version,
        json!({ "volume": 100 }),
    );
    let _ = app.send_renderer_report_command(volume_report).await;

    let quality_report = RendererReport::new(
        RendererReportType::RndrSrvrMaxAudioQualityChanged,
        Uuid::new_v4().to_string(),
        queue_version,
        json!({ "max_audio_quality": AUDIO_QUALITY_HIRES_LEVEL2 }),
    );
    let _ = app.send_renderer_report_command(quality_report).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    let refresh_command = app
        .build_queue_command(QueueCommandType::CtrlSrvrAskForQueueState, json!({}))
        .await;
    if let Ok(action_uuid) = app.send_queue_command(refresh_command).await
    {
        clear_pending_if_matches(app, &action_uuid).await;
    }
}

async fn clear_pending_if_matches(app: &Arc<App>, action_uuid: &str)
{
    let state_handle = app.state_handle();
    let mut state = state_handle.lock().await;
    let pending_matches = state
        .pending
        .current()
        .map(|pending| pending.uuid == action_uuid)
        .unwrap_or(false);
    if pending_matches
    {
        state.pending.clear();
    }
}

async fn resolve_transport_config(options: &ClientOptions) -> Result<WsTransportConfig>
{
    let mut endpoint = options.endpoint.clone();
    let mut jwt_qws = options.jwt_qws.clone();

    if endpoint.is_none() || jwt_qws.is_none()
    {
        let app_id = options.app_id.as_deref().ok_or_else(|| {
            anyhow!(
                "missing --app-id/QOBUZ_APP_ID; required when --endpoint and --jwt-qws are not both supplied"
            )
        })?;
        let user_auth_token = options.user_auth_token.as_deref().ok_or_else(|| {
            anyhow!(
                "missing --user-auth-token/QOBUZ_USER_AUTH_TOKEN; required when --endpoint and --jwt-qws are not both supplied"
            )
        })?;
        let credentials = fetch_qws_credentials(app_id, user_auth_token).await?;
        endpoint = endpoint.or(Some(credentials.endpoint));
        jwt_qws = jwt_qws.or(Some(credentials.jwt));
    }

    let endpoint_url =
        endpoint.ok_or_else(|| anyhow!("missing Qobuz Connect websocket endpoint"))?;
    let subscribe_channels = parse_subscribe_channels(&options.subscribe_channels_hex)?;

    Ok(WsTransportConfig {
        endpoint_url,
        jwt_qws,
        reconnect_backoff_ms: options.reconnect_backoff_ms,
        reconnect_backoff_max_ms: options.reconnect_backoff_max_ms,
        connect_timeout_ms: options.connect_timeout_ms,
        keepalive_interval_ms: options.keepalive_interval_ms,
        auto_subscribe: true,
        subscribe_channels,
        qcloud_proto: options.qcloud_proto,
    })
}

async fn receive_oauth_code(listener: tokio::net::TcpListener) -> Result<String>
{
    loop
    {
        let (mut stream, _) = listener
            .accept()
            .await
            .context("accept OAuth callback connection")?;
        let mut buffer = vec![0_u8; 8192];
        let read = stream
            .read(&mut buffer)
            .await
            .context("read OAuth callback request")?;
        let request = String::from_utf8_lossy(&buffer[..read]);
        let code = parse_oauth_code_from_request(&request);
        let body = if code.is_some()
        {
            oauth_html("Login successful", "You can close this tab and return to qconnect.")
        }
        else
        {
            oauth_html(
                "Login failed",
                "No authorization code was received. Return to qconnect and try again.",
            )
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;

        if let Some(code) = code
        {
            return Ok(code);
        }
    }
}

fn parse_oauth_code_from_request(request: &str) -> Option<String>
{
    let request_target = request.lines().next()?.split_whitespace().nth(1)?;
    let query = request_target.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        if key == "code_autorisation" || key == "code"
        {
            urlencoding::decode(value)
                .ok()
                .map(|decoded| decoded.into_owned())
        }
        else
        {
            None
        }
    })
}

fn oauth_html(title: &str, message: &str) -> String
{
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body style=\"font-family:system-ui,sans-serif;text-align:center;padding:64px\">\
         <h2>{title}</h2><p>{message}</p></body></html>"
    )
}

fn redirect_host_for_bind(bind: &str) -> String
{
    match bind
    {
        "127.0.0.1" => "127.0.0.1".to_string(),
        "::1" => "[::1]".to_string(),
        "localhost" => "localhost".to_string(),
        "0.0.0.0" | "::" => detect_lan_ip().unwrap_or_else(|| "localhost".to_string()),
        other => other.to_string(),
    }
}

fn detect_lan_ip() -> Option<String>
{
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    let ip = addr.ip().to_string();
    (ip != "0.0.0.0" && ip != "127.0.0.1").then_some(ip)
}

struct QwsCredentials
{
    endpoint: String,
    jwt: String,
}

async fn fetch_qws_credentials(app_id: &str, user_auth_token: &str) -> Result<QwsCredentials>
{
    let response = reqwest::Client::new()
        .post("https://www.qobuz.com/api.json/0.2/qws/createToken")
        .header("X-App-Id", app_id)
        .header("X-User-Auth-Token", user_auth_token)
        .form(&[
            ("jwt", "jwt_qws"),
            ("user_auth_token_needed", "true"),
            ("strong_auth_needed", "true"),
        ])
        .send()
        .await
        .context("qws/createToken request failed")?;

    let status = response.status();
    let body: Value = response
        .json()
        .await
        .context("qws/createToken response was not JSON")?;

    if !status.is_success()
    {
        let preview = serde_json::to_string(&body)
            .unwrap_or_else(|_| "<unserializable>".to_string())
            .chars()
            .take(300)
            .collect::<String>();
        bail!("qws/createToken returned {status}: {preview}");
    }

    let jwt_qws = body
        .get("jwt_qws")
        .ok_or_else(|| anyhow!("qws/createToken response missing jwt_qws"))?;
    let endpoint = jwt_qws
        .get("endpoint")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("qws/createToken response missing jwt_qws.endpoint"))?
        .to_string();
    let jwt = jwt_qws
        .get("jwt")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("qws/createToken response missing jwt_qws.jwt"))?
        .to_string();

    Ok(QwsCredentials { endpoint, jwt })
}

fn parse_subscribe_channels(values: &[String]) -> Result<Vec<Vec<u8>>>
{
    if values.is_empty()
    {
        return Ok(vec![vec![0x01], vec![0x02], vec![0x03]]);
    }

    values
        .iter()
        .map(|value| {
            let cleaned = value
                .trim()
                .strip_prefix("0x")
                .or_else(|| value.trim().strip_prefix("0X"))
                .unwrap_or_else(|| value.trim())
                .replace([' ', ':', '-'], "");
            if cleaned.is_empty()
            {
                bail!("empty subscribe channel hex value");
            }
            if cleaned.len() % 2 != 0
            {
                bail!("subscribe channel hex must have an even number of digits: {value}");
            }
            hex::decode(&cleaned).with_context(|| format!("invalid subscribe channel hex: {value}"))
        })
        .collect()
}

fn build_device_info(device_name: &str, device_uuid: &str) -> Value
{
    json!({
        "device_uuid": device_uuid,
        "friendly_name": device_name,
        "brand": "qconnect",
        "model": "qconnect-cli",
        "serial_number": null,
        "device_type": 5,
        "capabilities": {
            "min_audio_quality": AUDIO_QUALITY_MP3,
            "max_audio_quality": AUDIO_QUALITY_HIRES_LEVEL2,
            "volume_remote_control": VOLUME_REMOTE_CONTROL_ALLOWED
        },
        "software_version": format!("qconnect-cli/{}", env!("CARGO_PKG_VERSION")),
    })
}

fn resolve_device_name(explicit: Option<&str>) -> String
{
    explicit
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| std::env::var("HOSTNAME").ok())
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .map(|hostname| format!("qconnect-cli ({hostname})"))
        .unwrap_or_else(|| "qconnect-cli".to_string())
}

fn resolve_device_uuid(explicit: Option<&str>) -> Result<String>
{
    if let Some(value) = explicit
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
    {
        return Ok(value);
    }

    let path = device_uuid_path();
    if let Ok(existing) = std::fs::read_to_string(&path)
    {
        let trimmed = existing.trim();
        if !trimmed.is_empty()
        {
            return Ok(trimmed.to_string());
        }
    }

    let uuid = Uuid::new_v4().to_string();
    if let Some(parent) = path.parent()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config dir {}", parent.display()))?;
    }
    std::fs::write(&path, &uuid)
        .with_context(|| format!("write device uuid {}", path.display()))?;
    Ok(uuid)
}

fn device_uuid_path() -> PathBuf
{
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("qconnect")
        .join("device_uuid")
}

fn app_id_path() -> PathBuf
{
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("qconnect")
        .join("app_id")
}

fn user_auth_token_path() -> PathBuf
{
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("qconnect")
        .join("user_auth_token")
}

fn read_optional_secret_file(path: &PathBuf) -> Result<Option<String>>
{
    match std::fs::read_to_string(path)
    {
        Ok(value) =>
        {
            let trimmed = value.trim().to_string();
            Ok((!trimmed.is_empty()).then_some(trimmed))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn prompt_required(name: &str) -> Result<String>
{
    println!("Paste {name}:");
    let mut value = String::new();
    std::io::stdin()
        .read_line(&mut value)
        .with_context(|| format!("read {name} from stdin"))?;
    let trimmed = value.trim().to_string();
    if trimmed.is_empty()
    {
        bail!("{name} cannot be empty");
    }
    Ok(trimmed)
}

fn save_app_id(app_id: &str) -> Result<()>
{
    write_secret_file(&app_id_path(), app_id)
}

fn save_user_auth_token(token: &str) -> Result<()>
{
    write_secret_file(&user_auth_token_path(), token)
}

fn write_secret_file(path: &PathBuf, value: &str) -> Result<()>
{
    if let Some(parent) = path.parent()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config dir {}", parent.display()))?;
    }
    std::fs::write(path, value).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict permissions {}", path.display()))?;
    }
    Ok(())
}

#[derive(Clone)]
struct Printer
{
    json: bool,
}

impl Printer
{
    fn new(json: bool) -> Self
    {
        Self { json }
    }

    fn event(&self, name: &str, payload: Value)
    {
        if self.json
        {
            println!(
                "{}",
                json!({
                    "event": name,
                    "payload": payload
                })
            );
            return;
        }

        match name
        {
            "connecting" =>
            {
                println!(
                    "connecting to {} as {}{}",
                    payload
                        .get("endpoint")
                        .and_then(Value::as_str)
                        .unwrap_or("<unknown>"),
                    payload
                        .get("device_name")
                        .and_then(Value::as_str)
                        .unwrap_or("qconnect-cli"),
                    if payload
                        .get("renderer")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        " (renderer)"
                    }
                    else
                    {
                        ""
                    }
                );
            }
            "queue_updated" =>
            {
                let version = payload.get("version").unwrap_or(&Value::Null);
                let tracks = payload
                    .get("tracks")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                println!(
                    "queue v{}.{} [{}]",
                    version.get("major").and_then(Value::as_u64).unwrap_or(0),
                    version.get("minor").and_then(Value::as_u64).unwrap_or(0),
                    tracks
                );
            }
            "renderer_command" =>
            {
                println!(
                    "renderer command: {}",
                    payload
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("<unknown>")
                );
            }
            "session_event" =>
            {
                println!(
                    "session event: {}",
                    payload
                        .get("message_type")
                        .and_then(Value::as_str)
                        .unwrap_or("<unknown>")
                );
            }
            "command_sent" =>
            {
                println!(
                    "sent {} ({})",
                    payload
                        .get("message_type")
                        .and_then(Value::as_str)
                        .unwrap_or("<unknown>"),
                    payload
                        .get("action_uuid")
                        .and_then(Value::as_str)
                        .unwrap_or("<no action>")
                );
            }
            other =>
            {
                if payload == json!({})
                {
                    println!("{other}");
                }
                else
                {
                    println!("{other}: {payload}");
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MprisCommand
{
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
}

fn start_mpris(
    printer: &Printer,
    sender: mpsc::UnboundedSender<MprisCommand>,
) -> Option<Arc<StdMutex<MediaControls>>>
{
    let config = PlatformConfig {
        dbus_name: "com.qconnect.cli",
        display_name: "qconnect",
        hwnd: None,
    };

    let mut controls = match MediaControls::new(config)
    {
        Ok(controls) => controls,
        Err(err) =>
        {
            printer.event("mpris_error", json!({ "error": format!("{err:?}") }));
            return None;
        }
    };

    let event_sender = sender.clone();
    if let Err(err) = controls.attach(move |event| {
        let command = match event
        {
            MediaControlEvent::Play => Some(MprisCommand::Play),
            MediaControlEvent::Pause => Some(MprisCommand::Pause),
            MediaControlEvent::Toggle => Some(MprisCommand::Toggle),
            MediaControlEvent::Stop => Some(MprisCommand::Stop),
            MediaControlEvent::Next => Some(MprisCommand::Next),
            MediaControlEvent::Previous => Some(MprisCommand::Previous),
            _ => None,
        };
        if let Some(command) = command
        {
            let _ = event_sender.send(command);
        }
    })
    {
        printer.event("mpris_error", json!({ "error": format!("{err:?}") }));
        return None;
    }

    let _ = controls.set_playback(MediaPlayback::Stopped);
    printer.event("mpris_ready", json!({ "dbus_name": "com.qconnect.cli" }));
    Some(Arc::new(StdMutex::new(controls)))
}

fn update_mpris_metadata(controls: &Arc<StdMutex<MediaControls>>, track: &Track)
{
    let title = track.title.as_str();
    let artist = track
        .performer
        .as_ref()
        .map(|artist| artist.name.as_str())
        .unwrap_or("");
    let album = track
        .album
        .as_ref()
        .map(|album| album.title.as_str())
        .unwrap_or("");

    if let Ok(mut controls) = controls.lock()
    {
        let _ = controls.set_metadata(MediaMetadata {
            title: Some(title),
            artist: Some(artist),
            album: Some(album),
            duration: Some(Duration::from_secs(track.duration as u64)),
            ..Default::default()
        });
    }
}

fn update_mpris_playback(
    controls: &Arc<StdMutex<MediaControls>>,
    playing_state: i32,
    position_secs: u64,
)
{
    let position = Some(MediaPosition(Duration::from_secs(position_secs)));
    let playback = match playing_state
    {
        PLAYING_STATE_PLAYING => MediaPlayback::Playing { progress: position },
        PLAYING_STATE_PAUSED => MediaPlayback::Paused { progress: position },
        _ => MediaPlayback::Stopped,
    };

    if let Ok(mut controls) = controls.lock()
    {
        let _ = controls.set_playback(playback);
    }
}

fn spawn_mpris_command_loop(
    app: Arc<App>,
    mut rx: mpsc::UnboundedReceiver<MprisCommand>,
    printer: Printer,
)
{
    tokio::spawn(async move {
        while let Some(command) = rx.recv().await
        {
            if let Err(err) = handle_mpris_command(&app, command).await
            {
                printer.event(
                    "mpris_command_error",
                    json!({
                        "command": format!("{command:?}"),
                        "error": err.to_string()
                    }),
                );
            }
        }
    });
}

async fn handle_mpris_command(app: &Arc<App>, command: MprisCommand) -> Result<()>
{
    match command
    {
        MprisCommand::Play => send_mpris_player_state(app, PLAYING_STATE_PLAYING, None).await,
        MprisCommand::Pause => send_mpris_player_state(app, PLAYING_STATE_PAUSED, None).await,
        MprisCommand::Stop => send_mpris_player_state(app, PLAYING_STATE_STOPPED, None).await,
        MprisCommand::Toggle =>
        {
            let renderer = app.renderer_state_snapshot().await;
            let next_state = if renderer.playing_state == Some(PLAYING_STATE_PLAYING)
            {
                PLAYING_STATE_PAUSED
            }
            else
            {
                PLAYING_STATE_PLAYING
            };
            send_mpris_player_state(app, next_state, None).await
        }
        MprisCommand::Next =>
        {
            let target = adjacent_queue_item(app, 1).await;
            send_mpris_player_state(app, PLAYING_STATE_PLAYING, target).await
        }
        MprisCommand::Previous =>
        {
            let target = adjacent_queue_item(app, -1).await;
            send_mpris_player_state(app, PLAYING_STATE_PLAYING, target).await
        }
    }
}

async fn adjacent_queue_item(app: &Arc<App>, direction: i32) -> Option<QueueItem>
{
    let renderer = app.renderer_state_snapshot().await;
    let current = renderer.current_track?;
    let queue = app.queue_state_snapshot().await;
    let ordered = ordered_queue_items(&queue);
    let current_index = ordered
        .iter()
        .position(|item| item.queue_item_id == current.queue_item_id)?;
    let next_index = current_index as i32 + direction;
    if next_index < 0
    {
        return None;
    }
    ordered.get(next_index as usize).cloned()
}

fn ordered_queue_items(queue: &QConnectQueueState) -> Vec<QueueItem>
{
    if queue.shuffle_mode
    {
        if let Some(order) = &queue.shuffle_order
        {
            return order
                .iter()
                .filter_map(|index| queue.queue_items.get(*index).cloned())
                .collect();
        }
    }
    queue.queue_items.clone()
}

async fn send_mpris_player_state(
    app: &Arc<App>,
    playing_state: i32,
    target_track: Option<QueueItem>,
) -> Result<()>
{
    let renderer = app.renderer_state_snapshot().await;
    let queue = app.queue_state_snapshot().await;
    let has_explicit_target = target_track.is_some();
    let mut current_track = target_track.or(renderer.current_track);
    if current_track.is_none() && playing_state == PLAYING_STATE_PLAYING
    {
        current_track = ordered_queue_items(&queue).first().cloned();
    }
    let current_position = if has_explicit_target
    {
        Some(0)
    }
    else
    {
        renderer
            .current_position_ms
            .and_then(|value| i32::try_from(value).ok())
    };

    let current_queue_item = current_track.as_ref().map(|item| {
        json!({
            "queue_version": {
                "major": queue.version.major,
                "minor": queue.version.minor
            },
            "id": item.queue_item_id
        })
    });

    let command = app
        .build_queue_command(
            QueueCommandType::CtrlSrvrSetPlayerState,
            json!({
                "playing_state": playing_state,
                "current_position": current_position,
                "current_queue_item": current_queue_item
            }),
        )
        .await;
    let action_uuid = app.send_queue_command(command).await?;
    clear_pending_if_matches(app, &action_uuid).await;
    Ok(())
}

struct AudioPlayback
{
    client: QobuzClient,
    player: Arc<Player>,
    printer: Printer,
    preferred_quality: Quality,
    loading: Mutex<Option<JoinHandle<()>>>,
    mpris: Option<Arc<StdMutex<MediaControls>>>,
}

impl AudioPlayback
{
    async fn new(
        options: &ClientOptions,
        printer: Printer,
        mpris_sender: Option<mpsc::UnboundedSender<MprisCommand>>,
    ) -> Result<Arc<Self>>
    {
        let token = options.user_auth_token.as_deref().ok_or_else(|| {
            anyhow!(
                "audio playback requires --user-auth-token/QOBUZ_USER_AUTH_TOKEN; use serve --no-audio for state-only rendering"
            )
        })?;

        let client = QobuzClient::new().context("create Qobuz playback client")?;
        client
            .init()
            .await
            .context("extract Qobuz playback credentials")?;
        client
            .login_with_token(token)
            .await
            .context("restore Qobuz playback session")?;

        let player = Player::new(
            options.audio_device.clone(),
            AudioSettings::default(),
            None,
            AudioDiagnostic::new(),
        );
        let mpris = mpris_sender.and_then(|sender| start_mpris(&printer, sender));

        printer.event(
            "audio_ready",
            json!({
                "device": options.audio_device,
                "quality": options.audio_quality.label()
            }),
        );

        Ok(Arc::new(Self {
            client,
            player: Arc::new(player),
            printer,
            preferred_quality: options.audio_quality,
            loading: Mutex::new(None),
            mpris,
        }))
    }

    async fn play(&self, track: QueueItem, position_ms: u64, max_audio_quality: i32)
    {
        let remote_max_quality =
            connect_quality_to_qobuz(max_audio_quality).unwrap_or(self.preferred_quality);
        let quality = lower_quality(self.preferred_quality, remote_max_quality);
        let current = self.player.get_playback_event();
        if current.track_id == track.track_id && self.player.has_loaded_audio()
        {
            if position_ms > 0
            {
                if let Err(err) = self.player.seek(position_ms / 1000)
                {
                    self.printer
                        .event("audio_error", json!({ "error": err, "track_id": track.track_id }));
                }
            }
            if let Err(err) = self.player.resume()
            {
                self.printer
                    .event("audio_error", json!({ "error": err, "track_id": track.track_id }));
            }
            return;
        }

        self.replace_loading_task(track, position_ms, quality).await;
    }

    async fn replace_loading_task(&self, track: QueueItem, position_ms: u64, quality: Quality)
    {
        if let Some(handle) = self.loading.lock().await.take()
        {
            handle.abort();
        }

        let player = Arc::clone(&self.player);
        let client = self.client.clone();
        let printer = self.printer.clone();
        let mpris = self.mpris.clone();
        let track_id = track.track_id;
        let handle = tokio::spawn(async move {
            printer.event(
                "audio_loading",
                json!({
                    "track_id": track_id,
                    "queue_item_id": track.queue_item_id,
                    "quality": quality.label()
                }),
            );
            if let Some(mpris) = mpris.clone()
            {
                let client = client.clone();
                tokio::spawn(async move {
                    if let Ok(track) = client.get_track(track_id).await
                    {
                        update_mpris_metadata(&mpris, &track);
                    }
                });
            }
            match player.play_track(&client, track_id, quality).await
            {
                Ok(()) =>
                {
                    if position_ms > 0
                    {
                        if let Err(err) = player.seek(position_ms / 1000)
                        {
                            printer.event(
                                "audio_error",
                                json!({ "error": err, "track_id": track_id }),
                            );
                        }
                    }
                    printer.event("audio_started", json!({ "track_id": track_id }));
                }
                Err(err) =>
                {
                    printer.event("audio_error", json!({ "error": err, "track_id": track_id }));
                }
            }
        });
        *self.loading.lock().await = Some(handle);
    }

    async fn pause(&self)
    {
        if let Err(err) = self.player.pause()
        {
            self.printer.event("audio_error", json!({ "error": err }));
        }
    }

    async fn resume(&self)
    {
        if let Err(err) = self.player.resume()
        {
            self.printer.event("audio_error", json!({ "error": err }));
        }
    }

    async fn stop(&self)
    {
        if let Some(handle) = self.loading.lock().await.take()
        {
            handle.abort();
        }
        if let Err(err) = self.player.stop()
        {
            self.printer.event("audio_error", json!({ "error": err }));
        }
    }

    async fn seek(&self, position_ms: u64)
    {
        if let Err(err) = self.player.seek(position_ms / 1000)
        {
            self.printer.event("audio_error", json!({ "error": err }));
        }
    }

    async fn set_volume(&self, volume: i32, muted: bool)
    {
        let player_volume = if muted
        {
            0.0
        }
        else
        {
            volume.clamp(0, 100) as f32 / 100.0
        };
        if let Err(err) = self.player.set_volume(player_volume)
        {
            self.printer.event("audio_error", json!({ "error": err }));
        }
    }

    fn snapshot(&self, fallback: &PlaybackState) -> PlaybackSnapshot
    {
        let event = self.player.get_playback_event();
        if event.track_id == 0
        {
            let snapshot = fallback.snapshot();
            if let Some(mpris) = &self.mpris
            {
                update_mpris_playback(
                    mpris,
                    snapshot.playing_state,
                    snapshot.current_position_ms / 1000,
                );
            }
            return snapshot;
        }

        let event_matches_renderer_track = fallback
            .current_track
            .as_ref()
            .map(|track| track.track_id == event.track_id)
            .unwrap_or(false);
        let finished = event_matches_renderer_track
            && !event.is_playing
            && fallback.playing_state == PLAYING_STATE_PLAYING
            && event.duration > 0
            && event.position >= event.duration;
        let snapshot = PlaybackSnapshot {
            playing_state: if event.is_playing
            {
                PLAYING_STATE_PLAYING
            }
            else if finished
            {
                PLAYING_STATE_STOPPED
            }
            else
            {
                fallback.playing_state
            },
            current_position_ms: event.position.saturating_mul(1000),
            duration_ms: event.duration.saturating_mul(1000),
            finished,
        };
        if let Some(mpris) = &self.mpris
        {
            update_mpris_playback(mpris, snapshot.playing_state, event.position);
        }
        snapshot
    }
}

fn connect_quality_to_qobuz(max_audio_quality: i32) -> Option<Quality>
{
    match max_audio_quality
    {
        value if value <= 0 => None,
        1 => Some(Quality::Mp3),
        2 => Some(Quality::Lossless),
        3 => Some(Quality::HiRes),
        _ => Some(Quality::UltraHiRes),
    }
}

fn lower_quality(left: Quality, right: Quality) -> Quality
{
    if quality_rank(left) <= quality_rank(right)
    {
        left
    }
    else
    {
        right
    }
}

fn quality_rank(quality: Quality) -> u8
{
    match quality
    {
        Quality::Mp3 => 1,
        Quality::Lossless => 2,
        Quality::HiRes => 3,
        Quality::UltraHiRes => 4,
    }
}

enum AudioAction
{
    Play
    {
        track: QueueItem,
        position_ms: u64,
        max_audio_quality: i32,
    },
    Resume,
    Pause,
    Stop,
    Seek(u64),
    Volume
    {
        volume: i32,
        muted: bool,
    },
}

struct CliEventSink
{
    printer: Printer,
    playback: Mutex<PlaybackState>,
    audio: Option<Arc<AudioPlayback>>,
}

impl CliEventSink
{
    fn new(printer: Printer, audio: Option<Arc<AudioPlayback>>) -> Self
    {
        Self {
            printer,
            playback: Mutex::new(PlaybackState::default()),
            audio,
        }
    }

    async fn playback_snapshot(&self) -> PlaybackSnapshot
    {
        let playback = self.playback.lock().await;
        match &self.audio
        {
            Some(audio) => audio.snapshot(&playback),
            None => playback.snapshot(),
        }
    }
}

#[async_trait]
impl QconnectEventSink for CliEventSink
{
    async fn on_event(&self, event: QconnectAppEvent)
    {
        match event
        {
            QconnectAppEvent::QueueUpdated(queue) =>
            {
                let tracks = queue
                    .queue_items
                    .iter()
                    .map(|item| format!("{}#{}", item.track_id, item.queue_item_id))
                    .collect::<Vec<_>>();
                self.printer.event(
                    "queue_updated",
                    json!({
                        "version": {
                            "major": queue.version.major,
                            "minor": queue.version.minor
                        },
                        "shuffle_mode": queue.shuffle_mode,
                        "autoplay_mode": queue.autoplay_mode,
                        "tracks": tracks,
                        "track_count": queue.queue_items.len()
                    }),
                );
            }
            QconnectAppEvent::RendererCommandApplied { command, .. } =>
            {
                self.apply_renderer_command(&command).await;
                self.printer.event(
                    "renderer_command",
                    json!({
                        "command": format!("{command:?}")
                    }),
                );
            }
            QconnectAppEvent::RendererUpdated(renderer) =>
            {
                let mut playback = self.playback.lock().await;
                if let Some(track) = renderer.current_track
                {
                    playback.current_track = Some(track);
                }
                if let Some(track) = renderer.next_track
                {
                    playback.next_track = Some(track);
                }
            }
            QconnectAppEvent::SessionManagementEvent {
                message_type,
                payload,
            } =>
            {
                self.printer.event(
                    "session_event",
                    json!({
                        "message_type": message_type,
                        "payload": payload
                    }),
                );
            }
            QconnectAppEvent::PendingActionTimedOut { uuid, timeout_ms } =>
            {
                self.printer.event(
                    "pending_timeout",
                    json!({
                        "uuid": uuid,
                        "timeout_ms": timeout_ms
                    }),
                );
            }
            _ =>
            {}
        }
    }
}

impl CliEventSink
{
    async fn apply_renderer_command(&self, command: &RendererCommand)
    {
        let mut playback = self.playback.lock().await;
        let action = match command
        {
            RendererCommand::SetState {
                playing_state,
                current_position_ms,
                current_track,
                next_track,
            } =>
            {
                if let Some(value) = playing_state
                {
                    playback.playing_state = *value;
                }
                if let Some(value) = current_position_ms
                {
                    playback.current_position_ms = *value;
                }
                if let Some(track) = current_track
                {
                    playback.current_track = Some(track.clone());
                }
                if let Some(track) = next_track
                {
                    playback.next_track = Some(track.clone());
                }
                playback.updated_at = Instant::now();
                match playing_state
                {
                    Some(PLAYING_STATE_PLAYING) =>
                    {
                        if let Some(track) = playback.current_track.clone()
                        {
                            Some(AudioAction::Play {
                                track,
                                position_ms: playback.current_position_ms,
                                max_audio_quality: playback.max_audio_quality,
                            })
                        }
                        else
                        {
                            Some(AudioAction::Resume)
                        }
                    }
                    Some(PLAYING_STATE_PAUSED) => Some(AudioAction::Pause),
                    Some(PLAYING_STATE_STOPPED) => Some(AudioAction::Stop),
                    _ => current_position_ms.map(AudioAction::Seek),
                }
            }
            RendererCommand::SetVolume {
                volume,
                volume_delta,
            } =>
            {
                if let Some(value) = volume
                {
                    playback.volume = (*value).clamp(0, 100);
                }
                if let Some(delta) = volume_delta
                {
                    playback.volume = playback.volume.saturating_add(*delta).clamp(0, 100);
                }
                Some(AudioAction::Volume {
                    volume: playback.volume,
                    muted: playback.muted,
                })
            }
            RendererCommand::MuteVolume { value } =>
            {
                playback.muted = *value;
                Some(AudioAction::Volume {
                    volume: playback.volume,
                    muted: playback.muted,
                })
            }
            RendererCommand::SetMaxAudioQuality { max_audio_quality } =>
            {
                playback.max_audio_quality = *max_audio_quality;
                None
            }
            RendererCommand::SetActive { .. }
            | RendererCommand::SetLoopMode { .. }
            | RendererCommand::SetShuffleMode { .. } => None,
        };
        drop(playback);

        if let Some(audio) = &self.audio
        {
            match action
            {
                Some(AudioAction::Play {
                    track,
                    position_ms,
                    max_audio_quality,
                }) => audio.play(track, position_ms, max_audio_quality).await,
                Some(AudioAction::Resume) => audio.resume().await,
                Some(AudioAction::Pause) => audio.pause().await,
                Some(AudioAction::Stop) => audio.stop().await,
                Some(AudioAction::Seek(position_ms)) => audio.seek(position_ms).await,
                Some(AudioAction::Volume { volume, muted }) =>
                {
                    audio.set_volume(volume, muted).await
                }
                None =>
                {}
            }
        }
    }
}

#[derive(Debug, Clone)]
struct PlaybackState
{
    playing_state: i32,
    current_position_ms: u64,
    duration_ms: u64,
    current_track: Option<QueueItem>,
    next_track: Option<QueueItem>,
    volume: i32,
    muted: bool,
    max_audio_quality: i32,
    updated_at: Instant,
}

impl Default for PlaybackState
{
    fn default() -> Self
    {
        Self {
            playing_state: PLAYING_STATE_STOPPED,
            current_position_ms: 0,
            duration_ms: 0,
            current_track: None,
            next_track: None,
            volume: 100,
            muted: false,
            max_audio_quality: AUDIO_QUALITY_HIRES_LEVEL2,
            updated_at: Instant::now(),
        }
    }
}

impl PlaybackState
{
    fn snapshot(&self) -> PlaybackSnapshot
    {
        let elapsed = if self.playing_state == PLAYING_STATE_PLAYING
        {
            self.updated_at.elapsed().as_millis() as u64
        }
        else
        {
            0
        };
        PlaybackSnapshot {
            playing_state: self.playing_state,
            current_position_ms: self.current_position_ms.saturating_add(elapsed),
            duration_ms: self.duration_ms,
            finished: false,
        }
    }
}

#[derive(Debug, Clone)]
struct PlaybackSnapshot
{
    playing_state: i32,
    current_position_ms: u64,
    duration_ms: u64,
    finished: bool,
}

#[allow(dead_code)]
const _: i32 = PLAYING_STATE_PAUSED;
