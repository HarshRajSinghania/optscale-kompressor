"""Build a codex-shaped /v1/responses body for offline validation.

Mirrors what `codex` actually sends on a gpt-5.5 Responses turn: top-level
`instructions`, an `input` list carrying a user message, a reasoning item with
an opaque `encrypted_content` blob, a function_call and its function_call_output
holding a large repetitive build log, plus codex's own tools and the flags it
sets (store=false, include=[reasoning.encrypted_content], prompt_cache_key).
"""
import json
import sys

INSTRUCTIONS = (
    "You are a coding agent running in the Codex CLI. Be precise. "
    "Never guess at file contents; read them. " * 8
)

LOG_LINES = []
for i in range(900):
    LOG_LINES.append(
        f"2026-09-18 10:{i // 60:02d}:{i % 60:02d} INFO  [worker-{i % 4}] "
        f"compiling crate unit {i} in {i % 37}ms (features=default,std,alloc)"
    )
for i in range(40):
    LOG_LINES.append(f"warning: unused variable `tmp{i}`")
    LOG_LINES.append(f"  --> src/gen/mod_{i}.rs:{10 + i}:9")
    LOG_LINES.append(f"   |\n{10 + i:>4} |     let tmp{i} = compute();\n   |         ^^^^ help: prefix it with an underscore")
LOG = "\n".join(LOG_LINES)

# a realistic opaque reasoning blob: base64-ish, and deliberately containing a
# run of characters that a naive secret-redactor would mangle
BLOB = "gAAAAABn0pQ" + ("sk-" + "A1b2C3d4E5f6G7h8" * 3) + "Zq" * 400

body = {
    "model": "gpt-5.5",
    "instructions": INSTRUCTIONS,
    "input": [
        {"type": "message", "role": "user",
         "content": [{"type": "input_text", "text": "Why is the build slow? Fix it."}]},
        {"type": "reasoning", "id": "rs_68c1", "summary": [], "encrypted_content": BLOB},
        {"type": "function_call", "call_id": "call_a1", "name": "shell",
         "arguments": json.dumps({"command": ["bash", "-lc", "cargo build 2>&1 | tail -n 2000"],
                                  "workdir": "/app"})},
        {"type": "function_call_output", "call_id": "call_a1", "output": LOG},
    ],
    "tools": [
        {"type": "function", "name": "shell",
         "description": "Run a shell command and return its output.",
         "parameters": {"type": "object",
                        "properties": {"command": {"type": "array", "items": {"type": "string"}},
                                       "workdir": {"type": "string"}},
                        "required": ["command"]}},
    ],
    "tool_choice": "auto",
    "parallel_tool_calls": False,
    "store": False,
    "stream": True,
    "include": ["reasoning.encrypted_content"],
    "prompt_cache_key": "codex-session-abc123",
    "reasoning": {"effort": "medium", "summary": "auto"},
    "text": {"verbosity": "medium"},
}

if "--nostream" in sys.argv:
    body["stream"] = False
sys.stdout.write(json.dumps(body))
