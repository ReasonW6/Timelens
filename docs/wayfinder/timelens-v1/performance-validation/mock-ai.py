"""Loopback-only synthetic OpenAI adapter for reproducible Timelens UI acceptance."""
import argparse
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import time

parser = argparse.ArgumentParser()
parser.add_argument("--state-dir", required=True, type=Path)
parser.add_argument("--stream-seconds", type=float, default=0.9)
args = parser.parse_args()
if not 0.3 <= args.stream_seconds <= 180:
    parser.error("stream-seconds must be between 0.3 and 180")
args.state_dir.mkdir(parents=True, exist_ok=True)

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        payload = json.dumps({"data": [{"id": "fixture-v1"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        size = int(self.headers.get("Content-Length", "0"))
        if size > 32 * 1024 * 1024:
            self.send_error(413)
            return
        request = json.loads(self.rfile.read(size))
        # Never log credentials or request bodies, even in local acceptance.
        with (args.state_dir / "requests.jsonl").open("a", encoding="utf-8") as out:
            out.write(json.dumps({"at": time.time(), "messages": len(request.get("messages", [])),
                                  "model": request.get("model"), "store": request.get("store")}) + "\n")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        chunks = ["这是本机合成数据的验收回答。\n", "WritingStudio、CodeEditor 与 ReadingDesk 的重叠时长分别展示。\n",
                  "本回答不代表真实用户行为，也未调用外部服务。"]
        try:
            repeats = max(1, round(args.stream_seconds / 0.9))
            for text in chunks * repeats:
                payload = {"choices": [{"delta": {"content": text}}]}
                self.wfile.write(("data: " + json.dumps(payload, ensure_ascii=False) + "\n\n").encode())
                self.wfile.flush()
                time.sleep(0.3)
            usage = {"choices": [], "usage": {"prompt_tokens": 120, "completion_tokens": 60}}
            self.wfile.write(("data: " + json.dumps(usage) + "\n\ndata: [DONE]\n\n").encode())
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

server = HTTPServer(("127.0.0.1", 0), Handler)
(args.state_dir / "endpoint.txt").write_text(f"http://127.0.0.1:{server.server_port}/v1", encoding="utf-8")
server.serve_forever()
