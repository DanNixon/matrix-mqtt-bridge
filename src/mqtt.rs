use super::{Cli, Event, ReadinessConditions};
use anyhow::Result;
use kagiyama::ReadinessProbe;
use matrix_sdk::ruma::RoomId;
use paho_mqtt::{
    AsyncClient, ConnectOptionsBuilder, CreateOptionsBuilder, Message, PersistenceType,
};
use tokio::{
    sync::broadcast::Sender,
    task::JoinHandle,
    time::{self, Duration},
};

pub(crate) mod metrics {
    use lazy_static::lazy_static;
    use prometheus_client::encoding::text::Encode;
    use prometheus_client::metrics::{counter::Counter, family::Family};

    #[derive(Clone, Eq, Hash, PartialEq, Encode)]
    pub(crate) struct ConnectionEventLables {
        event: ConnectionEventType,
    }

    impl ConnectionEventLables {
        pub(crate) fn new(event: ConnectionEventType) -> Self {
            Self { event }
        }
    }

    #[derive(Clone, Eq, Hash, PartialEq, Encode)]
    pub(crate) enum ConnectionEventType {
        Connected,
        Disconnected,
        Lost,
    }

    #[derive(Clone, Eq, Hash, PartialEq, Encode)]
    pub(crate) struct MessageEventLables {
        kind: String,
        room_id: String,
    }

    impl MessageEventLables {
        pub(crate) fn new(kind: String, room_id: String) -> Self {
            Self { kind, room_id }
        }
    }

    lazy_static! {
        pub(crate) static ref CONNECTION_EVENT: Family::<ConnectionEventLables, Counter> =
            Family::<ConnectionEventLables, Counter>::default();
        pub(crate) static ref MESSAGE_EVENT: Family::<MessageEventLables, Counter> =
            Family::<MessageEventLables, Counter>::default();
        pub(crate) static ref DELIVERY_FAILURES: Counter = Counter::default();
    }
}

pub(crate) async fn run(
    tx: Sender<Event>,
    readiness_conditions: ReadinessProbe<ReadinessConditions>,
    args: Cli,
) -> Result<JoinHandle<()>> {
    let mut client = AsyncClient::new(
        CreateOptionsBuilder::new()
            .server_uri(&args.mqtt_broker)
            .client_id(&args.mqtt_client_id)
            .persistence(PersistenceType::None)
            .finalize(),
    )?;

    let stream = client.get_stream(25);

    let topics_to_subscribe: Vec<String> = args
        .matrix_rooms
        .iter()
        .map(|r| format!("{}/{}/send/text", args.mqtt_topic_prefix, r))
        .collect();

    {
        let mut readiness_conditions = readiness_conditions.clone();

        client.set_disconnected_callback(move |_c, _p, _r| {
            log::info!("Disconnected");

            metrics::CONNECTION_EVENT
                .get_or_create(&metrics::ConnectionEventLables::new(
                    metrics::ConnectionEventType::Disconnected,
                ))
                .inc();

            readiness_conditions
                .mark_not_ready(super::ReadinessConditions::MqttBrokerConnectionIsAlive);
        });
    }

    {
        let mut readiness_conditions = readiness_conditions.clone();

        client.set_connection_lost_callback(move |_c| {
            log::warn!("Connection lost");

            metrics::CONNECTION_EVENT
                .get_or_create(&metrics::ConnectionEventLables::new(
                    metrics::ConnectionEventType::Lost,
                ))
                .inc();

            readiness_conditions
                .mark_not_ready(super::ReadinessConditions::MqttBrokerConnectionIsAlive);
        });
    }

    {
        let mut readiness_conditions = readiness_conditions;

        client.set_connected_callback(move |c| {
            log::info!("Connected");

            for topic in &topics_to_subscribe {
                c.subscribe(topic, args.mqtt_qos);
            }

            metrics::CONNECTION_EVENT
                .get_or_create(&metrics::ConnectionEventLables::new(
                    metrics::ConnectionEventType::Connected,
                ))
                .inc();

            readiness_conditions
                .mark_ready(super::ReadinessConditions::MqttBrokerConnectionIsAlive);
        });
    }

    client
        .connect(
            ConnectOptionsBuilder::new()
                .clean_session(true)
                .automatic_reconnect(Duration::from_secs(1), Duration::from_secs(5))
                .keep_alive_interval(Duration::from_secs(5))
                .user_name(&args.mqtt_username)
                .password(&args.mqtt_password)
                .finalize(),
        )
        .wait()?;

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
                                    metrics::DELIVERY_FAILURES.inc();
                                }
                            }
                            Err(e) => {
                                log::error!("Error creating/queuing the message: {}", e);
                            }
                        }
                    }
                    _ => {}
                }
            }

            if let Ok(Some(msg)) = stream.try_recv() {
                log::info!("Received message on topic \"{}\"", msg.topic());
                match msg.topic().split('/').nth(1) {
                    Some(room) => match RoomId::parse(room) {
                        Ok(room) => {
                            metrics::MESSAGE_EVENT
                                .get_or_create(&metrics::MessageEventLables::new(
                                    "text".into(),
                                    room.to_string(),
                                ))
                                .inc();
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
