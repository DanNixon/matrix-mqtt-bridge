use super::{Cli, Event};
use anyhow::Result;
use matrix_sdk::ruma::RoomId;
use paho_mqtt::{AsyncClient, ConnectOptionsBuilder, CreateOptionsBuilder, Message};
use std::{env, str::FromStr};
use tokio::{
    sync::broadcast::Sender,
    task::JoinHandle,
    time::{self, Duration},
};

pub(crate) async fn run(tx: Sender<Event>, args: Cli) -> Result<JoinHandle<()>> {
    let mut client = AsyncClient::new(
        CreateOptionsBuilder::new()
            .server_uri(&args.mqtt_broker)
            .client_id(&args.mqtt_client_id)
            .persistence(env::temp_dir())
            .finalize(),
    )?;

    let stream = client.get_stream(25);

    client
        .connect(
            ConnectOptionsBuilder::new()
                .user_name(&args.mqtt_username)
                .password(&args.mqtt_password)
                .finalize(),
        )
        .wait()?;

    for topic in generate_subscriptions(&args) {
        client.subscribe(&topic, args.mqtt_qos).await?;
    }

    let mut rx = tx.subscribe();

    Ok(tokio::spawn(async move {
        let mut beat = time::interval(Duration::from_millis(100));

        loop {
            if let Ok(event) = rx.try_recv() {
                match event {
                    Event::Exit => {
                        log::debug!("Task exit");
                        return;
                    }
                    Event::MessageFromMatrix(msg) => {
                        match client.try_publish(Message::new(
                            format!("{}/{}", &args.mqtt_topic_prefix, msg.room),
                            msg.body.clone(),
                            args.mqtt_qos,
                        )) {
                            Ok(delivery_token) => {
                                if let Err(e) = delivery_token.wait() {
                                    log::error!("Error sending message: {}", e);
                                }
                            }
                            Err(e) => log::error!("Error creating/queuing the message: {}", e),
                        }
                    }
                    _ => {}
                }
            }

            if let Ok(Some(msg)) = stream.try_recv() {
                log::info! {"Received message on topic \"{}\"", msg.topic()};
                match msg.topic().split('/').nth(1) {
                    Some(room) => match RoomId::from_str(room) {
                        Ok(room) => {
                            if let Err(e) = tx.send(Event::MessageFromMqtt(super::Message {
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

            beat.tick().await;
        }
    }))
}

fn generate_subscriptions(args: &Cli) -> Vec<String> {
    args.matrix_rooms
        .iter()
        .map(|r| format!("{}/{}/send/text", args.mqtt_topic_prefix, r))
        .collect()
}
