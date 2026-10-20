#!/usr/bin/env python3
"""Resume opaque legacy Codex history through a confirmed origin; localhost only."""
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


def check(helper, codex):
    calls, failures = [], []
    cipher = "fixture-original-account-encrypted-context"

    class Upstream(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            try:
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                assert self.path == "/v1/responses", "wrong endpoint"
                assert self.headers.get("Authorization") == "Bearer fixture-original-key", "wrong origin identity"
                if calls:
                    assert any(item.get("encrypted_content") == cipher for item in body.get("input", [])), "old opaque history missing"
                calls.append(len(calls))
                response = {"id": f"resp-{len(calls)}", "status": "completed", "model": body["model"], "output": [
                    {"type": "reasoning", "id": f"reason-{len(calls)}", "summary": [], "encrypted_content": cipher},
                    {"type": "message", "id": f"message-{len(calls)}", "role": "assistant", "status": "completed", "content": [
                        {"type": "output_text", "text": "LOCAL_RESUME_OK", "annotations": []}]}],
                    "usage": {"input_tokens": 100, "output_tokens": 20, "total_tokens": 120,
                              "input_tokens_details": {"cached_tokens": 40}, "output_tokens_details": {"reasoning_tokens": 5}}}
                events = [{"type": "response.created", "response": {"id": response["id"], "output": [], "status": "in_progress"}}]
                events += [{"type": "response.output_item.done", "output_index": i, "item": item} for i, item in enumerate(response["output"])]
                events.append({"type": "response.completed", "response": response})
                payload = "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
            except Exception as error:
                failures.append(str(error) if isinstance(error, AssertionError) else type(error).__name__)
                self.send_error(500, "fixture failure")

    server = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    process = None
    try:
        with tempfile.TemporaryDirectory(prefix="cx-resume-", dir="/tmp", ignore_cleanup_errors=True) as directory:
            root = Path(directory)
            home, state, work = [root / name for name in ["codex", "app", "work"]]
            for path in [home, state, work]:
                path.mkdir()
            base = f"http://127.0.0.1:{server.server_port}/v1"
            (home / "config.toml").write_text(f'model_provider="legacy"\n[model_providers.legacy]\nname="Original"\nbase_url="{base}"\nwire_api="responses"\nrequires_openai_auth=false\nexperimental_bearer_token="fixture-original-key"\nsupports_websockets=false\n')
            env = {k: v for k, v in os.environ.items() if not k.startswith(("CODEX_", "OPENAI_", "CHATGPT_"))}
            env["CODEX_HOME"] = str(home)

            def run(resume=None):
                args = [codex, "--strict-config", "-c", 'model_provider="legacy"', "exec"]
                args += ["resume", resume] if resume else ["-C", str(work), "--sandbox", "read-only"]
                args += ["--skip-git-repo-check", "--ignore-rules", "--json", "-m", "fixture-model", "Continue the local fixture."]
                result = subprocess.run(args, env=env, cwd=work, text=True, capture_output=True, timeout=40)
                assert result.returncode == 0 and "LOCAL_RESUME_OK" in result.stdout, "local Codex resume failed"
                return [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]

            events = run()
            thread = next(event["thread_id"] for event in events if event.get("type") == "thread.started")
            with socket.socket() as free:
                free.bind(("127.0.0.1", 0)); port = free.getsockname()[1]

            def start():
                process = subprocess.Popen([str(helper), "--state-dir", str(state), "--port", str(port)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                for _ in range(100):
                    if (state / "control.sock").exists():
                        try:
                            with socket.socket(socket.AF_UNIX) as probe:
                                probe.connect(str(state / "control.sock"))
                            return process
                        except (ConnectionRefusedError, FileNotFoundError):
                            pass
                    assert process.poll() is None, "helper exited"
                    time.sleep(.05)
                raise AssertionError("helper socket unavailable")

            def ipc(op, payload):
                with socket.socket(socket.AF_UNIX) as sock:
                    sock.settimeout(12); sock.connect(str(state / "control.sock"))
                    sock.sendall((json.dumps({"id": "fixture", "op": op, "payload": payload}) + "\n").encode())
                    result = json.loads(sock.makefile("rb").readline())
                    assert result["ok"], "fixture management failed"
                    return result["data"]

            sources = [{"id": "original", "name": "Original", "kind": "api", "base_url": base, "enabled": True, "priority": 1,
                        "proxy": {"mode": "direct"}, "models": [], "secret": {"api_key": "fixture-original-key"}},
                       {"id": "higher", "name": "Higher priority", "kind": "api", "base_url": "http://127.0.0.1:1/v1", "enabled": True, "priority": 0,
                        "proxy": {"mode": "direct"}, "models": [], "secret": {"api_key": "different-fixture-key"}}]
            process = start()
            ipc("sync", {"sources": sources, "mode": "auto"})
            identity = next(row["identity_digest"] for row in ipc("status", {})["sources"] if row["id"] == "original")
            ipc("config.attach", {"codex_home": str(home), "provider_origins": [{"provider_id": "legacy", "source_id": "original", "source_identity": identity, "confirmed": True}]})
            run(thread)
            process.terminate(); process.wait(timeout=5)
            process = start()
            ipc("sync", {"sources": sources, "mode": "auto"})
            run(thread)
            assert len(calls) == 3 and not failures, "opaque resume lost or replayed"
            usage = ipc("usage", {"days": 1})
            assert len(usage["recent"]) == 2 and all(row["source_id"] == "original" and row["complete"] for row in usage["recent"]), "origin or detailed usage missing"
            ipc("config.restore", {"codex_home": str(home)})
            print(json.dumps({"passed": True, "original_direct_requests": 1, "gateway_resumed_requests": 2, "helper_restart_verified": True, "opaque_history_preserved": True, "detailed_usage_verified": True}))
    finally:
        if process is not None and process.poll() is None:
            process.terminate(); process.wait(timeout=5)
        server.shutdown(); server.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--helper", type=Path, default=Path(__file__).resolve().parents[1] / "dist/Codexpress.app/Contents/MacOS/codexpress-gateway")
    parser.add_argument("--codex", default="codex")
    args = parser.parse_args()
    check(args.helper.resolve(), args.codex)
