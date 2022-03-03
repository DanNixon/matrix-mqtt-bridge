FROM docker.io/library/rust:alpine3.15 as builder

RUN apk add \
  cmake \
  g++ \
  libc-dev \
  make \
  openssl-dev \
  pkgconf

COPY . .
RUN RUSTFLAGS=-Ctarget-feature=-crt-static cargo install \
  --path . \
  --root /usr/local

FROM docker.io/library/alpine:3.15

RUN apk add \
  libgcc

COPY --from=builder \
  /usr/local/bin/matrix-mqtt-bridge \
  /usr/local/bin/matrix-mqtt-bridge

ENTRYPOINT ["/usr/local/bin/matrix-mqtt-bridge"]
