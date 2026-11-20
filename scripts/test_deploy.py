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
        self.unit_states = []
        self.waits = []
        self.ops = ops.Deployment(self.root, self.systemctl, self.check,
                                  lambda _: self.healthy, self.waits.append)

    def systemctl(self, action, slot):
        self.calls.append((action, slot))
        if action == "is-active":
            return slot in self.active
        if action == "show":
            if self.unit_states:
                return self.unit_states.pop(0)
            return {"ActiveState": "active", "NRestarts": "0", "MainPID": "1234"}
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

    def test_old_health_does_not_mask_new_slot_failure(self):
        for state in (
                {},
                {"ActiveState": "failed", "NRestarts": "0", "MainPID": "0"},
                {"ActiveState": "active", "NRestarts": "1", "MainPID": "1234"},
                {"ActiveState": "active", "NRestarts": "0", "MainPID": "0"}):
            with self.subTest(state=state):
                self.active = {"a"}
                self.calls.clear()
                self.unit_states = [state]
                with self.assertRaisesRegex(RuntimeError, "new slot"):
                    self.ops.deploy(self.binary, self.config)
                self.assertEqual(self.active, {"a"})
                self.assertNotIn(("stop", "a"), self.calls)
                self.assertIn(("stop", "b"), self.calls)

    def test_new_slot_must_stay_stable_for_settle_window(self):
        stable = {"ActiveState": "active", "NRestarts": "0", "MainPID": "1234"}
        self.unit_states = [stable, stable, dict(stable, MainPID="5678")]
        with self.assertRaisesRegex(RuntimeError, "new slot"):
            self.ops.deploy(self.binary, self.config)
        self.assertEqual(self.active, {"a"})
        self.assertNotIn(("stop", "a"), self.calls)

    def test_healthy_slot_is_observed_for_three_seconds(self):
        self.ops.deploy(self.binary, self.config)
        self.assertGreaterEqual(sum(self.waits), 3)
        self.assertGreaterEqual(self.calls.count(("show", "b")), 2)

    def test_cli_checks_new_unit_properties(self):
        commands = []
        def run(command, **kwargs):
            commands.append(command)
            if "--check" in command:
                return subprocess.CompletedProcess(command, 0, "127.0.0.1:8080\n", "")
            if command[2] == "show":
                return subprocess.CompletedProcess(command, 0, "MainPID=4321\nNRestarts=0\nActiveState=active\n", "")
            if command[2] == "is-active":
                active = command[-1] == "llmux2@a.service"
                return subprocess.CompletedProcess(command, 0 if active else 3,
                                                   "active\n" if active else "inactive\n", "")
            return subprocess.CompletedProcess(command, 0, "", "")
        with patch.object(ops.Path, "home", return_value=self.root), \
                patch.object(ops.subprocess, "run", side_effect=run), \
                patch.object(ops, "health", return_value=True), \
                patch.object(ops.time, "sleep"), \
                patch.object(ops.sys, "argv", ["llmux2-deploy", "deploy", "--binary", str(self.binary), "--config", str(self.config)]), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(ops.main(), 0)
        probes = [command for command in commands if "show" in command]
        self.assertGreaterEqual(len(probes), 2)
        self.assertTrue(all(command == ["systemctl", "--user", "show", "-p",
                                       "ActiveState,NRestarts,MainPID", "llmux2@b.service"] for command in probes))

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
