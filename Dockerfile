# ============================================================================
# Stage 1: Build
# ============================================================================
FROM rust:bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
      cmake libssl-dev libssh2-1-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Cache dependency builds: copy manifests first, then compile dependencies.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

# Build the real project.
COPY . .
# Remove the dummy skot package artifacts so cargo rebuilds the real crate and library.
RUN rm -rf target/release/deps/libskot* target/release/deps/skot* target/release/.fingerprint/skot-* \
    && cargo build --release --locked

# ============================================================================
# Stage 2: Runtime
# ============================================================================
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
      libssl3 libssh2-1 ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

# Run as a non-root user.
RUN useradd --create-home --shell /bin/bash skot
USER skot
WORKDIR /home/skot

COPY --from=builder /src/target/release/skot /usr/local/bin/skot

ENTRYPOINT ["skot"]
