# syntax=docker/dockerfile:1

FROM rust:1.96-bookworm AS chef
RUN cargo install cargo-chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin switchboard

FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=builder /app/target/release/switchboard /app/switchboard
# Config is read from the working directory at runtime; migrations moved with the crate.
COPY config/ /app/config/
COPY crates/switchboard/migrations/ /app/migrations/
ENV RUST_LOG=info
EXPOSE 8080
USER nonroot:nonroot
ENTRYPOINT ["/app/switchboard"]
