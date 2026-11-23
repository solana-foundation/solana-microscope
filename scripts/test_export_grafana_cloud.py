import json
import re
import shutil
import subprocess
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("export-grafana-cloud.sh")


def scope(expr, deployment="mainnet"):
    definition = re.search(r"^def scope_exprs.*?^  \);$", SCRIPT.read_text(), re.S | re.M).group(0)
    proc = subprocess.run(
        ["jq", "--raw-output", f'{definition} scope_exprs("{deployment}") | .expr'],
        input=json.dumps({"expr": expr}),
        capture_output=True,
        text=True,
        check=True,
    )
    return proc.stdout.strip()


@unittest.skipIf(shutil.which("jq") is None, "export-grafana-cloud.sh needs jq")
class ScopeExprs(unittest.TestCase):
    def test_merges_the_deployment_into_existing_matchers(self):
        self.assertEqual(
            scope('max(microscope_rpc_active_endpoint{role="fallback"}) or on() vector(0)'),
            'max(microscope_rpc_active_endpoint{deployment="mainnet", role="fallback"}) or on() vector(0)',
        )

    def test_adds_a_selector_to_bare_series(self):
        self.assertEqual(
            scope("sum(increase(microscope_transactions_total[10m])) > 0 unless sum(increase(loki_write_sent_entries_total[10m])) > 0"),
            'sum(increase(microscope_transactions_total{deployment="mainnet"}[10m])) > 0 unless sum(increase(loki_write_sent_entries_total{deployment="mainnet"}[10m])) > 0',
        )

    def test_scopes_the_log_stream_selector(self):
        self.assertEqual(
            scope('sum(count_over_time({service_name="microscope-indexer"} |= "x" [60s]))'),
            'sum(count_over_time({service_name="microscope-indexer", deployment="mainnet"} |= "x" [60s]))',
        )


if __name__ == "__main__":
    unittest.main()
