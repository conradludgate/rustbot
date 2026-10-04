FROM lukemathwalker/cargo-chef:latest-rust-1.95-slim-bookworm AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
ARG FERRISBOT_GIT_TAG
ARG FERRISBOT_GIT_SHA
ENV FERRISBOT_GIT_TAG=${FERRISBOT_GIT_TAG}
ENV FERRISBOT_GIT_SHA=${FERRISBOT_GIT_SHA}
RUN SQLX_OFFLINE=true cargo build --release --bin main --locked

FROM debian:bookworm-slim
ARG APP=/usr/src/app
WORKDIR ${APP}

ENV TZ=Etc/UTC

COPY assets migrations ./
COPY --from=builder /app/target/release/main ./ferrisbot

ENTRYPOINT ["./ferrisbot"]
