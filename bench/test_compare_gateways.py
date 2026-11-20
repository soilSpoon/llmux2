import io
import json
import os
import tempfile
import threading
import time
import unittest
from contextlib import redirect_stderr
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from html.parser import HTMLParser
from pathlib import Path
from unittest.mock import patch

from bench import compare_gateways as compare


class ReportTables(HTMLParser):
    def __init__(self, source):
        super().__init__()
        self.tables = {}
        self.rows = self.row = self.cell = None
        self.feed(source)

    def handle_starttag(self, tag, attrs):
        if tag == 'table':
            self.rows = []
            self.tables[dict(attrs).get('aria-label')] = self.rows
        elif tag == 'tr':
            self.row = []
        elif tag in ('td', 'th'):
            self.cell = []

    def handle_data(self, data):
        if self.cell is not None:
            self.cell.append(data)

    def handle_endtag(self, tag):
        if tag in ('td', 'th'):
            self.row.append(' '.join(' '.join(self.cell).split()))
            self.cell = None
        elif tag == 'tr':
            self.rows.append(self.row)
        elif tag == 'table':
            self.rows = None


def result(gateway, model="m", kind="chat", error_class="ok", latency_ms=10):
    return dict(gateway=gateway, model=model, kind=kind, error_class=error_class,
                status=200 if error_class == "ok" else 429, latency_ms=latency_ms,
                ttft_ms=None, done=None, deployment=None, attempts=None,
                fallback=None, routing_headers={}, error=None)


