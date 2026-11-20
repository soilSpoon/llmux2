import io
import unittest
from unittest.mock import MagicMock, Mock, call, patch

import mock_upstream as bench


class BenchmarkTest(unittest.TestCase):
    def test_p99_is_nearest_rank(self):
        self.assertEqual(bench.p99(list(reversed(range(100)))), 98)

    def test_pairs_alternate_run_order_but_keep_direct_first_in_results(self):
        measure = Mock(side_effect=[2, 2.25, 4.5, 4, 1, 30])
        pairs = bench.paired_rounds(3, measure)
        self.assertEqual(measure.call_args_list, [
            call(False), call(True), call(True), call(False), call(False), call(True),
        ])
        self.assertEqual(pairs, [(2, 2.25), (4, 4.5), (1, 30)])
        self.assertEqual(bench.overhead_verdict([gateway - direct for direct, gateway in pairs]),
                         (0.5, "PASS"))

    def test_limit_is_strict_and_uses_the_median(self):
        self.assertEqual(bench.overhead_verdict([-0.25, 0.75, 1.25, 10]), (1.0, "FAIL"))

    def test_json_latency_can_include_time_since_scheduled_arrival(self):
        chat = bench.Chat("backend-model", bench.UPSTREAM_KEY, False)
        response = io.BytesIO(b'{"choices":[{"message":{"content":"xxxxxxxx"}}]}')
        with patch.object(chat, "start", return_value=response), patch.object(
            bench.time, "perf_counter", return_value=3
        ):
            self.assertEqual(chat.request(None, scheduled=1), (2, None))

    def test_arrivals_keep_fixed_schedule_when_dispatch_falls_behind(self):
        with patch.object(
            bench.time, "perf_counter", side_effect=[0, 0, 1.5, 2.5, 2.75]
        ), patch.object(bench.time, "sleep") as sleep:
            self.assertEqual(list(bench.arrivals(4, 1)), [0, 1, 2, 3])
            sleep.assert_called_once_with(0.25)

    def test_ttft_uses_complete_content_event_not_role_event(self):
        response = MagicMock()
        response.__enter__.return_value = response
        response.read1.side_effect = [
            b'data: {"choices":[{"delta":{"role":"assistant"}}]}\n\n'
            b'data: {"choices":[{"delta":{"content":',
            b'"x"}}]}\n',
            b'\n' + b'data: {"choices":[{"delta":{"content":"x"}}]}\n\n' * 7,
            b'data: [DONE]\n\n',
            b'',
        ]
        chat = bench.Chat("backend-model", bench.UPSTREAM_KEY, True)
        with patch.object(chat, "start", return_value=response), patch.object(
            bench.time, "perf_counter", side_effect=[0, 1, 2, 3, 4, 5]
        ):
            self.assertEqual(chat.request(None), (5, 3))


if __name__ == "__main__":
    unittest.main()
