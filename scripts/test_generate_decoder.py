import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent


def struct(name, *fields):
    return {"name": name, "type": {"kind": "struct", "fields": [{"name": n, "type": t} for n, t in fields]}}


def idl_with(types, args):
    return {
        "name": "sample",
        "version": "0.1.0",
        "instructions": [{"name": "go", "accounts": [], "args": args}],
        "types": types,
    }


@unittest.skipIf(sys.version_info < (3, 11), "generate-decoder.sh needs python >= 3.11")
class GenerateDecoderIdlStep(unittest.TestCase):
    def run_script(self, idl):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "scripts").mkdir()
            for name in ("generate-decoder.sh", "idl_prune.py"):
                shutil.copy(SCRIPTS / name, root / "scripts" / name)
            tool = root / "scripts" / "ensure-decoder-tool.sh"
            tool.write_text("#!/usr/bin/env bash\nexit 0\n")
            tool.chmod(0o755)
            cli = root / "node_modules" / ".bin" / "carbon-cli"
            cli.parent.mkdir(parents=True)
            cli.write_text('#!/usr/bin/env bash\ntouch "$(dirname "$0")/../../carbon-cli-ran"\nexit 1\n')
            cli.chmod(0o755)
            (root / "bin").mkdir()
            (root / "bin" / "python3").symlink_to(sys.executable)
            (root / "package-lock.json").write_text("{}")
            (root / "idl.json").write_text(json.dumps(idl))
            (root / "microscope.toml").write_text(
                'program_id = "11111111111111111111111111111111"\nidl_path = "idl.json"\n'
            )
            env = {**os.environ, "PATH": f"{root / 'bin'}:/usr/bin:/bin"}
            proc = subprocess.run(
                ["bash", "scripts/generate-decoder.sh"], cwd=root, env=env, capture_output=True, text=True
            )
            return proc, (root / "carbon-cli-ran").exists(), (root / "crates").exists()

    def test_stops_at_the_idl_step_when_a_needed_type_cannot_be_pruned(self):
        idl = idl_with(
            [struct("Msg", ("keys", {"defined": "Missing<u8>"}))],
            [{"name": "m", "type": {"defined": "Msg"}}],
        )
        proc, generator_ran, crates_created = self.run_script(idl)
        self.assertEqual(proc.returncode, 1)
        lines = [line for line in proc.stderr.splitlines() if line.strip()]
        self.assertEqual(len(lines), 1, proc.stderr)
        self.assertIn("Msg", lines[0])
        self.assertNotIn("Traceback", proc.stderr)
        self.assertNotIn("unbound variable", proc.stderr)
        self.assertFalse(generator_ran, "carbon-cli must not run after the IDL step failed")
        self.assertFalse(crates_created)

    def test_a_prunable_idl_reaches_the_generator(self):
        idl = idl_with(
            [struct("Unused", ("keys", {"defined": "Missing<u8>"})), struct("Keep", ("x", "u8"))],
            [{"name": "k", "type": {"defined": "Keep"}}],
        )
        proc, generator_ran, _ = self.run_script(idl)
        self.assertIn("pruned IDL types with undefined references: Unused", proc.stderr)
        self.assertTrue(generator_ran, proc.stderr)


CARBON_STUB = """#!/usr/bin/env bash
while [[ $# -gt 0 ]]; do
    if [[ "$1" == "--out-dir" ]]; then out="$2"; fi
    shift
done
mkdir -p "$out/src/instructions"
touch "$out/src/instructions/cpi_event.rs"
printf '[package]\nname = "program-decoder"\n' > "$out/Cargo.toml"
printf 'mod base58;\npub struct ProgramDecoder;\n' > "$out/src/lib.rs"
"""


@unittest.skipIf(sys.version_info < (3, 11), "generate-decoder.sh needs python >= 3.11")
class GenerateDecoderStamp(unittest.TestCase):
    def test_changing_idl_prune_py_regenerates(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "scripts").mkdir()
            for name in ("generate-decoder.sh", "idl_prune.py"):
                shutil.copy(SCRIPTS / name, root / "scripts" / name)
            tool = root / "scripts" / "ensure-decoder-tool.sh"
            tool.write_text("#!/usr/bin/env bash\nexit 0\n")
            tool.chmod(0o755)
            cli = root / "node_modules" / ".bin" / "carbon-cli"
            cli.parent.mkdir(parents=True)
            cli.write_text(CARBON_STUB)
            cli.chmod(0o755)
            (root / "bin").mkdir()
            (root / "bin" / "python3").symlink_to(sys.executable)
            (root / "package-lock.json").write_text("{}")
            (root / "idl" / "squads").mkdir(parents=True)
            for name in ("v3.json", "v4.json", "smart-account-v0.1.json"):
                (root / "idl" / "squads" / name).write_text("{}")
            (root / "idl.json").write_text(json.dumps(idl_with([struct("A", ("x", "u8"))], [])))
            (root / "microscope.toml").write_text(
                'program_id = "11111111111111111111111111111111"\nidl_path = "idl.json"\n'
            )
            env = {**os.environ, "PATH": f"{root / 'bin'}:/usr/bin:/bin"}
            stamp = root / "crates" / "programs" / "0" / ".microscope-source.sha256"

            def run():
                return subprocess.run(
                    ["bash", "scripts/generate-decoder.sh"], cwd=root, env=env, capture_output=True, text=True
                )

            first = run()
            self.assertEqual(first.returncode, 0, first.stderr)
            before = stamp.read_text()

            unchanged = run()
            self.assertIn("crates/programs/0 is up to date", unchanged.stdout)
            self.assertEqual(stamp.read_text(), before)

            with open(root / "scripts" / "idl_prune.py", "a") as helper:
                helper.write("\n# touched\n")
            touched = run()
            self.assertIn("generated crates/programs/0", touched.stdout)
            self.assertNotEqual(stamp.read_text(), before)


if __name__ == "__main__":
    unittest.main()
