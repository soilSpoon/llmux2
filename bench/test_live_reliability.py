import http.client
import json
import os
import tempfile
import unittest

import live_reliability as rel

CONFIG = """
[server]
listen = "127.0.0.1:8090"
client_keys = ["file:/tmp/client"]

[[credentials]]
id = "a"
kind = "api_key"
provider = "openai_compat"
key = "file:/srv/a.key"

[[credentials]]
id = "o"
kind = "oauth"
provider = "openai_compat"
token_url = "https://auth.example/token"
client_id = "x"
refresh_token = "file:/srv/o.key"

[[models]]
name = "m"

[[models.deployments]]
id = "m-0"
credential = "a"
base_url = "https://api.example.com/v1/"
upstream_model = "up-1"
priority = 0

[[models.deployments]]
id = "m-1"
credential = "o"
base_url = "https://oauth.example.com/v1"
upstream_model = "up-2"

[[models]]
name = "n"

[[models.deployments]]
id = "n-0"
credential = "a"
base_url = "https://api.example.com/v1"
upstream_model = "up-3"
priority = 2
"""


class ConfigTest(unittest.TestCase):
    def test_parses_and_renders_the_deployed_config_shape(self):
        cfg = rel.parse_toml_subset(CONFIG)
        self.assertEqual(cfg["server"]["client_keys"], ["file:/tmp/client"])
        self.assertEqual([d["id"] for d in cfg["models"][0]["deployments"]], ["m-0", "m-1"])
        self.assertEqual(cfg["models"][1]["deployments"][0]["priority"], 2)
        self.assertEqual(rel.parse_toml_subset(rel.render_config(cfg)), cfg)

    def test_one_api_key_target_per_base_url(self):
        targets = rel.targets_from_config(rel.parse_toml_subset(CONFIG))
        self.assertEqual([(t.base_url, t.key_ref, t.upstream_model) for t in targets],
                         [("https://api.example.com/v1", "file:/srv/a.key", "up-1")])

    def test_redacts_known_secrets_and_bearer_tokens(self):
        redact = rel.make_redactor(["sk-live-secret-value", "short"])
        self.assertEqual(redact("key sk-live-secret-value / Bearer abcdefghijkl / short"),
                         "key [REDACTED] / Bearer [REDACTED] / short")


class ProxyTest(unittest.TestCase):
    def setUp(self):
        self.proxy = rel.FaultProxy()
        self.upstream = rel.FakeUpstream()
        self.addCleanup(self.proxy.close)
        self.addCleanup(self.upstream.close)

    def post(self, url, payload):
        parts = rel.urllib.parse.urlsplit(url + "/chat/completions")
        conn = http.client.HTTPConnection(parts.hostname, parts.port, timeout=5)
        self.addCleanup(conn.close)
        conn.request("POST", parts.path, json.dumps(payload), {"Content-Type": "application/json", "Authorization": "Bearer offline-key"})
        response = conn.getresponse()
        return response.status, dict(response.getheaders()), response.read()

    def test_injected_status_is_returned_and_counted_without_forwarding(self):
        route = rel.Route(self.upstream.url, "status", status=429, headers={"Retry-After": "4"})
        status, headers, _ = self.post(self.proxy.add("x", route), {"model": "m", "messages": []})
        self.assertEqual((status, headers.get("Retry-After"), route.hits, route.statuses), (429, "4", 1, [429]))
        self.assertEqual(self.upstream.hits, 0)

    def test_pass_forwards_to_the_upstream_base_path(self):
        route = rel.Route(self.upstream.url)
        status, _, body = self.post(self.proxy.add("x", route), {"model": "m", "messages": []})
        self.assertEqual((status, route.statuses, self.upstream.hits), (200, [200], 1))
        self.assertIn("choices", json.loads(body))


class ReportTest(unittest.TestCase):
    def test_markdown_counts_verdicts(self):
        report = {"primary": "p", "fallback": "f", "binary": "b", "probes": [], "results": [
            {"scenario": "a", "expect": "x", "verdict": "PASS", "observed": "200", "note": ""},
            {"scenario": "b", "expect": "y", "verdict": "FAIL", "observed": "502 | z", "note": ""},
        ]}
        text = rel.markdown(report)
        self.assertIn("## Scenarios (1 FAIL, 1 PASS)", text)
        self.assertIn("| FAIL | b | y | 502 / z |", text)


if __name__ == "__main__":
    unittest.main()
