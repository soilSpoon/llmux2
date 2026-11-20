"""Slot deployment and recovery for llmux2 (Python 3.9+, stdlib only)."""
from pathlib import Path
import argparse
import contextlib
import fcntl
import http.client
import ipaddress
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time


def atomic_write(path, writer, mode):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(prefix="." + path.name, dir=str(path.parent))
    try:
        with os.fdopen(fd, "wb") as output:
            writer(output)
            output.flush()
            os.fsync(output.fileno())
            os.fchmod(output.fileno(), mode)
        os.replace(name, path)
    finally:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(name)


def address_parts(listen):
    host, port = listen.rsplit(":", 1)
    host = host.strip("[]")
    address = ipaddress.ip_address(host)
    if address.is_unspecified:
        host = "127.0.0.1" if address.version == 4 else "::1"
    return host, int(port)


def health(listen):
    host, port = address_parts(listen)
    connection = http.client.HTTPConnection(host, port, timeout=2)
    try:
        connection.request("GET", "/healthz")
        response = connection.getresponse()
        return response.status == 200 and response.read(3) == b"ok"
    except (OSError, http.client.HTTPException):
        return False
    finally:
        connection.close()


class Deployment:
    def __init__(self, root, systemctl, check, health, sleep=lambda _: None):
        self.root = Path(root)
        self.systemctl = systemctl
        self.check = check
        self.health = health
        self.sleep = sleep

    def deploy(self, binary, config):
        binary, config = Path(binary).resolve(), Path(config).resolve()
        listen = self.check(binary, config)
        address_parts(listen)
        active = self.active_slots()
        if len(active) > 1:
            raise RuntimeError("both slots are active; inspect status and stop the unwanted slot first")
        old = active[0] if active else None
        idle = "b" if old == "a" else "a"
        target_binary = self.root / "bin" / ("llmux-server-" + idle)
        target_config = self.root / ("llmux-" + idle + ".toml")
        for source, target, mode in ((binary, target_binary, 0o700), (config, target_config, 0o600)):
            with source.open("rb") as incoming:
                atomic_write(target, lambda output: shutil.copyfileobj(incoming, output), mode)
        # Check the exact installed copies, not just the source files.
        if self.check(target_binary, target_config) != listen:
            raise RuntimeError("installed configuration changed its listen address")
        stopping_old = False
        try:
            self.systemctl("start", idle)
            pid = None
            for sample in range(7):
                if sample:
                    self.sleep(0.5)
                state = self.systemctl("show", idle)
                current_pid = state.get("MainPID", "")
                if (state.get("ActiveState") != "active"
                        or state.get("NRestarts") != "0"
                        or not current_pid.isdigit() or int(current_pid) == 0
                        or (pid is not None and current_pid != pid)):
                    raise RuntimeError("new slot did not remain active without restarts during settle window")
                pid = current_pid
            for _ in range(60):
                if self.health(listen):
                    break
                self.sleep(0.5)
            else:
                raise RuntimeError("new slot health check timed out")
            if old:
                stopping_old = True
                self.systemctl("stop", old)
            record = json.dumps({"slot": idle, "listen": listen}).encode()
            atomic_write(self.root / "active.json", lambda output: output.write(record), 0o600)
            self.systemctl("enable", idle)
            self.systemctl("disable", "b" if idle == "a" else "a")
            return idle
        except Exception as error:
            if not stopping_old:
                try:
                    self.systemctl("stop", idle)
                except Exception as cleanup:
                    raise RuntimeError("deployment failed: {}; new slot cleanup also failed: {}".format(error, cleanup)) from error
                raise RuntimeError("deployment failed; old slot unchanged: {}".format(error)) from error
            raise RuntimeError("handoff reached old-slot stop; new slot remains running; inspect status: {}".format(error)) from error

    def active_slots(self):
        return [slot for slot in ("a", "b") if self.systemctl("is-active", slot)]

    def record(self):
        try:
            record = json.loads((self.root / "active.json").read_text())
        except FileNotFoundError:
            return None
        if record.get("slot") not in ("a", "b"):
            raise RuntimeError("invalid recorded active slot")
        address_parts(record["listen"])
        return record

    def status(self):
        record = self.record()
        return {"active": self.active_slots(), "recorded": record,
                "healthy": self.health(record["listen"]) if record else False}

    def guard(self):
        if self.active_slots():
            return
        record = self.record()
        if record and not self.health(record["listen"]):
            self.systemctl("start", record["slot"])


def main(guard=False):
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    if not guard:
        commands = parser.add_subparsers(dest="command", required=True)
        deploy = commands.add_parser("deploy")
        deploy.add_argument("--binary", type=Path)
        deploy.add_argument("--config", type=Path)
        commands.add_parser("status")
    args = parser.parse_args()
    root = Path.home() / "llmux2"
    root.mkdir(mode=0o700, parents=True, exist_ok=True)

    def systemctl(action, slot):
        command = ["systemctl", "--user", action]
        if action == "show":
            command += ["-p", "ActiveState,NRestarts,MainPID"]
        command.append("llmux2@" + slot + ".service")
        result = subprocess.run(command,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                timeout=130)
        if action == "is-active" and result.returncode in (0, 3):
            state = result.stdout.strip()
            if state in ("inactive", "failed"):
                return False
            if state in ("active", "reloading", "activating", "deactivating", "refreshing"):
                return True
            raise RuntimeError("unexpected unit state: " + state)
        if result.returncode:
            raise RuntimeError("systemctl {} {} failed: {}".format(action, slot, result.stderr.strip()))
        if action == "show":
            return dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
        return True

    def check(binary, config):
        result = subprocess.run([str(binary), "--check", str(config)], cwd=str(root),
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=120)
        if result.returncode:
            raise RuntimeError("configuration validation failed: " + result.stderr.strip())
        return result.stdout.strip()

    try:
        with (root / "deploy.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            deployment = Deployment(root, systemctl, check, health, time.sleep)
            if guard:
                deployment.guard()
            elif args.command == "status":
                print(json.dumps(deployment.status(), indent=2))
            else:
                slot = deployment.deploy(args.binary or root / "bin/llmux-server",
                                         args.config or root / "llmux.toml")
                print("active slot: " + slot)
        return 0
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print("llmux2: " + str(error), file=sys.stderr)
        return 1
