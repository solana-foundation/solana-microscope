FROM node:24.18.1-trixie-slim@sha256:ac39e4b5fcb2b1b34b20364fd58b2e898f3bb80731ee6f62a7536f9df3d6aadc AS node

FROM rust:1.97-slim-trixie@sha256:8e8cf8f7fd54a2d23d5a743b3a03f56e26b6c774276c33fa0595111704ebb15c AS chef
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev build-essential python3 \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked --version 0.1.78

FROM chef AS decoders
# carbon-cli needs node >= 20 (its deps use ES2024 regex flags); copy the
# pinned node build instead of relying on the distro package.
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx
COPY package.json package-lock.json ./
COPY scripts scripts
RUN ./scripts/ensure-decoder-tool.sh
# idl_path is an arbitrary repository-relative path, so generation needs the
# whole context. It reruns on any source change; the generated crates are
# deterministic, so the copies of them below stay cache hits.
COPY . .
RUN ./scripts/generate-decoder.sh
# The runtime resolves idl_path against the config's directory, so the IDL has
# to keep its relative path into the image. Only the configured one is needed:
# the decoder is built for a single program and main.rs rejects any other IDL.
RUN mkdir -p /staged \
    && cp --parents "$(python3 -c "import tomllib; print(tomllib.load(open('microscope.toml','rb'))['idl_path'])")" /staged/

FROM chef AS planner
COPY . .
COPY --from=decoders /app/crates/program-decoder crates/program-decoder
COPY --from=decoders /app/crates/squads-v3-decoder crates/squads-v3-decoder
COPY --from=decoders /app/crates/squads-v4-decoder crates/squads-v4-decoder
COPY --from=decoders /app/crates/squads-smart-account-decoder crates/squads-smart-account-decoder
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
# The decoder crates are path dependencies outside the workspace, and cargo-chef
# reads their manifests rather than reconstructing skeletons for them, so they
# have to exist before cook runs.
COPY --from=decoders /app/crates/program-decoder crates/program-decoder
COPY --from=decoders /app/crates/squads-v3-decoder crates/squads-v3-decoder
COPY --from=decoders /app/crates/squads-v4-decoder crates/squads-v4-decoder
COPY --from=decoders /app/crates/squads-smart-account-decoder crates/squads-smart-account-decoder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json -p microscope-indexer
COPY . .
RUN cargo build --release -p microscope-indexer

FROM debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
LABEL org.opencontainers.image.title="Solana Microscope" \
      org.opencontainers.image.description="Self-hosted monitoring and alerting for Solana programs" \
      org.opencontainers.image.source="https://github.com/solana-foundation/solana-microscope" \
      org.opencontainers.image.licenses="MIT"
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/microscope-indexer /usr/local/bin/microscope-indexer
COPY --from=decoders /staged /etc/microscope
WORKDIR /etc/microscope
EXPOSE 9090 9091
ENTRYPOINT ["microscope-indexer"]
CMD ["run", "/etc/microscope/microscope.toml"]
