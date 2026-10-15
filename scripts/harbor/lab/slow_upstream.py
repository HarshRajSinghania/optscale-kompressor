"""An upstream that behaves like a reasoning model: it emits response.created
immediately, then THINKS for --think seconds before the first output item.

The question this answers: does the proxy relay those early frames to the
client while it waits, or does it hold them? A client with an idle-stream
timeout (codex: 300s) cares about the first byte it sees, not the first byte
upstream sent.
"""
import argparse, json, sys, threading, time, uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGS = None
FIRST = {}


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("content-length") or 0))
        body = json.loads(raw or b"{}")
        rid = f"resp_{uuid.uuid4().hex[:16]}"
        item = {"type": "message", "id": f"msg_{uuid.uuid4().hex[:12]}",
                "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]}
        usage = {"input_tokens": 100, "output_tokens": 5, "total_tokens": 105,
                 "input_tokens_details": {"cached_tokens": 0},
                 "output_tokens_details": {"reasoning_tokens": 0}}
        response = {"id": rid, "object": "response", "status": "completed",
                    "model": body.get("model", "mock"), "output": [item], "usage": usage}
        created = dict(response, status="in_progress", output=[], usage=None)

        self.close_connection = True
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()

        def send(e):
            self.wfile.write(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n".encode())
            self.wfile.flush()

        t0 = time.time()
        send({"type": "response.created", "response": created, "sequence_number": 0})
        send({"type": "response.in_progress", "response": created, "sequence_number": 1})
        print(f"upstream: sent response.created at +0.0s, now thinking {ARGS.think}s",
              flush=True)
        time.sleep(ARGS.think)
        send({"type": "response.output_item.added", "output_index": 0,
              "item": dict(item, status="in_progress"), "sequence_number": 2})
        send({"type": "response.output_text.delta", "item_id": item["id"],
              "output_index": 0, "content_index": 0, "delta": "ok", "sequence_number": 3})
        send({"type": "response.output_item.done", "output_index": 0, "item": item,
              "sequence_number": 4})
        send({"type": "response.completed", "response": response, "sequence_number": 5})
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        print(f"upstream: finished at +{time.time()-t0:.1f}s", flush=True)


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=9807)
    p.add_argument("--think", type=float, default=20.0)
    ARGS = p.parse_args()
    print(f"slow upstream on 127.0.0.1:{ARGS.port}, think={ARGS.think}s", flush=True)
    ThreadingHTTPServer(("127.0.0.1", ARGS.port), H).serve_forever()
