FROM lukemathwalker/cargo-chef:latest-rust-1.96-slim-bookworm@sha256:099be2ffa07bc150b242eeb855746b49d0bfbc1f2a99fee992cc46de2913de90 AS chef
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

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251
ARG APP=/usr/src/app
WORKDIR ${APP}

ENV TZ=Etc/UTC

COPY assets migrations ./
COPY --from=builder /app/target/release/main ./ferrisbot

ENTRYPOINT ["./ferrisbot"]
