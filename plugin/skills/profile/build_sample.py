#!/usr/bin/env python3
"""Shard the `wend messages` dump so EVERY message gets read (not sampled).

Reads   /tmp/wend-profile.json  (output of `wend messages --role user --json`)
Writes  /tmp/prof/shard_00.txt, shard_01.txt, ...  (~400KB each, session order)
Prints  the shard count (pass it to the mining workflow as args.shards).

`wend messages` already excludes tool results, system reminders, slash commands,
bracketed system markers, and subagent turns. This adds one cut — skill/plan
boilerplate the user did not type — then keeps EVERY remaining message (each
truncated to a gist so huge pastes don't blow an agent's context) in
conversation order, split into shards small enough to read in full. Stdlib only,
language-agnostic.
"""
import json
import os
import re

SRC = "/tmp/wend-profile.json"
OUTDIR = "/tmp/prof"
SHARD_BYTES = 400_000    # each shard is small enough for one agent to read fully
KEEP_FULL = 2000         # keep whole: covers ~99.9% of what the user actually writes
HEAD, TAIL = 1000, 500   # only beyond KEEP_FULL (near-certainly a paste) → head+tail

# Measured on real history: median message is ~113 chars; genuine long-form
# writing is rare (~0.6% of messages) and almost always under 2000 chars. Nearly
# all >2000-char messages are pasted logs/DOM/skill output, so cropping their
# middle loses no authored content. Keep everything up to KEEP_FULL whole.


def crop(t: str) -> str:
    """Whole message up to KEEP_FULL; head + tail only for longer pastes."""
    t = re.sub(r"\s+", " ", t).strip()
    if len(t) <= KEEP_FULL:
        return t
    return f"{t[:HEAD]} …[{len(t) - HEAD - TAIL} chars cut]… {t[-TAIL:]}"

# skill/plan-injected boilerplate (tool-generated, not the user's words)
NOISE = re.compile(
    r"^(base directory for this skill|implement the following|<!--|"
    r"# commit skill|## preamble|_upd=|source <)",
    re.I,
)


def main() -> None:
    data = json.load(open(SRC))
    os.makedirs(OUTDIR, exist_ok=True)
    for old in os.listdir(OUTDIR):
        if old.startswith("shard_"):
            os.remove(os.path.join(OUTDIR, old))

    shard, buf, size, kept, cur_sess = 0, [], 0, 0, None

    def flush():
        nonlocal shard, buf, size
        if not buf:
            return
        with open(os.path.join(OUTDIR, f"shard_{shard:02d}.txt"), "w") as f:
            f.write(f"===== SHARD {shard} (conversation order) =====\n")
            f.writelines(buf)
        shard += 1
        buf, size = [], 0

    for m in data:  # already ordered by session, then line
        t = m["text"].strip()
        if not t or NOISE.search(t):
            continue
        if m["session_id"] != cur_sess:  # mark session boundaries for flow-reading
            cur_sess = m["session_id"]
            line = f"\n--- session [{m['project']}] ---\n"
            buf.append(line); size += len(line)
        line = f"• {crop(t)}\n"
        buf.append(line); size += len(line); kept += 1
        if size >= SHARD_BYTES:
            flush()
    flush()

    n = shard
    print(f"shards={n}  messages={kept}  (read every one; pass args.shards={n})")


if __name__ == "__main__":
    main()
