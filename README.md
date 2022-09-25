# matrix-mqtt-bridge

[![CI](https://github.com/DanNixon/matrix-mqtt-bridge/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/DanNixon/matrix-mqtt-bridge/actions/workflows/ci.yml)
[![dependency status](https://deps.rs/repo/github/dannixon/matrix-mqtt-bridge/status.svg)](https://deps.rs/repo/github/dannixon/matrix-mqtt-bridge)

Bridge between Matrix and MQTT.

## Usage

See `matrix-mqtt-bridge --help` for configuration.

Bot user should already be joined to any rooms you wish to interact with.

## MQTT API

Events from the Matrix room are published to `<prefix>/<room id>`.

Text published to `<prefix>/<room id>/send/text` are sent as `m.text` messages.
