#!/usr/bin/env python3
"""Generate real-model compaction summaries via the NextAPI gateway.

Calls each model with the exact system prompt used by
TurnEngine::compaction_summary and a realistic discarded-context
transcript (Chinese and English), then writes the raw model output to
fixtures/compaction/<model>-<lang>.txt for validator regression tests.

Requires NEXTAPI_TOKEN in the environment. Never prints the token.
"""

import json
import os
import sys
import time
import urllib.request

BASE = "https://api.nextapi.store/v1/chat/completions"
MODELS = ["kimi-k3", "glm-5.2", "minimax-m3"]
SYSTEM = (
    "Summarize the prior agent context into concise structured points. "
    "Use exactly these sections: Goal; Completed actions and results; "
    "Key discoveries and file paths; Unfinished next steps. "
    "Do not invent facts, emit tool calls, or include reasoning tags."
)

TRANSCRIPT_EN = [
    {"role": "user", "content": "Fix the rounding bug in the pricing module and add regression tests."},
    {"role": "assistant", "content": "I'll start by reading the pricing module.", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"src/pricing.py\"}"}}]},
    {"role": "tool", "tool_call_id": "call_1", "content": "def price(amount):\n    return round(amount * 1.08)\n"},
    {"role": "assistant", "content": "The bug is bankers rounding on negative amounts. I'll patch price() to use decimal quantize.", "tool_calls": [{"id": "call_2", "type": "function", "function": {"name": "edit_file", "arguments": "{\"path\":\"src/pricing.py\",\"old\":\"return round(amount * 1.08)\",\"new\":\"return float(Decimal(amount * 1.08).quantize(Decimal('0.01'), rounding=ROUND_HALF_UP))\"}"}}]},
    {"role": "tool", "tool_call_id": "call_2", "content": "edit applied"},
    {"role": "assistant", "content": "Edit applied. Running the test suite next.", "tool_calls": [{"id": "call_3", "type": "function", "function": {"name": "run_command", "arguments": "{\"command\":\"pytest tests/test_pricing.py\"}"}}]},
    {"role": "tool", "tool_call_id": "call_3", "content": "3 passed, 1 failed: test_negative_amount expects -1.08 got -1.07"},
]

TRANSCRIPT_ZH = [
    {"role": "user", "content": "修复定价模块的舍入 bug，并补充回归测试。"},
    {"role": "assistant", "content": "我先读取定价模块。", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"src/pricing.py\"}"}}]},
    {"role": "tool", "tool_call_id": "call_1", "content": "def price(amount):\n    return round(amount * 1.08)\n"},
    {"role": "assistant", "content": "问题是负数金额下的银行家舍入。我会把 price() 改成 Decimal quantize。", "tool_calls": [{"id": "call_2", "type": "function", "function": {"name": "edit_file", "arguments": "{\"path\":\"src/pricing.py\",\"old\":\"return round(amount * 1.08)\",\"new\":\"return float(Decimal(amount * 1.08).quantize(Decimal('0.01'), rounding=ROUND_HALF_UP))\"}"}}]},
    {"role": "tool", "tool_call_id": "call_2", "content": "修改已应用"},
    {"role": "assistant", "content": "修改完成，接下来运行测试。", "tool_calls": [{"id": "call_3", "type": "function", "function": {"name": "run_command", "arguments": "{\"command\":\"pytest tests/test_pricing.py\"}"}}]},
    {"role": "tool", "tool_call_id": "call_3", "content": "3 通过，1 失败：test_negative_amount 期望 -1.08 实际 -1.07"},
]


def encode_context(messages):
    # Mirror TurnEngine::compaction_summary: JSON-encode each discarded message per line.
    return "\n".join(json.dumps(m, ensure_ascii=False) for m in messages)


def call(model, context):
    body = json.dumps({
        "model": model,
        "messages": [
            {"role": "system", "content": SYSTEM},
            {"role": "user", "content": context},
        ],
        "max_tokens": 8192,
        "temperature": 0.2,
    }).encode()
    req = urllib.request.Request(
        BASE,
        data=body,
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {os.environ['NEXTAPI_TOKEN']}",
        },
    )
    start = time.time()
    with urllib.request.urlopen(req, timeout=120) as resp:
        data = json.load(resp)
    elapsed = time.time() - start
    text = data["choices"][0]["message"]["content"]
    return text, elapsed


def main():
    out_dir = os.path.join(os.path.dirname(__file__), "..", "fixtures", "compaction")
    os.makedirs(out_dir, exist_ok=True)
    failures = 0
    for model in MODELS:
        for lang, transcript in (("en", TRANSCRIPT_EN), ("zh", TRANSCRIPT_ZH)):
            try:
                text, elapsed = call(model, encode_context(transcript))
            except Exception as error:  # noqa: BLE001
                print(f"{model} {lang}: FAILED {error}")
                failures += 1
                continue
            path = os.path.join(out_dir, f"{model}-{lang}.txt")
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(text)
            print(f"{model} {lang}: {elapsed:.1f}s {len(text)} chars -> {path}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
