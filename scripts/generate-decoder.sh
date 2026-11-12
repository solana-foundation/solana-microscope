#!/usr/bin/env bash
# Generates one decoder per monitored program plus the pinned Squads decoders
# used for multisig activity. Requires python3 >= 3.11 and node >= 20.
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
programs = cfg.get("programs") or [cfg]
print("PROGRAM_IDS=(" + " ".join(shlex.quote(p["program_id"]) for p in programs) + ")")
print("IDL_PATHS=(" + " ".join(shlex.quote(p["idl_path"]) for p in programs) + ")")
PY
)"

for idl_path in "${IDL_PATHS[@]}"; do
    if [[ ! -f "$idl_path" ]]; then
        echo "idl_path $idl_path from microscope.toml does not exist" >&2
        echo "in a container build the file also has to survive .dockerignore, which excludes infra/, .local/, and node_modules/" >&2
        exit 1
    fi
done

./scripts/ensure-decoder-tool.sh

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

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

# carbon-cli derives crate and type names from the IDL's program name; pin them
# to "program", "program_1", ... so the generated Rust API is stable across IDLs.
for index in "${!PROGRAM_IDS[@]}"; do
    target_idl="$TMP_DIR/idl-$index.json"
    eval "$(python3 - "${IDL_PATHS[$index]}" "$target_idl" "$index" <<'PY'
import json
import shlex
import sys

name = "program" if sys.argv[3] == "0" else f"program_{sys.argv[3]}"
with open(sys.argv[1]) as f:
    idl = json.load(f)
if "program" in idl:
    idl["program"]["name"] = name
    standard = "codama"
    events = idl["program"].get("events") or []
else:
    if "name" in idl:
        idl["name"] = name
    if isinstance(idl.get("metadata"), dict) and "name" in idl["metadata"]:
        idl["metadata"]["name"] = name
    standard = "anchor"
    events = idl.get("events") or []
with open(sys.argv[2], "w") as f:
    json.dump(idl, f)
print(f"TARGET_STANDARD={shlex.quote(standard)}")
print(f"TARGET_DECLARES_EVENTS={shlex.quote('true' if events else 'false')}")
PY
)"
    generate_decoder "${IDL_PATHS[$index]}" "$target_idl" "$TARGET_STANDARD" \
        "${PROGRAM_IDS[$index]}" "crates/programs/$index" "$TARGET_DECLARES_EVENTS" true
done

# The indexer depends on this one crate whatever the number of programs. It
# compiles each IDL's hash in and compares it against the IDL its runtime
# config points at.
python3 - "${IDL_PATHS[@]}" <<'PY'
import hashlib
import pathlib
import shutil
import sys
import tomllib

root = pathlib.Path("crates/programs")
dependencies = []
modules = []
callbacks = []
for index, idl_path in enumerate(sys.argv[1:]):
    crate = root / str(index)
    package = tomllib.loads((crate / "Cargo.toml").read_text())["package"]["name"]
    prefix = "Program" if index == 0 else f"Program{index}"
    if f"pub struct {prefix}Decoder;" not in (crate / "src/lib.rs").read_text():
        sys.exit(f"{crate} does not define {prefix}Decoder; carbon-cli changed its naming")
    events = (crate / "src/instructions/cpi_event.rs").is_file()
    idl_sha256 = hashlib.sha256(pathlib.Path(idl_path).read_bytes()).hexdigest()
    dependencies.append(f'{package} = {{ path = "{index}", features = ["base58"] }}')
    instructions = f"{prefix}Instruction as Instruction" + (", CpiEvent" if events else "")
    modules.append(
        f"pub mod p{index} {{\n"
        f"    pub use {package.replace('-', '_')}::{{\n"
        f"        instructions::{{{instructions}}},\n"
        f"        {prefix}Decoder as Decoder, PROGRAM_ID,\n"
        f"    }};\n"
        f'    pub const IDL_SHA256: &str = "{idl_sha256}";\n'
        f"}}\n"
    )
    callbacks.append(f"        $callback!(p{index}, {'events' if events else 'no_events'});")

manifest = (
    "[package]\n"
    'name = "microscope-programs"\n'
    'version = "0.1.0"\n'
    'edition = "2021"\n'
    "publish = false\n\n"
    "[lib]\n"
    'path = "src/lib.rs"\n\n'
    "[dependencies]\n" + "\n".join(dependencies) + "\n"
)
library = (
    "\n".join(modules)
    + "\n#[macro_export]\nmacro_rules! for_each_program {\n    ($callback:ident) => {\n"
    + "\n".join(callbacks)
    + "\n    };\n}\n"
)
(root / "src").mkdir(exist_ok=True)
for stale in root.iterdir():
    if stale.name.isdigit() and int(stale.name) >= len(dependencies):
        shutil.rmtree(stale)
for path, content in ((root / "Cargo.toml", manifest), (root / "src/lib.rs", library)):
    if not path.is_file() or path.read_text() != content:
        path.write_text(content)
PY

generate_decoder "idl/squads/v3.json" "idl/squads/v3.json" anchor \
    "SMPLecH534NA9acpos4G6x7uf3LWbCAwZQE9e8ZekMu" "crates/squads-v3-decoder" false false
generate_decoder "idl/squads/v4.json" "idl/squads/v4.json" anchor \
    "SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf" "crates/squads-v4-decoder" false false
generate_decoder "idl/squads/smart-account-v0.1.json" "idl/squads/smart-account-v0.1.json" anchor \
    "SMRTzfY6DfH5ik3TKiyLFfXexV8uSG3d2UksSCYdunG" "crates/squads-smart-account-decoder" false false
