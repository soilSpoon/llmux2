import unittest
from unittest.mock import MagicMock, patch

import mock_upstream as bench


class BenchmarkTest(unittest.TestCase):
    def test_p99_is_nearest_rank(self):
        self.assertEqual(bench.p99(list(reversed(range(100)))), 98)

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
