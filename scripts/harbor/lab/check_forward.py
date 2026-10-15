"""Compare what the client sent with what the proxy forwarded upstream."""
import json, pathlib, sys

sent = json.loads(pathlib.Path(sys.argv[1]).read_text())
up = pathlib.Path(sys.argv[2])
reqs = sorted(p for p in up.glob("req-*.json") if not p.name.endswith(".headers.json"))
print(f"-- upstream saw {len(reqs)} request(s) --")
for i, r in enumerate(reqs, 1):
    got = json.loads(r.read_text())
    tools = [t.get("name") for t in got.get("tools", [])]
    print(f"  [{i}] {r.stat().st_size:>7} bytes (client sent {pathlib.Path(sys.argv[1]).stat().st_size})")
    print(f"      instructions byte-identical : {got.get('instructions') == sent['instructions']}")
    print(f"      reasoning item byte-identical: {got['input'][1] == sent['input'][1]}")
    out = got["input"][3].get("output")
    if isinstance(out, str):
        print(f"      tool output chars            : {len(sent['input'][3]['output'])} -> {len(out)}")
        marker = [ln for ln in out.splitlines() if "ref:k_" in ln or "omitted" in ln]
        print(f"      omission markers             : {len(marker)}")
        if marker:
            print(f"        e.g. {marker[0][:110]}")
    print(f"      tools                        : {tools}")
    print(f"      key delta vs client          : {set(got) ^ set(sent) or '{}'}")
    extra = [k for k in ("stream", "store", "include", "prompt_cache_key") if got.get(k) != sent.get(k)]
    print(f"      changed flags                : {extra or 'none'}")
