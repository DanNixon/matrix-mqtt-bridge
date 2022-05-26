use super::{Cli, Event, Message};
use anyhow::Result;
use matrix_sdk::{
    room::{Joined, Room},
    ruma::{
        events::{room::message::MessageEventContent, AnyMessageEventContent, SyncMessageEvent},
        UserId,
    },
    Client, SyncSettings,
};
use tokio::{sync::broadcast::Sender, task::JoinHandle};

pub(crate) async fn login(tx: Sender<Event>, args: Cli) -> Result<Client> {
    log::info!("Logging into Matrix homeserver...");
    let user = UserId::try_from(args.matrix_username.clone())?;
    let client = Client::new_from_user_id(user.clone()).await?;
    client
        .login(
            user.localpart(),
            &args.matrix_password,
            None,
            Some("matrix-mqtt-bridge"),
        )
        .await?;

    log::info!("Performing initial sync...");
    client.sync_once(SyncSettings::default()).await?;

    log::info!("Successfully logged in to Matrix homeserver");

    client
        .register_event_handler({
            let tx = tx.clone();
            let watched_rooms = args.matrix_rooms.clone();
            move |event: SyncMessageEvent<MessageEventContent>, room: Room| {
                let tx = tx.clone();
                let watched_rooms = watched_rooms.clone();
                async move {
                    if let Room::Joined(room) = room {
                        if watched_rooms.contains(room.room_id()) {
                            log::info!("Received message in room {}", room.room_id());
                            parse_and_queue_message(&tx, event, room);
                        }
                    }
                }
            }
        })
        .await;

    Ok(client)
}

fn parse_and_queue_message(
    tx: &Sender<Event>,
    event: SyncMessageEvent<MessageEventContent>,
    room: Joined,
) {
    if let Ok(body) = serde_json::to_string(&event) {
        if let Err(e) = tx.send(Event::MessageFromMatrix(Message {
            room: room.room_id().clone(),
            body,
        })) {
            log::error!("Failed to notify of new message from MQTT: {}", e);
        }
    }
}

pub(crate) fn run_send_task(tx: Sender<Event>, client: Client) -> Result<JoinHandle<()>> {
    let mut rx = tx.subscribe();

    Ok(tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            match event {
                Event::Exit => {
                    log::debug!("Task exit");
                    return;
                }
                Event::MessageFromMqtt(msg) => {
                    log::debug!("Sending message...");
                    if let Err(e) = client
                        .get_joined_room(&msg.room)
                        .unwrap()
                        .send(
                            AnyMessageEventContent::RoomMessage(MessageEventContent::text_plain(
                                msg.body,
                            )),
                            None,
                        )
                        .await
                    {
                        log::error!("Failed to send message: {}", e);
                    }
                }
                _ => {}
            }
        }
    }))
}
