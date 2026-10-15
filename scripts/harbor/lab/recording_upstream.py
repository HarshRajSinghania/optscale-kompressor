"""Recording OpenAI-Responses upstream for offline validation.

Writes every request body it receives to <dump>/req-NNN.json, so the exact
bytes kompressor forwarded can be diffed against the exact bytes the client
sent. Answers /v1/responses as SSE (what codex asks for) or JSON.

Optionally answers the FIRST request with a tool call to `optscale_kompressor_retrieve`
(--retrieve k_HANDLE), which is how the server-side retrieve loop is exercised
without a model.
"""
import argparse
import json
import pathlib
import re
import sys
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGS = None
DUMP = None
COUNT = threading.Lock()
N = [0]


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _record(self, raw):
        with COUNT:
            N[0] += 1
            n = N[0]
        (DUMP / f"req-{n:03d}.json").write_bytes(raw)
        (DUMP / f"req-{n:03d}.headers.json").write_text(
            json.dumps({k.lower(): v for k, v in self.headers.items()}, indent=2))
        return n

    def _sse(self, events):
        # no content-length and no chunking: the connection close is what
        # delimits the body, so it must actually close
        self.close_connection = True
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        for e in events:
            self.wfile.write(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n".encode())
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def _json(self, payload):
        out = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("content-length") or 0))
        n = self._record(raw)
        body = json.loads(raw or b"{}")
        rid = f"resp_{uuid.uuid4().hex[:16]}"
        want_tool = bool(ARGS.retrieve) and n == 1
        handle = ARGS.retrieve
        if want_tool and handle == "auto":
            found = re.findall(rb"k_[A-Za-z0-9_-]{12}", raw)
            if not found:
                want_tool = False
            else:
                handle = found[0].decode()

        if want_tool:
            item = {"type": "function_call", "id": f"fc_{uuid.uuid4().hex[:12]}",
                    "call_id": f"call_{uuid.uuid4().hex[:12]}",
                    "name": "optscale_kompressor_retrieve",
                    "arguments": json.dumps({"ref": handle}),
                    "status": "completed"}
            text = None
        else:
            item = {"type": "message", "id": f"msg_{uuid.uuid4().hex[:12]}",
                    "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "ok", "annotations": []}]}
            text = "ok"

        usage = {"input_tokens": max(1, len(raw) // 4), "output_tokens": 5,
                 "total_tokens": max(1, len(raw) // 4) + 5,
                 "input_tokens_details": {"cached_tokens": 0},
                 "output_tokens_details": {"reasoning_tokens": 0}}
        response = {"id": rid, "object": "response", "status": "completed",
                    "model": body.get("model", "mock"), "output": [item], "usage": usage}

        if not body.get("stream"):
            return self._json(response)

        created = dict(response, status="in_progress", output=[], usage=None)
        events = [{"type": "response.created", "response": created, "sequence_number": 0},
                  {"type": "response.in_progress", "response": created, "sequence_number": 1},
                  {"type": "response.output_item.added", "output_index": 0,
                   "item": dict(item, status="in_progress"), "sequence_number": 2}]
        if want_tool:
            events += [
                {"type": "response.function_call_arguments.delta", "item_id": item["id"],
                 "output_index": 0, "delta": item["arguments"], "sequence_number": 3},
                {"type": "response.function_call_arguments.done", "item_id": item["id"],
                 "output_index": 0, "arguments": item["arguments"], "sequence_number": 4}]
        else:
            events += [
                {"type": "response.content_part.added", "item_id": item["id"], "output_index": 0,
                 "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []},
                 "sequence_number": 3},
                {"type": "response.output_text.delta", "item_id": item["id"], "output_index": 0,
                 "content_index": 0, "delta": text, "sequence_number": 4},
                {"type": "response.output_text.done", "item_id": item["id"], "output_index": 0,
                 "content_index": 0, "text": text, "sequence_number": 5},
                {"type": "response.content_part.done", "item_id": item["id"], "output_index": 0,
                 "content_index": 0, "part": item["content"][0], "sequence_number": 6}]
        events += [{"type": "response.output_item.done", "output_index": 0, "item": item,
                    "sequence_number": 90},
                   {"type": "response.completed", "response": response, "sequence_number": 91}]
        self._sse(events)


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=9801)
    p.add_argument("--dump", required=True)
    p.add_argument("--retrieve", default=None,
                   help="answer request #1 with a optscale_kompressor_retrieve call for this handle")
    ARGS = p.parse_args()
    DUMP = pathlib.Path(ARGS.dump)
    DUMP.mkdir(parents=True, exist_ok=True)
    print(f"recording upstream on 127.0.0.1:{ARGS.port} -> {DUMP}", flush=True)
    ThreadingHTTPServer(("127.0.0.1", ARGS.port), H).serve_forever()
