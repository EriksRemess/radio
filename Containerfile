FROM rust:1-alpine AS builder

RUN apk add --no-cache build-base
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
RUN cargo build --release --offline --locked

FROM alpine:3

RUN apk add --no-cache ffmpeg
COPY --from=builder /build/target/release/linein-radio /usr/local/bin/linein-radio

WORKDIR /app
COPY radio.conf ./radio.conf

EXPOSE 8000
ENTRYPOINT ["/usr/local/bin/linein-radio"]
