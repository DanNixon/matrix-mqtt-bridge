mod matrix;
mod mqtt;

use anyhow::Result;
use clap::Parser;
use matrix_sdk::{config::SyncSettings, ruma::OwnedRoomId};
use tokio::{signal, sync::broadcast};

/// A bridge between Matrix and MQTT
#[derive(Clone, Debug, Parser)]
#[clap(author, version, about)]
struct Cli {
    /// Address of MQTT broker to connect to
    #[clap(
        value_parser,
        long,
        env = "MQTT_BROKER",
        default_value = "tcp://localhost:1883"
    )]
    mqtt_broker: String,

    /// Client ID to use when connecting to MQTT broker
    #[clap(
        value_parser,
        long,
        env = "MQTT_CLIENT_ID",
        default_value = "matrix-mqtt-bridge"
    )]
    mqtt_client_id: String,

    /// MQTT QoS, must be 0, 1 or 2
    #[clap(value_parser, long, env = "MQTT_QOS", default_value = "0")]
    mqtt_qos: i32,

    /// MQTT username
    #[clap(value_parser, long, env = "MQTT_USERNAME", default_value = "")]
    mqtt_username: String,

    /// MQTT password
    #[clap(value_parser, long, env = "MQTT_PASSWORD", default_value = "")]
    mqtt_password: String,

    /// Prefix for MQTT topics (<PREFIX>/<ROOM ID>...)
    #[clap(
        value_parser,
        long,
        env = "MQTT_TOPIC_PREFIX",
        default_value = "matrix_bridge"
    )]
    mqtt_topic_prefix: String,

    /// Matrix username
    #[clap(value_parser, long, env = "MATRIX_USERNAME")]
    matrix_username: String,

    /// Matrix password
    #[clap(value_parser, long, env = "MATRIX_PASSWORD")]
    matrix_password: String,

    /// IDs of Matrix rooms to interact with
    #[clap(value_parser)]
    matrix_rooms: Vec<OwnedRoomId>,
}

#[derive(Clone, Debug)]
enum Event {
    MessageFromMqtt(Message),
    MessageFromMatrix(Message),
    Exit,
}

#[derive(Clone, Debug)]
pub(crate) struct Message {
    pub room: OwnedRoomId,
    pub body: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Cli::parse();

    let (tx, mut rx) = broadcast::channel::<Event>(16);

    let matrix_client = matrix::login(tx.clone(), args.clone()).await?;

    let tasks = vec![
        mqtt::run(tx.clone(), args).await?,
        matrix::run_send_task(tx.clone(), matrix_client.clone())?,
    ];

    let matrix_sync_task = tokio::spawn(async move {
        matrix_client
            .sync(SyncSettings::default().token(matrix_client.sync_token().await.unwrap()))
            .await;
    });

    loop {
        let should_exit = tokio::select! {
            _ = signal::ctrl_c() => true,
            event = rx.recv() => matches!(event, Ok(Event::Exit)),
        };
        if should_exit {
            break;
        }
    }

    log::info!("Terminating...");
    tx.send(Event::Exit)?;
    for task in tasks {
        if let Err(e) = task.await {
            log::error!("Failed waiting for task to finish: {}", e);
        }
    }
    matrix_sync_task.abort();
    let _ = matrix_sync_task.await;

    Ok(())
}
