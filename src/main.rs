mod cli;

use anyhow::Result;
use clap::Parser;
use kagiyama::prometheus::{
    self as prometheus_client,
    encoding::EncodeLabelSet,
    metrics::{counter::Counter, family::Family},
};
use kagiyama::Watcher;
use lazy_static::lazy_static;
use matrix_sdk::{
    event_handler::Ctx,
    room::Room,
    ruma::{
        events::room::message::{RoomMessageEventContent, SyncRoomMessageEvent},
        RoomId,
    },
};
use mqtt_channel_client as mqtt;
use serde::Serialize;
use std::{ops::Deref, time::Duration};
use strum_macros::EnumIter;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Eq, Hash, PartialEq, EncodeLabelSet)]
pub(crate) struct MessageEventLables {
    room_id: String,
}

impl MessageEventLables {
    pub(crate) fn new(room_id: String) -> Self {
        Self { room_id }
    }
}

lazy_static! {
    pub(crate) static ref MESSAGE_EVENT: Family::<MessageEventLables, Counter> =
        Family::<MessageEventLables, Counter>::default();
    pub(crate) static ref DELIVERY_FAILURES: Counter = Counter::default();
}

#[derive(Clone, Serialize, EnumIter, PartialEq, Hash, Eq)]
enum ReadinessConditions {
    MatrixLoginAndInitialSyncComplete,
    MqttBrokerConnectionIsAlive,
}

#[derive(Clone, Debug)]
enum Event {
    Exit,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = cli::Cli::parse();

    let (tx, mut rx) = broadcast::channel::<Event>(16);

    let mqtt_client = mqtt::Client::new(
        mqtt::paho_mqtt::create_options::CreateOptionsBuilder::new()
            .server_uri(&args.mqtt_broker)
            .client_id(&args.mqtt_client_id)
            .persistence(mqtt::paho_mqtt::PersistenceType::None)
            .finalize(),
        mqtt::ClientConfig::default(),
    )?;

    let mut watcher = Watcher::<ReadinessConditions>::default();
    {
        let mut registry = watcher.metrics_registry();
        let registry = registry.sub_registry_with_prefix("matrixmqttbridge");
        mqtt_client.register_metrics(registry);

        {
            let registry = registry.sub_registry_with_prefix("matrix");

            registry.register(
                "messages",
                "Matrix message receive count",
                MESSAGE_EVENT.clone(),
            );
            registry.register(
                "delivery_failures",
                "Matrix message receive count",
                DELIVERY_FAILURES.clone(),
            );
        }
    }
    let mut readiness_conditions = watcher.readiness_probe();
    watcher
        .start_server(args.observability_address.clone().parse()?)
        .await;

    mqtt_client
        .start(
            mqtt::paho_mqtt::connect_options::ConnectOptionsBuilder::new()
                .clean_session(true)
                .automatic_reconnect(Duration::from_secs(1), Duration::from_secs(5))
                .keep_alive_interval(Duration::from_secs(5))
                .user_name(&args.mqtt_username)
                .password(&args.mqtt_password)
                .finalize(),
        )
        .await?;

    for topic in args
        .matrix_rooms
        .iter()
        .map(|room| format!("{}/{}/send/text", args.mqtt_topic_prefix, room))
    {
        mqtt_client.subscribe(
            mqtt::SubscriptionBuilder::default()
                .topic(topic)
                .qos(args.mqtt_qos)
                .build()
                .unwrap(),
        );
    }

    let matrix_client = matrix_client_boilerplate::Client::new(
        &args.matrix_username,
        &args.matrix_password,
        "MQTT bridge",
        &args.matrix_storage,
    )
    .await?;
    matrix_client.initial_sync().await?;

    let context = CallbackContext {
        args: args.clone(),
        mqtt_client: mqtt_client.clone(),
    };
    matrix_client.client().add_event_handler_context(context);

    matrix_client
        .client()
        .add_event_handler(handle_matrix_message);

    matrix_client.start_background_sync().await;

    let mut mqtt_rx = mqtt_client.rx_channel();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tx.send(Event::Exit).unwrap();
            },
            event = rx.recv() => {
                if let Ok(Event::Exit) = event {
                    log::info!("Exiting");
                    break;
                }
            },
            event = mqtt_rx.recv() => {
                match event {
                    Ok(mqtt::Event::Status(mqtt::StatusEvent::Connected)) => {
                        readiness_conditions.mark_ready(ReadinessConditions::MqttBrokerConnectionIsAlive);
                    }
                    Ok(mqtt::Event::Status(mqtt::StatusEvent::Disconnected)) => {
                        readiness_conditions.mark_not_ready(ReadinessConditions::MqttBrokerConnectionIsAlive);
                    }
                    Ok(mqtt::Event::Rx(msg)) => {
                        handle_mqtt_message(matrix_client.client(), &msg).await;
                    }
                    _ => {}
                }
            }
        };
    }

    let _ = mqtt_client.stop().await;

    watcher.stop_server()?;

    Ok(())
}

#[derive(Clone)]
struct CallbackContext {
    args: cli::Cli,
    mqtt_client: mqtt::Client,
}

async fn handle_matrix_message(
    event: SyncRoomMessageEvent,
    room: Room,
    context: Ctx<CallbackContext>,
) {
    if let Room::Joined(room) = room {
        if context
            .args
            .matrix_rooms
            .iter()
            .any(|r| r.deref() == room.room_id())
        {
            log::info!("Received message in room {}", room.room_id());
            MESSAGE_EVENT
                .get_or_create(&MessageEventLables::new(room.room_id().to_string()))
                .inc();

            if let Ok(body) = serde_json::to_string(&event) {
                let msg = mqtt::paho_mqtt::Message::new(
                    format!(
                        "{}/{}",
                        &context.args.mqtt_topic_prefix,
                        room.room_id().to_owned()
                    ),
                    body,
                    context.args.mqtt_qos,
                );
                if let Err(e) = context.mqtt_client.send(msg) {
                    log::warn!("{}", e);
                }
            }

            if let Err(e) = room.read_receipt(event.event_id()).await {
                log::warn!("Failed to send read receipt, error: {}", e);
            }
        }
    }
}

async fn handle_mqtt_message(matrix_client: &matrix_sdk::Client, msg: &mqtt::paho_mqtt::Message) {
    log::info!("Received message on topic \"{}\"", msg.topic());
    match msg.topic().split('/').nth(1) {
        Some(room) => match RoomId::parse(room) {
            Ok(room) => {
                log::debug!("Sending message");
                if let Err(e) = matrix_client
                    .get_joined_room(&room)
                    .unwrap()
                    .send(
                        RoomMessageEventContent::text_plain(msg.payload_str().to_string()),
                        None,
                    )
                    .await
                {
                    log::error!("Failed to send message: {}", e);
                    DELIVERY_FAILURES.inc();
                }
            }
            Err(e) => log::error!("Failed to get room ID: {}", e),
        },
        None => log::error!("Failed to get room ID"),
    }
}
