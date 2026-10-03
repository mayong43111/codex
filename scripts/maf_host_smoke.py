#!/usr/bin/env python3
"""Explicit live MAF host checks using an isolated Codex home and workspace."""

import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import selectors
import signal
import struct
import subprocess
import tempfile
import termios
import time
import uuid


def config_args(endpoint, deployment):
    settings = {
        "model": deployment,
        "model_reasoning_effort": "none",
        "model_provider": "maf_foundry",
        "model_providers.maf_foundry.name": "Azure MAF (experimental)",
        "model_providers.maf_foundry.wire_api": "responses",
        "model_providers.maf_foundry.base_url": endpoint.removesuffix("/responses"),
        "model_providers.maf_foundry.requires_openai_auth": False,
    }
    return [argument for key, value in settings.items() for argument in ("-c", f"{key}={json.dumps(value)}")]


def stop(process):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)


class RpcHost:
    def __init__(self, command, environment, workspace):
        self.errors = tempfile.TemporaryFile()
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=self.errors, env=environment, cwd=workspace,
                                        start_new_session=True)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.buffer = b""
        self.sequence = 0
        self.events = []

    def send(self, message):
        self.process.stdin.write(json.dumps(message).encode() + b"\n")
        self.process.stdin.flush()

    def receive(self, deadline):
        while b"\n" not in self.buffer:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not self.selector.select(remaining):
                raise AssertionError("Timed out waiting for app-server")
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                raise AssertionError(f"app-server exited: {self.process.poll()}")
            self.buffer += chunk
            if len(self.buffer) > 4 * 1024 * 1024:
                raise AssertionError("app-server frame exceeded test limit")
        line, self.buffer = self.buffer.split(b"\n", 1)
        return json.loads(line)

    def call(self, method, params):
        self.sequence += 1
        self.send({"jsonrpc": "2.0", "id": self.sequence, "method": method, "params": params})
        deadline = time.monotonic() + 90
        while True:
            message = self.receive(deadline)
            if message.get("id") == self.sequence:
                if "error" in message:
                    raise AssertionError(f"{method}: {message['error']}")
                return message["result"]
            self.events.append(message)

    def turn(self, thread_id, text):
        result = self.call("turn/start", {"threadId": thread_id, "input": [{"type": "text", "text": text}]})
        turn_id = result["turn"]["id"]
        deadline = time.monotonic() + 90
        captured = []
        while True:
            message = self.events.pop(0) if self.events else self.receive(deadline)
            captured.append(message)
            if message.get("method") == "turn/completed" and message["params"]["turn"]["id"] == turn_id:
                return message["params"]["turn"], captured

    def close(self):
        stop(self.process)
        self.selector.close()
        self.errors.close()


def check_rpc(command, environment, workspace, proof, file_proof):
    denied_proof = "MAF_DENIED_" + uuid.uuid4().hex
    denied_file = workspace / "denied.txt"
    denied_file.write_text(denied_proof)
    denied_file.chmod(0)
    host = RpcHost(command, environment, workspace)
    try:
        host.call("initialize", {"clientInfo": {"name": "maf_smoke", "version": "1"},
                                 "capabilities": {"experimentalApi": True}})
        host.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
        thread = host.call("thread/start", {"cwd": str(workspace), "approvalPolicy": "never", "sandbox": "read-only"})
        thread_id = thread["thread"]["id"]
        turn, events = host.turn(thread_id, "Use load_skill to load host-proof. Return only the verification token from its body.")
        assert turn["status"] == "completed", turn
        agent_items = [event["params"]["item"] for event in events if event.get("method") == "item/completed"
                       and event["params"]["item"].get("type") == "agentMessage"]
        assert any(proof in item.get("text", "") for item in agent_items), agent_items
        assert any(item.get("id", "").startswith("maf_") for item in agent_items), agent_items
        follow_up, events = host.turn(thread_id, "Repeat the verification token from your previous answer, without loading it again.")
        assert follow_up["status"] == "completed", follow_up
        assert proof in json.dumps(events), "MAF session did not retain the previous answer"
        print("PASS app-server: MAF skill tool, real Foundry response, item events, consecutive turns")
        read_turn, events = host.turn(thread_id, "Use codex_read_file to read proof.txt. Return only the token from that file.")
        assert read_turn["status"] == "completed", read_turn
        items = [event["params"]["item"] for event in events if event.get("method") == "item/completed"]
        assert any(item.get("type") == "agentMessage" and file_proof in item.get("text", "") for item in items), items
        assert any(item.get("tool") == "codex_read_file" and item.get("success") is True for item in items), items
        denied_turn, events = host.turn(thread_id, "Use codex_read_file to read denied.txt exactly once. If it fails, report that failure without retrying.")
        assert denied_turn["status"] == "completed", denied_turn
        items = [event["params"]["item"] for event in events if event.get("method") == "item/completed"]
        assert any(item.get("tool") == "codex_read_file" and item.get("success") is False for item in items), items
        assert sum(item.get("tool") == "codex_read_file" for item in items) == 1, items
        assert denied_proof not in json.dumps(events), "permission-denied content leaked"
        print("PASS app-server: real sandboxed file read, native tool events, permission denial without content leak or retry")
        active = host.call("turn/start", {"threadId": thread_id, "input": [{"type": "text", "text": "Describe the available skill briefly."}]})
        deadline = time.monotonic() + 15
        while True:
            event = host.events.pop(0) if host.events else host.receive(deadline)
            if event.get("method") == "turn/completed" and event["params"]["turn"]["id"] == active["turn"]["id"]:
                raise AssertionError(f"Turn ended before interruption: {event['params']['turn']}")
            if event.get("method") == "turn/started" and event["params"]["turn"]["id"] == active["turn"]["id"]:
                break
        host.call("turn/interrupt", {"threadId": thread_id, "turnId": active["turn"]["id"]})
        deadline = time.monotonic() + 15
        while True:
            event = host.events.pop(0) if host.events else host.receive(deadline)
            if event.get("method") == "turn/completed" and event["params"]["turn"]["id"] == active["turn"]["id"]:
                assert event["params"]["turn"]["status"] == "interrupted", event
                break
        after_interrupt, events = host.turn(thread_id, "This request must not be replayed.")
        assert after_interrupt["status"] == "failed", after_interrupt
        assert "MAF" in json.dumps(after_interrupt), after_interrupt
        print("PASS app-server: interruption invalidates the MAF session without replay")
    finally:
        host.close()

    host = RpcHost(command, {**environment, "CODEX_AGENT_BACKEND": "rust"}, workspace)
    try:
        host.call("initialize", {"clientInfo": {"name": "maf_resume_smoke", "version": "1"}})
        host.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
        host.call("thread/resume", {"threadId": thread_id})
        resumed, events = host.turn(thread_id, "This old MAF session must not use the Rust Agent.")
        assert resumed["status"] == "failed", resumed
        assert "MAF" in json.dumps(resumed), resumed
        print("PASS app-server: persisted MAF history cannot silently resume on Rust")
    finally:
        host.close()


