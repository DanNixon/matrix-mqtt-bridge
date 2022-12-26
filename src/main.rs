mod cli;
mod matrix;

use anyhow::Result;
use clap::Parser;
use kagiyama::Watcher;
use matrix_sdk::{
    config::SyncSettings,
    ruma::{OwnedRoomId, RoomId},
};
use mqtt_channel_client as mqtt;
use serde::Serialize;
use std::time::Duration;
use strum_macros::EnumIter;
use tokio::sync::broadcast;

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

    let args = cli::Cli::parse();
    args.create_matrix_storage_dir();

    let (tx, mut rx) = broadcast::channel::<Event>(16);

    let mqtt_client = mqtt::Client::new(
        paho_mqtt::create_options::CreateOptionsBuilder::new()
            .server_uri(&args.mqtt_broker)
            .client_id(&args.mqtt_client_id)
            .persistence(paho_mqtt::PersistenceType::None)
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
                matrix::metrics::MESSAGE_EVENT.clone(),
            );
            registry.register(
                "delivery_failures",
                "Matrix message receive count",
                matrix::metrics::DELIVERY_FAILURES.clone(),
            );
        }
    }
    let mut readiness_conditions = watcher.readiness_probe();
    watcher
        .start_server(args.observability_address.clone().parse()?)
        .await;

    mqtt_client
        .start(
            paho_mqtt::connect_options::ConnectOptionsBuilder::new()
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

    let matrix_client = matrix::login(tx.clone(), watcher.readiness_probe(), args.clone()).await?;
    let matrix_task = matrix::run_send_task(tx.clone(), matrix_client.clone())?;
    let matrix_sync_task = tokio::spawn(async move {
        matrix_client
            .sync(
                SyncSettings::default().token(
                    matrix_client
                        .sync_token()
                        .await
                        .expect("sync token should be available"),
                ),
            )
            .await
            .unwrap();
    });

    let mut mqtt_rx = mqtt_client.rx_channel();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tx.send(Event::Exit).unwrap();
            },
            event = rx.recv() => {
                match event {
                    Ok(Event::Exit) => {
                        log::info!("Exiting");
                        break;
                    }
                    Ok(Event::MessageFromMatrix(msg)) => {
                        let msg = paho_mqtt::Message::new(
                            format!("{}/{}", &args.mqtt_topic_prefix, msg.room),
                            msg.body.clone(),
                            args.mqtt_qos,
                        );
                        if let Err(e) = mqtt_client.send(msg) {
                            log::warn!("{}", e);
                        }
                    }
                    _ => {}
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
                        log::info!("Received message on topic \"{}\"", msg.topic());
                        match msg.topic().split('/').nth(1) {
                            Some(room) => match RoomId::parse(room) {
                                Ok(room) => {
                                    if let Err(e) = tx.send(Event::MessageFromMqtt(Message {
                                        room,
                                        body: msg.payload_str().to_string(),
                                    })) {
                                        log::error!("Failed to notify of new message from MQTT: {}", e);
                                    }
                                }
                                Err(e) => log::error!("Failed to get room ID: {}", e),
                            },
                            None => log::error!("Failed to get room ID"),
                        }
                    }
                    _ => {}
                }
            }
        };
    }

    let _ = matrix_task.await;
    matrix_sync_task.abort();
    let _ = matrix_sync_task.await;

    let _ = mqtt_client.stop().await;

    watcher.stop_server()?;

    Ok(())
}
