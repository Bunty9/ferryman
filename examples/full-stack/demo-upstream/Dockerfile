# Build context is the repo root (workspace build), not this directory.
#   docker build -f examples/full-stack/demo-upstream/Dockerfile -t demo-upstream:dev .

FROM rust:1-bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p demo-upstream

FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=builder /app/target/release/demo-upstream /demo-upstream
EXPOSE 8080
ENTRYPOINT ["/demo-upstream"]