class ComparisonTest(unittest.TestCase):
    def test_report_has_compact_summary_model_overview_and_run_list(self):
        rows = [result('omniroute'), result('llmux'),
                result('omniroute', kind='stream'),
                result('llmux', kind='stream', error_class='rate_limited')]
        data = compare.aggregate(rows, ['omniroute', 'llmux'])
        data['run'] = {'completed_at': '2026-11-20T09:00:00Z', 'timeout_s': 45,
                       'max_requests': 80, 'note': 'A single sample'}
        source = compare.render_html(data)
        tables = ReportTables(source).tables
        self.assertEqual(tables['Summary'][0], ['Metric', 'omniroute', 'llmux'])
        self.assertIn(['Successes / requests', '2 / 2', '1 / 2'], tables['Summary'])
        self.assertIn(['Error classes', '—', 'rate_limited: 1'], tables['Summary'])
        self.assertIn(['p50 all (ms)', '10', '10'], tables['Summary'])
        self.assertIn(['p50 success (ms)', '10', '10'], tables['Summary'])
        self.assertIn(['p50 TTFT (ms)', '—', '—'], tables['Summary'])
        self.assertEqual(tables['Models'][0], ['Model', 'Chat', 'Stream', 'Responses', 'Verdict'])
        overview = tables['Models'][1]
        self.assertEqual(overview[0], 'm')
        self.assertIn('200 / 200', overview[1])
        self.assertIn('200 / 429', overview[2])
        self.assertEqual(overview[3:], ['—', 'llmux-only fail'])
        self.assertLess(source.index('aria-label="Models"'), source.index('aria-label="Request details"'))
        self.assertIn('<dl', source)
        self.assertIn('A single sample', source)
        self.assertNotIn('&quot;completed_at&quot;', source)
        self.assertIn('match: 1', source)
        self.assertIn('llmux-only fail: 1', source)
        self.assertIn('both fail: 0', source)

    def test_case_cells_show_human_values_and_only_candidate_attempt_evidence(self):
        rows = [result('omniroute'), result('llmux'),
                result('omniroute', kind='stream'), result('llmux', kind='stream')]
        rows[0]['attempts'] = 7
        rows[1].update(attempts=1, fallback=False, deployment='winner')
        rows[2].update(done=False)
        rows[3].update(done=True, attempts=2, fallback=True, ttft_ms=5)
        source = compare.render_html(compare.aggregate(rows, ['omniroute', 'llmux']))
        cells = ReportTables(source).tables['Request details']
        chat = next(r for r in cells if r[0] == 'chat')
        stream = next(r for r in cells if r[0] == 'stream')
        self.assertNotIn('DONE', ' '.join(chat))
        self.assertNotIn('attempt', chat[1])
        self.assertIn('1 attempt', chat[2])
        self.assertNotIn('(fallback)', chat[2])
        self.assertIn('winner', chat[2])
        self.assertIn('DONE ✗', stream[1])
        self.assertIn('DONE ✓', stream[2])
        self.assertIn('2 attempts (fallback)', stream[2])
        for literal in ('None', 'True', 'False'):
            self.assertNotIn(literal, ' '.join(' '.join(r) for r in cells))

    def test_case_bytes_are_fixed_and_pairs_alternate_before_cap_check(self):
        cases = compare.generate_cases(["m", "m", "n"], responses=2)
        self.assertEqual([(c["model"], c["kind"]) for c in cases], [
            ("m", "chat"), ("m", "stream"), ("n", "chat"), ("n", "stream"),
            ("m", "responses"), ("n", "responses")])
        self.assertEqual(json.loads(cases[0]["body"])["max_tokens"], 16)
        self.assertFalse(json.loads(cases[0]["body"])["stream"])
        plan = compare.make_plan(cases, ["omniroute", "llmux"], 12)
        self.assertEqual([g for _, g in plan[:4]], ["omniroute", "llmux", "llmux", "omniroute"])
        self.assertEqual(plan[0][0]["body"], plan[1][0]["body"])
        with self.assertRaises(ValueError):
            compare.make_plan(cases, ["omniroute", "llmux"], 11)

    def test_empty_models_bad_cap_and_response_count_are_rejected(self):
        for models, count in [([], 0), (["m"], -1), (["m"], 2), ([{}], 0)]:
            with self.assertRaises(ValueError):
                compare.generate_cases(models, count)
        with self.assertRaises(ValueError):
            compare.make_plan(compare.generate_cases(["m"]), ["a", "b"], 0)

    def test_coverage_filters_imported_names_after_top_traffic_cutoff(self):
        coverage = {"rows": [{"name": "low", "calls": 1, "present": True},
                             {"name": "missing", "calls": 99, "present": False},
                             {"name": "used", "calls": 50, "present": True}]}
        self.assertEqual(compare.coverage_models(coverage, 2), ["used"])
        self.assertEqual(compare.coverage_models({"rows": []}, 25), [])
        with self.assertRaises(ValueError):
            compare.coverage_models({"rows": "invalid"}, 25)
        with self.assertRaises(ValueError):
            compare.coverage_models([], 25)

    def test_result_classes_do_not_confuse_fast_errors_with_success(self):
        for status, code, transport, expected in [
            (200, None, None, "ok"), (429, None, None, "rate_limited"),
            (400, "context_length_exceeded", None, "gateway_reject"),
            (404, "unknown_model", None, "gateway_reject"),
            (404, "model_not_found", None, "upstream_4xx"),
            (502, None, None, "upstream_5xx"),
            (200, "503", None, "upstream_5xx"),
            (None, None, "timeout", "timeout"),
            (200, None, "connection", "connection")]:
            with self.subTest(expected=expected):
                self.assertEqual(compare.classify(status, code, transport), expected)

    def test_cli_cap_is_checked_before_any_request_or_output(self):
        with tempfile.TemporaryDirectory() as d, patch.dict(os.environ, {"COMPARE_KEY": "test-key"}):
            output = Path(d) / "run"
            args = ['--gateway', 'omniroute', 'http://127.0.0.1:1/v1', 'env:COMPARE_KEY',
                    '--gateway', 'llmux', 'http://127.0.0.1:2/v1', 'env:COMPARE_KEY',
                    '--model', 'm', '--max-requests', '3', '--output', str(output)]
            with patch.object(compare, 'send') as send, redirect_stderr(io.StringIO()):
                self.assertEqual(compare.main(args), 2)
                send.assert_not_called()
                self.assertFalse(output.exists())

    def test_routing_header_filter_keeps_discovered_fallback_but_not_secrets(self):
        headers = [('x-omniroute-fallback-used', 'true'), ('x-api-key', 'private'),
                   ('set-cookie', 'secret'), ('x-omniroute-token', 'private'),
                   ('x-omniroute-provider', 'provider'), ('x-omniroute-route', 'https://user:pass@host/private')]
        safe = compare.routing_headers(headers, [])
        self.assertEqual(safe['x-omniroute-fallback-used'], 'true')
        self.assertNotIn('private', json.dumps(safe))
        self.assertNotIn('user:pass', json.dumps(safe))
        self.assertNotIn('x-omniroute-agent-goal', compare.routing_headers(
            [('x-omniroute-agent-goal', 'must not store prompts')], []))

    def test_long_deployment_names_are_retained_while_opaque_credentials_are_removed(self):
        headers = [('x-llmux-deployment', 'deepseek-v4-flash-fast-deployment-0'),
                   ('x-omniroute-provider', 'abcdefghijklmnopqrstuvwxy123456')]
        safe = compare.routing_headers(headers, [])
        self.assertEqual(safe['x-llmux-deployment'], 'deepseek-v4-flash-fast-deployment-0')
        self.assertNotIn('abcdefghijklmnopqrstuvwxy123456', json.dumps(safe))

    def test_redaction_and_html_escape_untrusted_errors_and_headers(self):
        secret = "known-test-secret-value"
        message = 'Bearer abcdefghijklmnop sk-live1234567890 https://user:pass@host/path?key=x '+secret
        clean = compare.redact(message, [secret])
        for value in [secret, "abcdefghijklmnop", "sk-live1234567890", "user:pass", "key=x"]:
            self.assertNotIn(value, clean)
        rows = [result("omniroute"), result("llmux", error_class="rate_limited")]
        rows[1]["error"] = '<script>alert("x")</script>'
        data = compare.aggregate(rows, ["omniroute", "llmux"])
        data['run'] = {'note': 'Responses pairs rechecked after classifier correction'}
        report = compare.render_html(data)
        self.assertNotIn('<script>alert', report)
        self.assertIn("&lt;script&gt;", report)
        self.assertIn("llmux-only fail", report)
        self.assertNotIn('src="http', report)
        self.assertIn('Responses pairs rechecked after classifier correction', report)

    def test_aggregation_keeps_success_latency_separate_and_preserves_each_case(self):
        rows = [result("omniroute", latency_ms=10), result("llmux", latency_ms=20),
                result("omniroute", kind="stream", error_class="timeout", latency_ms=1000),
                result("llmux", kind="stream", latency_ms=30)]
        report = compare.aggregate(rows, ["omniroute", "llmux"])
        self.assertEqual(report["summary"]["verdicts"], {"match": 1, "omniroute-only fail": 1})
        self.assertEqual(report["summary"]["gateways"]["omniroute"]["success_p50_ms"], 10)
        self.assertEqual(report["summary"]["gateways"]["omniroute"]["p50_ms"], 505)
        self.assertEqual(report["models"][0]["verdict"], "omniroute-only fail")
        self.assertEqual(len(report["pairs"]), 2)
        empty = compare.aggregate([], ["omniroute", "llmux"])
        self.assertIsNone(empty["summary"]["gateways"]["llmux"]["p50_ms"])
        self.assertIn("No results", compare.render_html(empty))
        with self.assertRaises(ValueError):
            compare.aggregate([result("omniroute")], ["omniroute", "llmux"])

    def test_both_failure_and_mixed_direction_verdicts_are_not_matches(self):
        rows = [result("a", error_class="timeout"), result("b", error_class="rate_limited")]
        self.assertEqual(compare.aggregate(rows, ["a", "b"])["pairs"][0]["verdict"], "both fail")
        rows = [result("a"), result("b", error_class="timeout"),
                result("a", kind="stream", error_class="timeout"), result("b", kind="stream")]
        self.assertEqual(compare.aggregate(rows, ["a", "b"])["models"][0]["verdict"], "both fail")

    def test_secret_inputs_reject_literals_and_urls_reject_userinfo_query(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "key"
            p.write_text("secret-value\n")
            self.assertEqual(compare.read_key("file:"+str(p)), "secret-value")
        with patch.dict(os.environ, {"COMPARE_TEST_KEY": "env-secret"}):
            self.assertEqual(compare.read_key("env:COMPARE_TEST_KEY"), "env-secret")
        for ref in ["literal-secret", "env:UNSET_COMPARE_KEY"]:
            with self.assertRaises(ValueError):
                compare.read_key(ref)
        for url in ["http://user:pass@localhost/v1", "http://localhost/v1?key=oops", "ftp://localhost"]:
            with self.assertRaises(ValueError):
                compare.validate_url(url)


class TransportTest(unittest.TestCase):
    def setUp(self):
        self.received = []
        received = self.received

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                received.append(body)
                req = json.loads(body)
                self.send_response(200)
                if req['model'] in ('short-body', 'sized-body'):
                    payload = (b'data: {"choices":[{"delta":{}}]}\n\ndata: [DONE]\n\n' if req.get('stream')
                               else b'{"choices":[{"message":{}}]}')
                    self.send_header('Content-Length', str(len(payload) + (10 if req['model'] == 'short-body' else 0)))
                    self.end_headers()
                    self.wfile.write(payload)
                    return
                self.send_header("x-llmux-deployment", "deployment-1")
                self.send_header("x-llmux-attempts", "2")
                self.send_header("x-omniroute-route-class", "inference")
                self.send_header("x-api-key", "never-store-this")
                self.end_headers()
                if req['model'] == 'slow':
                    time.sleep(0.15)
                if req.get("stream"):
                    if req['model'] == 'invalid-delta':
                        self.wfile.write(b'data: {"choices":[{"delta":1}]}\n\ndata: [DONE]\n\n')
                        return
                    if req['model'] in ('cr-only', 'bom-error'):
                        if req['model'] == 'bom-error':
                            self.wfile.write(b'\xef\xbb\xbfdata: {"error":{"code":"unknown_model","message":"rejected"}}\r\rdata: [DONE]\r\r')
                        else:
                            self.wfile.write(b'data: {"choices":[{"delta":{"content":"hello"}}]}\r\rdata: [DONE]\r\r')
                        return
                    if req['model'] == 'malformed':
                        self.wfile.write(b'data: {"choices":1}\n\ndata: [DONE]\n\n')
                        return
                    self.wfile.write(b'data: {"choices":[{"delta":{"role":"assistant"}}]}\r\n\r\n')
                    self.wfile.flush()
                    time.sleep(0.02)
                    self.wfile.write(b'data: {"choices":[{"delta":{"content":"secret-response-text"}}]}\r\n\r\n')
                    if req["model"] != "broken":
                        self.wfile.write(b'data: [DONE]\r\n\r\n')
                elif req['model'] in ('scalar-items', 'empty-items'):
                    field = 'output' if self.path.endswith('/responses') else 'choices'
                    items = [1] if req['model'] == 'scalar-items' else []
                    self.wfile.write(json.dumps({field: items}).encode())
                elif req["model"] == "bad":
                    self.wfile.write(b'{"error":{"message":"sk-secret1234567890123456789", "code":"unknown_model"}}')
                elif self.path.endswith('/responses'):
                    self.wfile.write(b'{"error":null,"output":[{"type":"message","content":[{"text":"secret-response-text"}]}]}')
                else:
                    self.wfile.write(b'{"choices":[{"message":{"content":"secret-response-text"}}]}')

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.gateway = dict(name="llmux", base_url="http://127.0.0.1:%d/v1" % self.server.server_port,
                            key="test-client-key")

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def test_identical_bytes_headers_and_content_ttft_without_content_retention(self):
        case = compare.generate_cases(["m"])[1]
        a = compare.send(self.gateway, case, 2, ["test-client-key"])
        b = compare.send(self.gateway, case, 2, ["test-client-key"])
        self.assertEqual(self.received, [case["body"], case["body"]])
        self.assertEqual(a["error_class"], "ok")
        self.assertTrue(a["done"])
        self.assertGreaterEqual(a["ttft_ms"], 15)
        self.assertGreaterEqual(a["latency_ms"], a["ttft_ms"])
        self.assertTrue(a["fallback"])
        self.assertEqual(a["routing_headers"]["x-omniroute-route-class"], "inference")
        self.assertNotIn("never-store-this", json.dumps(a))
        self.assertNotIn("secret-response-text", json.dumps([a, b]))

    def test_incomplete_stream_and_error_in_http_200_are_failures(self):
        broken = compare.send(self.gateway, compare.generate_cases(["broken"])[1], 2, [])
        self.assertEqual(broken["error_class"], "connection")
        self.assertFalse(broken["done"])
        bad = compare.send(self.gateway, compare.generate_cases(["bad"])[0], 2, [])
        self.assertEqual(bad["error_class"], "gateway_reject")
        self.assertNotIn("sk-secret", json.dumps(bad))

    def test_deadline_bounds_wait_for_body_and_does_not_reclassify_http_200_as_ok(self):
        slow = compare.send(self.gateway, compare.generate_cases(['slow'])[0], 0.03, [])
        self.assertEqual(slow['status'], 200)
        self.assertEqual(slow['error_class'], 'timeout')
        self.assertLess(slow['latency_ms'], 140)

    def test_malformed_sse_is_a_result_not_a_crashed_comparison(self):
        row = compare.send(self.gateway, compare.generate_cases(['malformed'])[1], 2, [])
        self.assertEqual(row['status'], 200)
        self.assertEqual(row['error_class'], 'connection')

    def test_cr_only_events_complete_and_leading_bom_does_not_hide_errors(self):
        row = compare.send(self.gateway, compare.generate_cases(['cr-only'])[1], 2, [])
        self.assertEqual(row['error_class'], 'ok')
        self.assertTrue(row['done'])
        self.assertIsNotNone(row['ttft_ms'])
        row = compare.send(self.gateway, compare.generate_cases(['bom-error'])[1], 2, [])
        self.assertEqual(row['error_class'], 'gateway_reject')
        self.assertTrue(row['done'])

    def test_successful_responses_with_null_error_is_not_a_failure(self):
        case = compare.generate_cases(['m'], responses=1)[-1]
        row = compare.send(self.gateway, case, 2, [])
        self.assertEqual(row['status'], 200)
        self.assertEqual(row['error_class'], 'ok')
        self.assertIsNone(row['error'])
        self.assertNotIn('secret-response-text', json.dumps(row))

    def test_invalid_success_collections_and_stream_deltas_fail(self):
        for model in ('scalar-items', 'empty-items'):
            cases = compare.generate_cases([model], responses=1)
            for case in (cases[0], cases[-1]):
                with self.subTest(model=model, kind=case['kind']):
                    row = compare.send(self.gateway, case, 2, [])
                    self.assertEqual(row['error_class'], 'connection')
        row = compare.send(self.gateway, compare.generate_cases(['invalid-delta'])[1], 2, [])
        self.assertEqual(row['error_class'], 'connection')

    def test_content_length_requires_complete_body_even_after_stream_done(self):
        for model, expected in (('short-body', 'connection'), ('sized-body', 'ok')):
            for case in compare.generate_cases([model]):
                with self.subTest(model=model, kind=case['kind']):
                    row = compare.send(self.gateway, case, 2, [])
                    self.assertEqual(row['status'], 200)
                    self.assertEqual(row['error_class'], expected)


if __name__ == "__main__":
    unittest.main()
