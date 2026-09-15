FROM node:24.18.0-trixie-slim@sha256:5301bbf5e8046148348b1dea15436326f43c579031f8d76654a631225bdfe467 AS node

FROM rust:1.97-slim-trixie@sha256:1ac626cf2baacc6c87631f4c224391d7f5b3d3e6ba16b1c0f640232d7db172c8 AS chef
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

FROM debian:trixie-slim@sha256:9bb8a3626890e084ab54e888fdd7c4b6d2f119071cd4c5dc5fecb4d73062aa5f
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
