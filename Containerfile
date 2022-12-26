FROM docker.io/library/rust:bullseye as builder

RUN apt-get update && \
    apt-get install -y \
      cmake

COPY . .
RUN cargo install \
  --path . \
  --root /usr/local

FROM docker.io/library/debian:bullseye-slim

COPY --from=builder \
  /usr/local/bin/matrix-mqtt-bridge \
  /usr/local/bin/matrix-mqtt-bridge

RUN mkdir /data
VOLUME /data
ENV MATRIX_STORAGE=/data

ENV OBSERVABILITY_ADDRESS "0.0.0.0:9090"
EXPOSE 9090

ENTRYPOINT ["/usr/local/bin/matrix-mqtt-bridge"]
