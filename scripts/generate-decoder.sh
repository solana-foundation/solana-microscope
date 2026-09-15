#!/usr/bin/env bash
# Generates the deployment decoder plus the pinned Squads decoders used for
# multisig activity. Requires python3 >= 3.11 and node >= 20.
set -euo pipefail
cd "$(dirname "$0")/.."

CARBON_CLI="node_modules/.bin/carbon-cli"

if [[ ! -f microscope.toml ]]; then
    echo "microscope.toml not found — copy microscope.toml.example and edit it (or run 'just setup')" >&2
    exit 1
fi

eval "$(python3 - <<'PY'
import shlex
import tomllib

with open("microscope.toml", "rb") as f:
    cfg = tomllib.load(f)
print(f"PROGRAM_ID={shlex.quote(cfg['program_id'])}")
print(f"IDL_PATH={shlex.quote(cfg['idl_path'])}")
PY
)"

if [[ ! -f "$IDL_PATH" ]]; then
    echo "idl_path $IDL_PATH from microscope.toml does not exist" >&2
    echo "in a container build the file also has to survive .dockerignore, which excludes infra/, .local/, and node_modules/" >&2
    exit 1
fi

./scripts/ensure-decoder-tool.sh

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
TARGET_IDL="$TMP_DIR/idl.json"

# carbon-cli derives crate/type names from the IDL's program name; pin the
# deployment decoder to "program" so its Rust API is stable across IDLs.
eval "$(python3 - "$IDL_PATH" "$TARGET_IDL" <<'PY'
import json
import shlex
import sys

with open(sys.argv[1]) as f:
    idl = json.load(f)
if "program" in idl:
    idl["program"]["name"] = "program"
    standard = "codama"
    events = idl["program"].get("events") or []
else:
    if "name" in idl:
        idl["name"] = "program"
    if isinstance(idl.get("metadata"), dict) and "name" in idl["metadata"]:
        idl["metadata"]["name"] = "program"
    standard = "anchor"
    events = idl.get("events") or []
with open(sys.argv[2], "w") as f:
    json.dump(idl, f)
print(f"TARGET_STANDARD={shlex.quote(standard)}")
print(f"TARGET_DECLARES_EVENTS={shlex.quote('true' if events else 'false')}")
PY
)"

generate_decoder() {
    local source_idl="$1"
    local render_idl="$2"
    local standard="$3"
    local program_id="$4"
    local out_dir="$5"
    local require_events="$6"
    local with_base58="$7"
    local stamp_file="$out_dir/.microscope-source.sha256"
    local source_hash

    source_hash="$(python3 - "$source_idl" "$0" "package-lock.json" "$standard" "$program_id" <<'PY'
import hashlib
import pathlib
import sys

digest = hashlib.sha256()
for path in sys.argv[1:4]:
    data = pathlib.Path(path).read_bytes()
    digest.update(len(data).to_bytes(8, "big"))
    digest.update(data)
for value in sys.argv[4:]:
    data = value.encode()
    digest.update(len(data).to_bytes(8, "big"))
    digest.update(data)
print(digest.hexdigest())
PY
)"

    if [[ -f "$out_dir/Cargo.toml" && -f "$stamp_file" && "$(<"$stamp_file")" == "$source_hash" ]]; then
        echo "$out_dir is up to date"
        return
    fi

    "$CARBON_CLI" parse \
        --idl "$render_idl" \
        --standard "$standard" \
        --as-crate \
        --with-postgres false \
        --with-graphql false \
        --with-serde true \
        --with-base58 "$with_base58" \
        --program-id "$program_id" \
        --out-dir "$out_dir"

    if [[ "$require_events" == "true" && ! -f "$out_dir/src/instructions/cpi_event.rs" ]]; then
        echo "$source_idl declares events but Carbon generated no CpiEvent decoder" >&2
        exit 1
    fi

    if [[ "$with_base58" == "true" ]] && ! grep -q 'mod base58' "$out_dir/src/lib.rs"; then
        echo "$out_dir serializes pubkeys as byte arrays; every indexed record expects base58" >&2
        exit 1
    fi

    printf '%s\n' "$source_hash" > "$stamp_file"
    echo "generated $out_dir for $program_id from $source_idl"
}

generate_decoder "$IDL_PATH" "$TARGET_IDL" "$TARGET_STANDARD" "$PROGRAM_ID" \
    "crates/program-decoder" "$TARGET_DECLARES_EVENTS" true

# The indexer compiles this in and compares it against the IDL its runtime
# config points at.
python3 - "$IDL_PATH" <<'PY' > crates/program-decoder/.microscope-idl.sha256
import hashlib
import pathlib
import sys

print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY

generate_decoder "idl/squads/v3.json" "idl/squads/v3.json" anchor \
    "SMPLecH534NA9acpos4G6x7uf3LWbCAwZQE9e8ZekMu" "crates/squads-v3-decoder" false false
generate_decoder "idl/squads/v4.json" "idl/squads/v4.json" anchor \
    "SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf" "crates/squads-v4-decoder" false false
generate_decoder "idl/squads/smart-account-v0.1.json" "idl/squads/smart-account-v0.1.json" anchor \
    "SMRTzfY6DfH5ik3TKiyLFfXexV8uSG3d2UksSCYdunG" "crates/squads-smart-account-decoder" false false
