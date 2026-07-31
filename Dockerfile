# syntax=docker/dockerfile:1

# --- build -------------------------------------------------------------------
# Pinned to the same toolchain as rust-toolchain.toml so the image, CI, and a
# local build are the same compiler.
FROM rust:1.96-slim-bookworm AS builder

WORKDIR /build

# Dependencies first, in their own layer, so editing src/ does not rebuild the
# whole dependency graph.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY migrations ./migrations
COPY src ./src

# Cargo will not rebuild without a newer mtime than the stub it just compiled.
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --locked \
    && strip target/release/ledger-rs

# --- runtime -----------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# ca-certificates is required for a TLS connection to a managed Postgres.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Unprivileged: this process needs no root capability, and a ledger is a poor
# place to leave one lying around.
RUN useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin ledger

COPY --from=builder /build/target/release/ledger-rs /usr/local/bin/ledger-rs

USER 10001:10001
EXPOSE 3000

ENV BIND_ADDR=0.0.0.0:3000 \
    RUST_LOG=info,sqlx=warn \
    LOG_FORMAT=json

# Deliberately no HEALTHCHECK. The image carries no shell utilities to probe
# with (no curl, no wget), and Kubernetes ignores Docker HEALTHCHECK entirely.
# Point the orchestrator's probes at the endpoints instead:
#
#   livenessProbe  -> GET /health/live   (restart the process)
#   readinessProbe -> GET /health/ready  (shed traffic; checks the database)
#
# Keeping them distinct is what stops a brief database blip from restarting
# every healthy replica at once.
ENTRYPOINT ["/usr/local/bin/ledger-rs"]
