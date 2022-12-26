use super::{Event, Message, ReadinessConditions};
use crate::cli::Cli;
use anyhow::Result;
use kagiyama::ReadinessProbe;
use matrix_sdk::{
    config::SyncSettings,
    event_handler::Ctx,
    room::Room,
    ruma::{
        events::room::message::{RoomMessageEventContent, SyncRoomMessageEvent},
        OwnedRoomId, UserId,
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

#[derive(Clone)]
struct CallbackContext {
    watched_rooms: Vec<OwnedRoomId>,
    tx: Sender<Event>,
}

pub(crate) async fn login(
    tx: Sender<Event>,
    mut readiness_conditions: ReadinessProbe<ReadinessConditions>,
    args: Cli,
) -> Result<Client> {
    let user = UserId::parse(args.matrix_username.clone())?;

    let client = Client::builder()
        .server_name(user.server_name())
        .sled_store(args.matrix_sled_path(), None)?
        .build()
        .await?;

    if args.matrix_session_filename().exists() {
        log::info!("Restored login");

        // Load the session from file
        let session_file = std::fs::File::open(args.matrix_session_filename())?;
        let session = serde_json::from_reader(session_file)?;

        // Login
        client.restore_login(session).await?;
    } else {
        log::info!("Initial login");

        // Login
        client
            .login_username(user.localpart(), &args.matrix_password)
            .initial_device_display_name("MQTT bridge")
            .send()
            .await?;

        // Save the session to file
        let session = client.session().expect("Session should exist after login");
        let session_file = std::fs::File::create(args.matrix_session_filename())?;
        serde_json::to_writer(session_file, &session)?;
    }

    log::info!("Performing initial sync");
    client.sync_once(SyncSettings::default()).await?;

    log::info!("Successfully logged in to Matrix homeserver");
    readiness_conditions.mark_ready(ReadinessConditions::MatrixLoginAndInitialSyncComplete);

    let context = CallbackContext {
        watched_rooms: args.matrix_rooms.clone(),
        tx: tx.clone(),
    };
    client.add_event_handler_context(context);

    client.add_event_handler(handle_message);

    Ok(client)
}

async fn handle_message(event: SyncRoomMessageEvent, room: Room, context: Ctx<CallbackContext>) {
    if let Room::Joined(room) = room {
        if context
            .watched_rooms
            .iter()
            .any(|r| r.deref() == room.room_id())
        {
            log::info!("Received message in room {}", room.room_id());
            metrics::MESSAGE_EVENT
                .get_or_create(&metrics::MessageEventLables::new(
                    room.room_id().to_string(),
                ))
                .inc();

            if let Ok(body) = serde_json::to_string(&event) {
                if let Err(e) = context.tx.send(Event::MessageFromMatrix(Message {
                    room: room.room_id().to_owned(),
                    body,
                })) {
                    log::error!("Failed to notify of new message from MQTT: {}", e);
                }
            }
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
                    log::debug!("Sending message");
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
