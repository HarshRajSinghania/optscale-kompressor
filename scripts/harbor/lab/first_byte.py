"""Time to first byte AS THE CLIENT SEES IT, over a raw socket.

A raw socket is used deliberately: any HTTP client library may buffer, and a
buffered read would make the proxy look like it is holding frames it is
actually relaying (and vice versa).
"""
import json, socket, sys, time
from urllib.parse import urlparse

url, payload = sys.argv[1], sys.argv[2]
u = urlparse(url)
body = json.loads(open(payload).read())
body["stream"] = True
raw = json.dumps(body).encode()
req = (f"POST {u.path} HTTP/1.1\r\nHost: {u.hostname}:{u.port}\r\n"
       f"content-type: application/json\r\naccept: text/event-stream\r\n"
       f"authorization: Bearer sk-test-000000000000\r\n"
       f"content-length: {len(raw)}\r\nconnection: close\r\n\r\n").encode() + raw

t0 = time.time()
s = socket.create_connection((u.hostname, u.port), timeout=600)
s.sendall(req)
first_hdr = first_evt = None
buf = b""
seen_headers = False
while True:
    chunk = s.recv(65536)
    if not chunk:
        break
    now = time.time() - t0
    if first_hdr is None:
        first_hdr = now
    buf += chunk
    if not seen_headers and b"\r\n\r\n" in buf:
        seen_headers = True
        head, rest = buf.split(b"\r\n\r\n", 1)
        status = head.split(b"\r\n")[0].decode()
        kmode = [l for l in head.split(b"\r\n") if l.lower().startswith(b"x-kompressor")]
        print(f"  response head at +{first_hdr:6.1f}s  {status}  "
              + " ".join(l.decode() for l in kmode))
        if rest.strip() and first_evt is None:
            first_evt = now
            print(f"  FIRST BODY BYTES at +{first_evt:6.1f}s  {rest[:70]!r}")
        continue
    if seen_headers and first_evt is None and chunk.strip():
        first_evt = now
        print(f"  FIRST BODY BYTES at +{first_evt:6.1f}s  {chunk[:70]!r}")
print(f"  stream closed at +{time.time()-t0:6.1f}s  ({len(buf)} bytes total)")