def check_tui(command, environment, workspace, file_proof):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
    process = subprocess.Popen(command + ["--no-daemon", "--no-alt-screen", "Use codex_read_file to read proof.txt. Return only the token from that file."],
                               stdin=slave, stdout=slave, stderr=slave, cwd=workspace,
                               env={**environment, "TERM": "xterm-256color"}, start_new_session=True)
    os.close(slave)
    output = bytearray()
    selector = selectors.DefaultSelector()
    selector.register(master, selectors.EVENT_READ)
    try:
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            if not selector.select(max(0, deadline - time.monotonic())):
                break
            try:
                chunk = os.read(master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
            if not chunk:
                break
            output.extend(chunk)
            if b"\x1b[6n" in chunk:
                os.write(master, b"\x1b[1;1R")
            if b"\x1b[c" in chunk:
                os.write(master, b"\x1b[?1;2c")
            rollouts = list(Path(environment["CODEX_HOME"]).glob("sessions/**/*.jsonl"))
            for rollout in rollouts:
                for line in rollout.read_text().splitlines():
                    entry = json.loads(line)
                    payload = entry.get("payload", {})
                    if entry.get("type") == "response_item" and payload.get("role") == "assistant" \
                            and payload.get("id", "").startswith("maf_") \
                            and file_proof in json.dumps(payload):
                        assert "codex_read_file" in rollout.read_text(), "missing native tool history"
                        print("PASS TUI: real MAF file-tool call and Foundry answer persisted in original history")
                        return
        raise AssertionError("TUI did not record a MAF reply; terminal tail: " + output[-6000:].decode(errors="replace"))
    finally:
        stop(process)
        selector.close()
        os.close(master)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--tui", action="store_true")
    parser.add_argument("--codex", type=Path, default=Path(__file__).resolve().parents[1] / "codex-rs/target/debug/codex")
    args = parser.parse_args()
    environment = dict(os.environ, CODEX_AGENT_BACKEND="maf")
    for name in ("CODEX_MAF_BRIDGE_DLL", "CODEX_MAF_AZ", "CODEX_MAF_FOUNDRY_ENDPOINT", "CODEX_MAF_DEPLOYMENT"):
        if not environment.get(name):
            parser.error(f"{name} is required; this script performs live model inference")
    with tempfile.TemporaryDirectory(prefix="maf-host-smoke-") as temporary:
        root = Path(temporary)
        home = root / "home"
        workspace = root / "workspace"
        skill = home / "skills/host-proof"
        skill.mkdir(parents=True)
        workspace.mkdir()
        proof = "MAF_PROOF_" + uuid.uuid4().hex
        file_proof = "MAF_FILE_" + uuid.uuid4().hex
        (workspace / "proof.txt").write_text(file_proof + "\n")
        (skill / "SKILL.md").write_text("---\nname: host-proof\ndescription: Read the verification token.\n---\nVerification token: " + proof + "\n")
        (home / "config.toml").write_text(f"[projects.{json.dumps(str(workspace))}]\ntrust_level = \"trusted\"\n")
        environment["CODEX_HOME"] = str(home)
        command = [str(args.codex.resolve())] + config_args(environment["CODEX_MAF_FOUNDRY_ENDPOINT"], environment["CODEX_MAF_DEPLOYMENT"])
        if args.tui:
            check_tui(command, environment, workspace, file_proof)
        else:
            check_rpc(command + ["app-server"], environment, workspace, proof, file_proof)


if __name__ == "__main__":
    main()