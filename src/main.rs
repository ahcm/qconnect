mod qconnect;

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use qconnect::{ClientOptions, QconnectClient};
use qconnect_app::QueueCommandType;
use serde_json::json;
use tokio::time::sleep;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Standalone Qobuz Connect command-line client"
)]
struct Cli
{
    /// Qobuz Connect websocket endpoint. If omitted, qws/createToken is used.
    #[arg(long, env = "QBZ_QCONNECT_WS_ENDPOINT")]
    endpoint: Option<String>,

    /// Qobuz Connect JWT. If omitted, qws/createToken is used.
    #[arg(long, env = "QBZ_QCONNECT_JWT_QWS", alias = "jwt")]
    jwt_qws: Option<String>,

    /// Qobuz API app id used for qws/createToken.
    #[arg(long, env = "QOBUZ_APP_ID")]
    app_id: Option<String>,

    /// Qobuz user auth token used for qws/createToken.
    #[arg(long, env = "QOBUZ_USER_AUTH_TOKEN")]
    user_auth_token: Option<String>,

    /// Friendly name shown to other Qobuz Connect controllers.
    #[arg(long, env = "QCONNECT_DEVICE_NAME")]
    device_name: Option<String>,

    /// Persistent device UUID. Defaults to one stored under the user config dir.
    #[arg(long, env = "QCONNECT_DEVICE_UUID")]
    device_uuid: Option<String>,

