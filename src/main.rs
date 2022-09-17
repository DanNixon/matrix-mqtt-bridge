mod matrix;
mod mqtt;

use anyhow::Result;
use clap::Parser;
use kagiyama::Watcher;
use matrix_sdk::{config::SyncSettings, ruma::OwnedRoomId};
use serde::Serialize;
use strum_macros::EnumIter;
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

    /// Address to listen on for observability/metrics endpoints
    #[clap(
        value_parser,
        long,
        env = "OBSERVABILITY_ADDRESS",
        default_value = "127.0.0.1:9090"
    )]
    observability_address: String,
}

#[derive(Clone, Serialize, EnumIter, PartialEq, Hash, Eq)]
enum ReadinessConditions {
    MatrixLoginAndInitialSyncComplete,
    MqttBrokerConnectionIsAlive,
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

    let mut watcher = Watcher::<ReadinessConditions>::default();

    {
        let mut registry = watcher.metrics_registry();
        let registry = registry.sub_registry_with_prefix("matrixmqttbridge");

        {
            let registry = registry.sub_registry_with_prefix("mqtt");

            registry.register(
                "connection_events",
                "MQTT broker connection event count",
                Box::new(mqtt::metrics::CONNECTION_EVENT.clone()),
            );
            registry.register(
                "messages",
                "MQTT message receive count",
                Box::new(mqtt::metrics::MESSAGE_EVENT.clone()),
            );
            registry.register(
                "delivery_failures",
                "MQTT message receive count",
                Box::new(mqtt::metrics::DELIVERY_FAILURES.clone()),
            );
        }

        {
            let registry = registry.sub_registry_with_prefix("matrix");

            registry.register(
                "messages",
                "Matrix message receive count",
                Box::new(matrix::metrics::MESSAGE_EVENT.clone()),
            );
            registry.register(
                "delivery_failures",
                "Matrix message receive count",
                Box::new(matrix::metrics::DELIVERY_FAILURES.clone()),
            );
        }
    }

    watcher
        .start_server(args.observability_address.clone().parse()?)
        .await?;

    let matrix_client = matrix::login(tx.clone(), watcher.readiness_probe(), args.clone()).await?;

    let tasks = vec![
        mqtt::run(tx.clone(), watcher.readiness_probe(), args).await?,
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

    watcher.stop_server()?;

    Ok(())
}
