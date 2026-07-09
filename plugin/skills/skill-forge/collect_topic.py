#!/usr/bin/env python3
"""Collect ALL of the user's messages from TOPIC-related sessions into one file.

Discovery is done by `wend search` (proper FTS ranking), which writes the related
session ids to /tmp/topic/sessions.txt (one per line). This script then takes
EVERY message from each of those sessions (the whole arc, not just the matching
line) out of the full dump.

Inputs:  /tmp/wend-profile.json   (wend messages --role user --json)
         /tmp/topic/sessions.txt  (related session ids, from wend search)
Writes:  /tmp/topic/shard_00.txt, shard_01.txt, ...  (~400KB each, session order)
Prints:  the shard count (pass it to the forge workflow as args.shards).
"""
import json
import os
import re

SRC = "/tmp/wend-profile.json"
IDS = "/tmp/topic/sessions.txt"
OUTDIR = "/tmp/topic"
SHARD_BYTES = 400_000
KEEP_FULL, HEAD, TAIL = 2000, 1000, 500


def crop(t: str) -> str:
    t = re.sub(r"\s+", " ", t).strip()
    if len(t) <= KEEP_FULL:
        return t
    return f"{t[:HEAD]} …[{len(t) - HEAD - TAIL} chars cut]… {t[-TAIL:]}"


def main() -> None:
    ids = {line.strip() for line in open(IDS) if line.strip()}
    if not ids:
        raise SystemExit(f"{IDS} is empty — run `wend search` first to fill it")

    data = json.load(open(SRC))
    sessions = {}
    for m in data:  # dump is prose-only, in session/line order
        sessions.setdefault(m["session_id"], []).append(m)
    related = {sid: sessions[sid] for sid in ids if sid in sessions}

    os.makedirs(OUTDIR, exist_ok=True)
    for old in os.listdir(OUTDIR):
        if old.startswith("shard_"):
            os.remove(os.path.join(OUTDIR, old))

    shard, buf, size, n = 0, [], 0, 0

    def flush():
        nonlocal shard, buf, size
        if not buf:
            return
        with open(os.path.join(OUTDIR, f"shard_{shard:02d}.txt"), "w") as f:
            f.write(f"===== SHARD {shard} =====\n")
            f.writelines(buf)
        shard += 1
        buf, size = [], 0

    for msgs in related.values():  # every message of each related session, in full
        head = msgs[0]
        hdr = f"\n===== session [{head['project']}] {head['title'][:60]} =====\n"
        buf.append(hdr); size += len(hdr)
        for m in msgs:
            line = f"• {crop(m['text'])}\n"
            buf.append(line); size += len(line); n += 1
        if size >= SHARD_BYTES:
            flush()
    flush()

    print(f"shards={shard}  sessions={len(related)}  messages={n}  (pass args.shards={shard})")


if __name__ == "__main__":
    main()
