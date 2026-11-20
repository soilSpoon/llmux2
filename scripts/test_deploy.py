import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import contextlib
import io
import subprocess

SPEC = importlib.util.spec_from_file_location(
    "llmux2_ops", Path(__file__).resolve().parents[1] / "deploy/llmux2_ops.py"
)
ops = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ops)


class DeploymentTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.binary = self.root / "source-bin"
        self.config = self.root / "source.toml"
        self.binary.write_bytes(b"binary")
        self.config.write_text("config")
        self.active = {"a"}
        self.calls = []
        self.healthy = True
        self.fail_start = False
        self.fail_check = False
        self.ops = ops.Deployment(self.root, self.systemctl, self.check,
                                  lambda _: self.healthy)

    def systemctl(self, action, slot):
        self.calls.append((action, slot))
        if action == "is-active":
            return slot in self.active
        if action == "start":
            if self.fail_start:
                raise RuntimeError("start failed")
            self.active.add(slot)
        if action == "stop":
            self.active.discard(slot)
        return True

    def check(self, binary, config):
        self.calls.append(("check", str(binary), str(config)))
        if self.fail_check:
            raise RuntimeError("invalid config")
        return "127.0.0.1:8080"

    def test_handoff_installs_idle_slot_and_records_reboot_target(self):
        self.assertEqual(self.ops.deploy(self.binary, self.config), "b")
        self.assertEqual(self.active, {"b"})
        self.assertEqual((self.root / "bin/llmux-server-b").read_bytes(), b"binary")
        self.assertEqual((self.root / "llmux-b.toml").read_text(), "config")
        self.assertEqual(json.loads((self.root / "active.json").read_text())["slot"], "b")
        self.assertLess(self.calls.index(("start", "b")), self.calls.index(("stop", "a")))
        self.assertIn(("enable", "b"), self.calls)
        self.assertIn(("disable", "a"), self.calls)
        self.assertEqual(self.ops.deploy(self.binary, self.config), "a")
        self.assertEqual(self.active, {"a"})

    def test_first_install(self):
        self.active.clear()
        self.assertEqual(self.ops.deploy(self.binary, self.config), "a")
        self.assertEqual(self.active, {"a"})

    def test_failed_health_keeps_old(self):
        self.healthy = False
        with self.assertRaisesRegex(RuntimeError, "health"):
            self.ops.deploy(self.binary, self.config)
        self.assertEqual(self.active, {"a"})
        self.assertNotIn(("stop", "a"), self.calls)
        self.assertIn(("stop", "b"), self.calls)

    def test_failed_start_keeps_old(self):
        self.fail_start = True
        with self.assertRaisesRegex(RuntimeError, "start"):
            self.ops.deploy(self.binary, self.config)
        self.assertEqual(self.active, {"a"})
        self.assertIn(("stop", "b"), self.calls)

    def test_invalid_config_does_not_touch_units(self):
        self.fail_check = True
        with self.assertRaisesRegex(RuntimeError, "invalid"):
            self.ops.deploy(self.binary, self.config)
        self.assertFalse(any(call[0] in ("start", "stop") for call in self.calls))

    def test_guard_recovers_only_when_no_active_slot_and_unhealthy(self):
        (self.root / "active.json").write_text(json.dumps({"slot": "b", "listen": "127.0.0.1:8080"}))
        self.healthy = False
        self.ops.guard()
        self.assertNotIn(("start", "b"), self.calls)
        self.active.clear()
        self.healthy = True
        self.ops.guard()
        self.assertNotIn(("start", "b"), self.calls)
        self.healthy = False
        self.ops.guard()
        self.assertIn(("start", "b"), self.calls)

    def test_guard_without_record_is_noop(self):
        self.active.clear()
        self.ops.guard()
        self.assertFalse(any(call[0] == "start" for call in self.calls))

    def test_restarting_slot_is_busy(self):
        def run(command, **kwargs):
            state = "activating" if command[-1] == "llmux2@a.service" else "inactive"
            return subprocess.CompletedProcess(command, 3, state + "\n", "")
        with patch.object(ops.Path, "home", return_value=self.root), \
                patch.object(ops.subprocess, "run", side_effect=run), \
                patch.object(ops.sys, "argv", ["llmux2-deploy", "status"]):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(ops.main(), 0)
            self.assertEqual(json.loads(output.getvalue())["active"], ["a"])


if __name__ == "__main__":
    unittest.main()