    /// Subscribe channel hex values, comma-separated when read from env.
    #[arg(
        long = "subscribe-channel",
        env = "QBZ_QCONNECT_SUBSCRIBE_CHANNELS_HEX",
        value_delimiter = ','
    )]
    subscribe_channels_hex: Vec<String>,

    /// Emit machine-readable JSON events.
    #[arg(long)]
    json: bool,

    /// Websocket connect timeout in milliseconds.
    #[arg(long, default_value_t = 10_000)]
    connect_timeout_ms: u64,

    /// Initial reconnect backoff in milliseconds.
    #[arg(long, default_value_t = 2_000)]
    reconnect_backoff_ms: u64,

    /// Maximum reconnect backoff in milliseconds.
    #[arg(long, default_value_t = 30_000)]
    reconnect_backoff_max_ms: u64,

    /// Keepalive ping interval in milliseconds.
    #[arg(long, default_value_t = 30_000)]
    keepalive_interval_ms: u64,

    /// QCloud protocol version.
    #[arg(long, default_value_t = 1)]
    qcloud_proto: u32,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command
{
    /// Obtain or save Qobuz credentials.
    Login
    {
        /// Save pasted credentials instead of running browser OAuth.
        #[arg(long)]
        manual: bool,
        /// Qobuz API app id to save for this client.
        #[arg(long)]
        app_id: Option<String>,
        /// Qobuz user auth token to save for this client.
        #[arg(long)]
        user_auth_token: Option<String>,
        /// Bind address for the temporary OAuth callback listener.
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// Seconds to wait for the browser callback.
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,
        /// Print the raw token after login.
        #[arg(long)]
        print_token: bool,
        /// Print the browser URL without attempting to open it.
        #[arg(long)]
        no_browser: bool,
    },
    /// Join Qobuz Connect and expose this process as a headless renderer.
    Serve
    {
        /// Periodic renderer state report interval.
        #[arg(long, default_value_t = 2)]
        report_interval_secs: u64,
    },
    /// Connect, request state, print events, and exit.
    Status
    {
        /// Seconds to wait for server events before exiting.
        #[arg(long, default_value_t = 5)]
        wait_secs: u64,
    },
    /// Request and print the remote queue.
    Queue
    {
        /// Seconds to wait for queue events before exiting.
        #[arg(long, default_value_t = 5)]
        wait_secs: u64,
    },
    /// Replace the remote queue with Qobuz track ids.
    Load
    {
        track_ids: Vec<u64>,
        #[arg(long, default_value_t = 0)]
        start_index: usize,
        #[arg(long)]
        shuffle: bool,
        #[arg(long, default_value_t = 5)]
        wait_secs: u64,
    },
    /// Append Qobuz track ids to the remote queue.
    Add
    {
        track_ids: Vec<u64>,
        #[arg(long, default_value_t = 5)]
        wait_secs: u64,
    },
    /// Insert Qobuz track ids after a queue item id, or at the front when omitted.
    Insert
    {
        track_ids: Vec<u64>,
        #[arg(long)]
        after_queue_item_id: Option<u64>,
        #[arg(long, default_value_t = 5)]
        wait_secs: u64,
    },
    /// Clear the remote queue.
    Clear
    {
        #[arg(long, default_value_t = 5)]
        wait_secs: u64,
    },
    /// Tell the active renderer to play.
    Play
    {
        #[arg(long, default_value_t = 5)]
        wait_secs: u64,
    },
    /// Tell the active renderer to pause.
    Pause
    {
        #[arg(long, default_value_t = 5)]
        wait_secs: u64,
    },
    /// Tell the active renderer to stop.
    Stop
    {
        #[arg(long, default_value_t = 5)]
        wait_secs: u64,
    },
    /// Set or adjust active renderer volume.
    Volume
    {
        #[arg(long)]
        set: Option<i32>,
        #[arg(long)]
        delta: Option<i32>,
        #[arg(long)]
        mute: Option<bool>,
        #[arg(long, default_value_t = 5)]
        wait_secs: u64,
    },
    /// Enable or disable shuffle.
    Shuffle
    {
        mode: Toggle,
        #[arg(long, default_value_t = 5)]
        wait_secs: u64,
    },
    /// Set repeat mode.
    Repeat
    {
        mode: RepeatMode,
        #[arg(long, default_value_t = 5)]
        wait_secs: u64,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Toggle
{
    On,
    Off,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RepeatMode
{
    Off,
    One,
    All,
}

#[tokio::main]
async fn main() -> Result<()>
{
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();
    let command = cli.command;

    if let Command::Login {
        manual,
        app_id,
        user_auth_token,
        bind,
        timeout_secs,
        print_token,
        no_browser,
    } = &command
    {
        let pasted_app_id = first_non_empty([
            app_id.clone(),
            cli.app_id.clone(),
            std::env::var("QBZ_QOBUZ_APP_ID").ok(),
            std::env::var("QOBUZ_APP_ID").ok(),
        ]);
        let pasted_user_auth_token = first_non_empty([
            user_auth_token.clone(),
            cli.user_auth_token.clone(),
            std::env::var("QBZ_QOBUZ_USER_AUTH_TOKEN").ok(),
            std::env::var("QOBUZ_USER_AUTH_TOKEN").ok(),
        ]);

        if *manual || pasted_app_id.is_some() || pasted_user_auth_token.is_some()
        {
            qconnect::manual_login(pasted_app_id, pasted_user_auth_token)
                .context("save pasted Qobuz credentials")?;
            return Ok(());
        }

        qconnect::browser_login(
            bind,
            Duration::from_secs(*timeout_secs),
            *print_token,
            *no_browser,
        )
        .await
        .context("Qobuz browser login")?;
        return Ok(());
    }

    let enable_renderer = matches!(command, Command::Serve { .. });
    let wait_after_connect = wait_secs_for(&command);

    let options = ClientOptions {
        endpoint: first_non_empty([
            cli.endpoint,
            std::env::var("QCONNECT_ENDPOINT").ok(),
            std::env::var("QBZ_QCONNECT_WS_ENDPOINT").ok(),
        ]),
        jwt_qws: first_non_empty([
            cli.jwt_qws,
            std::env::var("QCONNECT_JWT_QWS").ok(),
            std::env::var("QBZ_QCONNECT_JWT").ok(),
            std::env::var("QBZ_QCONNECT_JWT_QWS").ok(),
        ]),
        app_id: first_non_empty([
            cli.app_id,
            std::env::var("QBZ_QOBUZ_APP_ID").ok(),
            std::env::var("QOBUZ_APP_ID").ok(),
            qconnect::load_saved_app_id().ok().flatten(),
        ]),
        user_auth_token: first_non_empty([
            cli.user_auth_token,
            std::env::var("QBZ_QOBUZ_USER_AUTH_TOKEN").ok(),
            std::env::var("QOBUZ_USER_AUTH_TOKEN").ok(),
            qconnect::load_saved_user_auth_token().ok().flatten(),
        ]),
        device_name: cli.device_name,
        device_uuid: cli.device_uuid,
        subscribe_channels_hex: cli.subscribe_channels_hex,
        connect_timeout_ms: cli.connect_timeout_ms,
        reconnect_backoff_ms: cli.reconnect_backoff_ms,
        reconnect_backoff_max_ms: cli.reconnect_backoff_max_ms,
        keepalive_interval_ms: cli.keepalive_interval_ms,
        qcloud_proto: cli.qcloud_proto,
        json: cli.json,
    };

    let client = Arc::new(
        QconnectClient::connect(options, enable_renderer)
            .await
            .context("connect to Qobuz Connect")?,
    );

    match command
    {
        Command::Login { .. } => unreachable!("login is handled before Connect startup"),
        Command::Serve {
            report_interval_secs,
        } =>
        {
            client.spawn_state_reporter(Duration::from_secs(report_interval_secs.max(1)));
            tokio::signal::ctrl_c()
                .await
                .context("wait for Ctrl-C signal")?;
        }
        Command::Status { .. } =>
        {
            sleep(Duration::from_secs(wait_after_connect)).await;
        }
        Command::Queue { .. } =>
        {
            client.ask_queue_state().await?;
            sleep(Duration::from_secs(wait_after_connect)).await;
        }
        Command::Load {
            track_ids,
            start_index,
            shuffle,
            ..
        } =>
        {
            send_load(&client, track_ids, start_index, shuffle).await?;
            sleep(Duration::from_secs(wait_after_connect)).await;
        }
        Command::Add { track_ids, .. } =>
        {
            send_track_ids(&client, QueueCommandType::CtrlSrvrQueueAddTracks, track_ids).await?;
            sleep(Duration::from_secs(wait_after_connect)).await;
        }
        Command::Insert {
            track_ids,
            after_queue_item_id,
            ..
        } =>
        {
            send_insert(&client, track_ids, after_queue_item_id).await?;
            sleep(Duration::from_secs(wait_after_connect)).await;
        }
        Command::Clear { .. } =>
        {
            client
                .send_queue_command(QueueCommandType::CtrlSrvrClearQueue, json!({}))
                .await?;
            sleep(Duration::from_secs(wait_after_connect)).await;
        }
        Command::Play { .. } =>
        {
            send_player_state(&client, 2).await?;
            sleep(Duration::from_secs(wait_after_connect)).await;
        }
        Command::Pause { .. } =>
        {
            send_player_state(&client, 3).await?;
            sleep(Duration::from_secs(wait_after_connect)).await;
        }
        Command::Stop { .. } =>
        {
            send_player_state(&client, 1).await?;
            sleep(Duration::from_secs(wait_after_connect)).await;
        }
        Command::Volume {
            set, delta, mute, ..
        } =>
        {
            send_volume(&client, set, delta, mute).await?;
            sleep(Duration::from_secs(wait_after_connect)).await;
        }
        Command::Shuffle { mode, .. } =>
        {
            client
                .send_queue_command(
                    QueueCommandType::CtrlSrvrSetShuffleMode,
                    json!({
                        "shuffle_mode": matches!(mode, Toggle::On),
                        "shuffle_seed": random_u32(),
                        "autoplay_reset": false,
                        "autoplay_loading": false
                    }),
                )
                .await?;
            sleep(Duration::from_secs(wait_after_connect)).await;
        }
        Command::Repeat { mode, .. } =>
        {
            client
                .send_queue_command(
                    QueueCommandType::CtrlSrvrSetLoopMode,
                    json!({ "loop_mode": repeat_mode_value(mode) }),
                )
                .await?;
            sleep(Duration::from_secs(wait_after_connect)).await;
        }
    }

    client.disconnect().await;
    Ok(())
}

async fn send_load(
    client: &QconnectClient,
    track_ids: Vec<u64>,
    start_index: usize,
    shuffle: bool,
) -> Result<()>
{
    let ids = to_u32_track_ids(&track_ids)?;
    if ids.is_empty()
    {
        bail!("load requires at least one track id");
    }

    let normalized_start = start_index.min(ids.len().saturating_sub(1));
    client
        .send_queue_command(
            QueueCommandType::CtrlSrvrQueueLoadTracks,
            json!({
                "track_ids": ids,
                "queue_position": if shuffle { 0 } else { normalized_start },
                "shuffle_mode": shuffle,
                "shuffle_seed": if shuffle { Some(random_u32()) } else { None },
                "shuffle_pivot_index": normalized_start,
                "context_uuid": Uuid::new_v4().to_string(),
                "autoplay_reset": true,
                "autoplay_loading": false
            }),
        )
        .await?;
    Ok(())
}

async fn send_track_ids(
    client: &QconnectClient,
    command_type: QueueCommandType,
    track_ids: Vec<u64>,
) -> Result<()>
{
    let ids = to_u32_track_ids(&track_ids)?;
    if ids.is_empty()
    {
        bail!("command requires at least one track id");
    }
    client
        .send_queue_command(
            command_type,
            json!({
                "track_ids": ids,
                "context_uuid": Uuid::new_v4().to_string(),
                "autoplay_reset": false,
                "autoplay_loading": false
            }),
        )
        .await?;
    Ok(())
}

async fn send_insert(
    client: &QconnectClient,
    track_ids: Vec<u64>,
    after_queue_item_id: Option<u64>,
) -> Result<()>
{
    let ids = to_u32_track_ids(&track_ids)?;
    if ids.is_empty()
    {
        bail!("insert requires at least one track id");
    }
    client
        .send_queue_command(
            QueueCommandType::CtrlSrvrQueueInsertTracks,
            json!({
                "track_ids": ids,
                "insert_after": after_queue_item_id,
                "context_uuid": Uuid::new_v4().to_string(),
                "autoplay_reset": false,
                "autoplay_loading": false
            }),
        )
        .await?;
    Ok(())
}

async fn send_player_state(client: &QconnectClient, playing_state: i32) -> Result<()>
{
    let renderer = client.renderer_state().await;
    let queue = client.queue_state().await;
    let current_position = renderer
        .current_position_ms
        .and_then(|value| i32::try_from(value).ok());
    let current_queue_item = renderer.current_track.as_ref().map(|item| {
        json!({
            "queue_version": {
                "major": queue.version.major,
                "minor": queue.version.minor
            },
            "id": item.queue_item_id
        })
    });

    client
        .send_queue_command(
            QueueCommandType::CtrlSrvrSetPlayerState,
            json!({
                "playing_state": playing_state,
                "current_position": current_position,
                "current_queue_item": current_queue_item
            }),
        )
        .await?;
    Ok(())
}

async fn send_volume(
    client: &QconnectClient,
    set: Option<i32>,
    delta: Option<i32>,
    mute: Option<bool>,
) -> Result<()>
{
    if set.is_none() && delta.is_none() && mute.is_none()
    {
        bail!("volume requires --set, --delta, or --mute");
    }

    if let Some(value) = mute
    {
        client
            .send_queue_command(QueueCommandType::CtrlSrvrMuteVolume, json!({ "value": value }))
            .await?;
    }

    if set.is_some() || delta.is_some()
    {
        client
            .send_queue_command(
                QueueCommandType::CtrlSrvrSetVolume,
                json!({
                    "volume": set.map(|value| value.clamp(0, 100)),
                    "volume_delta": delta
                }),
            )
            .await?;
    }

    Ok(())
}

fn first_non_empty(values: impl IntoIterator<Item = Option<String>>) -> Option<String>
{
    values
        .into_iter()
        .flatten()
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

fn wait_secs_for(command: &Command) -> u64
{
    match command
    {
        Command::Login { .. } => 0,
        Command::Serve { .. } => 0,
        Command::Status { wait_secs }
        | Command::Queue { wait_secs }
        | Command::Load { wait_secs, .. }
        | Command::Add { wait_secs, .. }
        | Command::Insert { wait_secs, .. }
        | Command::Clear { wait_secs }
        | Command::Play { wait_secs }
        | Command::Pause { wait_secs }
        | Command::Stop { wait_secs }
        | Command::Volume { wait_secs, .. }
        | Command::Shuffle { wait_secs, .. }
        | Command::Repeat { wait_secs, .. } => *wait_secs,
    }
}

fn to_u32_track_ids(track_ids: &[u64]) -> Result<Vec<u32>>
{
    track_ids
        .iter()
        .map(|id| u32::try_from(*id).with_context(|| format!("track id {id} exceeds u32 range")))
        .collect()
}

fn repeat_mode_value(mode: RepeatMode) -> i32
{
    match mode
    {
        RepeatMode::Off => 1,
        RepeatMode::One => 2,
        RepeatMode::All => 3,
    }
}

fn random_u32() -> u32
{
    let bytes = *Uuid::new_v4().as_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}
