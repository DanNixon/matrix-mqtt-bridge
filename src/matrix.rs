use super::{Cli, Event, Message, ReadinessConditions};
use anyhow::Result;
use kagiyama::ReadinessProbe;
use matrix_sdk::{
    config::SyncSettings,
    room::{Joined, Room},
    ruma::{
        events::room::message::{RoomMessageEventContent, SyncRoomMessageEvent},
        UserId,
    },
    Client,
};
use std::ops::Deref;
use tokio::{sync::broadcast::Sender, task::JoinHandle};

pub(crate) mod metrics {
    use lazy_static::lazy_static;
    use prometheus_client::encoding::EncodeLabelSet;
    use prometheus_client::metrics::{counter::Counter, family::Family};

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
}

pub(crate) async fn login(
    tx: Sender<Event>,
    mut readiness_conditions: ReadinessProbe<ReadinessConditions>,
    args: Cli,
) -> Result<Client> {
    log::info!("Logging into Matrix homeserver...");
    let user = UserId::parse(args.matrix_username.clone())?;
    let client = Client::builder()
        .homeserver_url(format!("https://{}", user.server_name()))
        .build()
        .await?;
    client
        .login_username(user.localpart(), &args.matrix_password)
        .initial_device_display_name("MQTT bridge")
        .send()
        .await?;

    log::info!("Performing initial sync...");
    client.sync_once(SyncSettings::default()).await?;

    log::info!("Successfully logged in to Matrix homeserver");
    readiness_conditions.mark_ready(ReadinessConditions::MatrixLoginAndInitialSyncComplete);

    client.add_event_handler({
        let tx = tx.clone();
        let watched_rooms = args.matrix_rooms.clone();
        move |event: SyncRoomMessageEvent, room: Room| {
            let tx = tx.clone();
            let watched_rooms = watched_rooms.clone();
            async move {
                if let Room::Joined(room) = room {
                    if watched_rooms
                        .into_iter()
                        .any(|r| r.deref() == room.room_id())
                    {
                        log::info!("Received message in room {}", room.room_id());
                        metrics::MESSAGE_EVENT
                            .get_or_create(&metrics::MessageEventLables::new(
                                room.room_id().to_string(),
                            ))
                            .inc();
                        parse_and_queue_message(&tx, event, room);
                    }
                }
            }
        }
    });

    Ok(client)
}

fn parse_and_queue_message(tx: &Sender<Event>, event: SyncRoomMessageEvent, room: Joined) {
    if let Ok(body) = serde_json::to_string(&event) {
        if let Err(e) = tx.send(Event::MessageFromMatrix(Message {
            room: room.room_id().to_owned(),
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
                        .send(RoomMessageEventContent::text_plain(msg.body), None)
                        .await
                    {
                        log::error!("Failed to send message: {}", e);
                        metrics::DELIVERY_FAILURES.inc();
                    }
                }
                _ => {}
            }
        }
    }))
}
