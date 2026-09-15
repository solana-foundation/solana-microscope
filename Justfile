set dotenv-load := true
set shell := ["bash", "-uc"]

default:
    @just --list

setup: setup-hooks generate
    cargo fetch

setup-hooks:
    git config core.hooksPath .githooks
    @echo "✓ Git hooks configured"

_ensure-config:
    test -f microscope.toml || cp microscope.toml.example microscope.toml

# Rerun whenever microscope.toml or an IDL changes.
generate: _ensure-config
    ./scripts/generate-decoder.sh

build: generate
    cargo build --release -p microscope-indexer

# -p rather than --all: --all also reaches into generated decoder path deps.
fmt: generate
    cargo fmt -p microscope-indexer

fmt-check: generate
    cargo fmt -p microscope-indexer -- --check

lint-check: generate
    # Decoder crates are codegen'd by carbon-cli from IDLs; don't gate on their lints.
    cargo clippy -p microscope-indexer --all-targets --no-deps -- -D warnings

lint: fmt-check lint-check

test: generate
    cargo test --workspace

check: fmt-check lint-check test

infra-check:
    terraform fmt -check -recursive infra
    TF_DATA_DIR=.terraform-check terraform -chdir=infra/aws init -backend=false -input=false
    TF_DATA_DIR=.terraform-check terraform -chdir=infra/aws validate
    TF_DATA_DIR=.terraform-check terraform -chdir=infra/gcp init -backend=false -input=false
    TF_DATA_DIR=.terraform-check terraform -chdir=infra/gcp validate
    bash -n infra/deploy-microscope.sh
    bash -n infra/tests/reconciler-test.sh
    infra/tests/reconciler-test.sh

run: generate
    cargo run -p microscope-indexer -- run microscope.toml

generate-alerting: generate
    cargo run -p microscope-indexer -- generate-alerting microscope.toml grafana/provisioning/alerting

up: _ensure-config
    docker compose build
    docker compose run --rm --no-deps alerting-config
    docker compose up --detach --no-build
    docker compose up --detach --no-build --no-deps --force-recreate grafana

down:
    docker compose down

# Requires the stack running and RPC_URL set in .env or the environment.
# Grafana Cloud stacks push through Alloy:
#   just backfill 7d --loki-url http://alloy:3100 --loki-max-age 30d
backfill since='7d' *args='':
    docker compose run --rm --no-deps indexer backfill /etc/microscope/microscope.toml --since {{ since }} {{ args }}
