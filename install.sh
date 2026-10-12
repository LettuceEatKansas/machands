#!/bin/bash
# One-shot installer for a Mac: build + install winhands.app, then wire it into Claude Code.
# Extra arguments go to packaging/macos/install.sh (for example --yes). Use --dry-run alone to preview the Claude step.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
if [ "${1:-}" = "--dry-run" ]; then exec "$ROOT/packaging/claude/install-claude.sh" --dry-run; fi
"$ROOT/packaging/macos/install.sh" "$@"
"$ROOT/packaging/claude/install-claude.sh"
