#!/usr/bin/env python3
"""Real Codex tool-loop checks using only temporary homes and localhost fixtures."""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def check(helper, codex, scenario):
    token = "fixture-routing-state-" * 100
    calls, failures = [], []

    class Upstream(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            try:
                data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                assert self.path == "/v1/responses", "unexpected endpoint"
                assert self.headers["Authorization"] == "Bearer fixture-key", "incorrect source key"
                index = len(calls)
                calls.append(index)
                if index == 0:
                    tools = {t.get("name") for t in data.get("tools", [])}
                    if "exec_command" in tools:
                        name, args = "exec_command", {"cmd": "printf LOCAL_TOOL_OK", "max_output_tokens": 20}
                    else:
                        assert "shell_command" in tools, "local shell tool missing"
                        name, args = "shell_command", {"command": "printf LOCAL_TOOL_OK"}
                    items = [{"id": "fc-fixture", "type": "function_call", "call_id": "call-fixture",
                              "name": name, "arguments": json.dumps(args)}]
                    if scenario == "oversized-reasoning":
                        items.insert(0, {"id": "rs-fixture", "type": "reasoning", "summary": [],
                                         "encrypted_content": "fixture-opaque-reasoning-" * 48000})
                else:
                    assert index == 1, "tool continuation was replayed"
                    results = [i for i in data["input"] if i.get("type") == "function_call_output"]
                    assert len(results) == 1 and results[0].get("call_id") == "call-fixture", "tool result missing or duplicated"
                    assert "LOCAL_TOOL_OK" in str(results[0].get("output", "")), "local tool did not complete"
                    if scenario == "long-turn-header":
                        assert self.headers.get("x-codex-turn-state") == token, "routing token was changed"
                    else:
                        assert self.headers.get("thread-id"), "Codex thread-id missing"
                        assert any(i.get("encrypted_content") for i in data["input"]), "opaque reasoning missing"
                    items = [{"id": "msg-fixture", "type": "message", "role": "assistant", "status": "completed",
                              "content": [{"type": "output_text", "text": "LOCAL_CONTINUATION_OK", "annotations": []}]}]
                response = {"id": f"resp-fixture-{index}", "object": "response", "status": "completed",
                            "model": data["model"], "output": items,
                            "usage": {"input_tokens": 100, "output_tokens": 20, "total_tokens": 120,
                                      "input_tokens_details": {"cached_tokens": 40, "cache_write_tokens": 5},
                                      "output_tokens_details": {"reasoning_tokens": 8}}}
                events = [{"type": "response.created", "response": {"id": response["id"], "output": [], "status": "in_progress"}}]
                events.extend({"type": "response.output_item.done", "output_index": i, "item": item}
                              for i, item in enumerate(items))
                events.append({"type": "response.completed", "response": response})
                payload = "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                if scenario == "long-turn-header":
                    self.send_header("x-codex-turn-state", token)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
            except Exception as error:
                # Deliberately exclude request bodies, credentials and headers.
                failures.append(str(error) if isinstance(error, AssertionError) else type(error).__name__)
                self.send_error(500, "fixture failed")

    server = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="cx-continuation-", dir="/tmp") as temp:
            root = Path(temp)
            home, state, work = [root / name for name in ("codex", "app", "work")]
            for directory in (home, state, work):
                directory.mkdir()
            with socket.socket() as free:
                free.bind(("127.0.0.1", 0))
                port = free.getsockname()[1]
            process = subprocess.Popen([str(helper), "--state-dir", str(state), "--port", str(port)],
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                for _ in range(100):
                    if (state / "control.sock").exists():
                        break
                    assert process.poll() is None, "temporary helper exited"
                    time.sleep(.05)

                def ipc(op, payload):
                    with socket.socket(socket.AF_UNIX) as sock:
                        sock.settimeout(10)
                        sock.connect(str(state / "control.sock"))
                        sock.sendall((json.dumps({"id": "fixture", "op": op, "payload": payload}) + "\n").encode())
                        result = json.loads(sock.makefile("rb").readline())
                        assert result["ok"], "fixture IPC failed"
                        return result["data"]

                source = {"id": "fixture", "name": "Local fixture", "kind": "api",
                          "base_url": f"http://127.0.0.1:{server.server_port}/v1", "enabled": True, "priority": 0,
                          "proxy": {"mode": "direct"}, "models": [], "secret": {"api_key": "fixture-key"}}
                ipc("sync", {"sources": [source], "mode": "auto"})
                ipc("config.attach", {"codex_home": str(home)})
                env = {k: v for k, v in os.environ.items() if not k.startswith(("CODEX_", "OPENAI_", "CHATGPT_"))}
                env["CODEX_HOME"] = str(home)
                run = subprocess.run([codex, "--strict-config", "-c", "model_providers.codexpress.stream_max_retries=0",
                                      "-c", "model_providers.codexpress.request_max_retries=0", "exec",
                                      "--skip-git-repo-check", "--ephemeral", "--ignore-rules", "--sandbox", "read-only",
                                      "--json", "-C", str(work), "-m", "fixture-model",
                                      "Perform the requested local tool once, then finish."],
                                     env=env, text=True, capture_output=True, timeout=45)
                passed = run.returncode == 0 and "LOCAL_CONTINUATION_OK" in run.stdout and len(calls) == 2 and not failures
                result = {"scenario": scenario, "passed": passed, "upstream_requests": len(calls),
                          "conflict_409": "409 Conflict" in run.stderr + run.stdout, "fixture_failures": failures}
                if not passed:
                    print(json.dumps(result), flush=True)
                assert passed, f"local continuation check failed: {scenario}"
                usage = ipc("usage", {"days": 1, "source_id": "fixture"})
                assert usage["inflight"] == 0 and len(usage["recent"]) == 2, "usage attempt lifecycle incorrect"
                if scenario == "long-turn-header":
                    assert usage["rows"][0]["input_tokens"] == 200 and usage["rows"][0]["output_tokens"] == 40, "usage counted incorrectly"
                reported = [row for row in usage["recent"] if row["complete"]]
                assert len(reported) == (2 if scenario == "long-turn-header" else 1), "bounded observation not respected"
                if scenario == "oversized-reasoning":
                    assert any(row["token_details"]["input_estimated"] for row in usage["recent"]), "oversized usage must remain estimated"
                for row in usage["recent"]:
                    if not row["complete"]:
                        continue
                    assert row["token_details"]["reasoning_output_tokens"] == 8, "reasoning usage missing"
                    assert row["token_details"]["cache_write_input_tokens"] == 5, "cache write usage missing"
                    assert row["token_details"]["input_estimated"] is False, "reported usage replaced by estimate"
                result["usage_verified"] = True
                print(json.dumps(result), flush=True)
                ipc("config.restore", {"codex_home": str(home)})
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--helper", type=Path, default=Path(__file__).resolve().parents[1] / "dist/Codexpress.app/Contents/MacOS/codexpress-gateway")
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--scenario", choices=["long-turn-header", "oversized-reasoning"])
    args = parser.parse_args()
    for scenario in [args.scenario] if args.scenario else ["long-turn-header", "oversized-reasoning"]:
        check(args.helper.resolve(), args.codex, scenario)
