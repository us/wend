#!/usr/bin/env bash
# Keep the wend index fresh in the background on session start.
#
# Hard requirements (learned the hard way):
#  - Return IMMEDIATELY: SessionStart blocks the session until the hook exits.
#  - Be SILENT on stdout: a SessionStart hook's stdout is injected into the
#    session context.
#  - Close stdin (</dev/null): a background process that inherits the parent's
#    stdin keeps the stream-json pipe open and hangs Claude Code (#43123).
#
# So: if `wend` is installed, spawn a fully-detached incremental index and exit 0.

command -v wend >/dev/null 2>&1 || exit 0
nohup wend index --incremental </dev/null >/dev/null 2>&1 & disown 2>/dev/null || true
exit 0
