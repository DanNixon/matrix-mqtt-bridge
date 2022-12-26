use clap::Parser;
use matrix_sdk::ruma::OwnedRoomId;
use std::path::PathBuf;

/// A bridge between Matrix and MQTT
#[derive(Clone, Debug, Parser)]
#[clap(author, version, about)]
pub(crate) struct Cli {
    /// Address of MQTT broker to connect to
    #[clap(
        value_parser,
        long,
        env = "MQTT_BROKER",
        default_value = "tcp://localhost:1883"
    )]
    pub(crate) mqtt_broker: String,

    /// Client ID to use when connecting to MQTT broker
    #[clap(
        value_parser,
        long,
        env = "MQTT_CLIENT_ID",
        default_value = "matrix-mqtt-bridge"
    )]
    pub(crate) mqtt_client_id: String,

    /// MQTT QoS, must be 0, 1 or 2
    #[clap(value_parser, long, env = "MQTT_QOS", default_value = "0")]
    pub(crate) mqtt_qos: i32,

    /// MQTT username
    #[clap(value_parser, long, env = "MQTT_USERNAME", default_value = "")]
    pub(crate) mqtt_username: String,

    /// MQTT password
    #[clap(value_parser, long, env = "MQTT_PASSWORD", default_value = "")]
    pub(crate) mqtt_password: String,

    /// Prefix for MQTT topics (<PREFIX>/<ROOM ID>...)
    #[clap(
        value_parser,
        long,
        env = "MQTT_TOPIC_PREFIX",
        default_value = "matrix_bridge"
    )]
    pub(crate) mqtt_topic_prefix: String,

    /// Matrix username
    #[clap(value_parser, long, env = "MATRIX_USERNAME")]
    pub(crate) matrix_username: String,

    /// Matrix password
    #[clap(value_parser, long, env = "MATRIX_PASSWORD")]
    pub(crate) matrix_password: String,

    /// Matrix storage directory
    #[clap(value_parser, long, env = "MATRIX_STORAGE")]
    pub(crate) matrix_storage: PathBuf,

    /// IDs of Matrix rooms to interact with
    #[clap(value_parser)]
    pub(crate) matrix_rooms: Vec<OwnedRoomId>,

    /// Address to listen on for observability/metrics endpoints
    #[clap(
        value_parser,
        long,
        env = "OBSERVABILITY_ADDRESS",
        default_value = "127.0.0.1:9090"
    )]
    pub(crate) observability_address: String,
}

impl Cli {
    pub(crate) fn create_matrix_storage_dir(&self) {
        if let Err(e) = std::fs::create_dir_all(&self.matrix_storage) {
            log::warn!("Failed to create Matrix storage directory, error={}", e);
        }
    }

    pub(crate) fn matrix_sled_path(&self) -> PathBuf {
        self.matrix_storage.join("sled")
    }

    pub(crate) fn matrix_session_filename(&self) -> PathBuf {
        self.matrix_storage.join("session.json")
    }
}
