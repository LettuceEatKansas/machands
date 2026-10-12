#!/bin/bash
# Wire an installed winhands.app into Claude Code: MCP server, Allow/Deny pop-up mod, "allow CODE" chat hook,
# and a deny rule so the model cannot approve its own steps. Safe to re-run. Use --dry-run to preview.
# Run packaging/macos/install.sh first (the one-shot ./install.sh at the repo root does both).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
APP_BIN="${MACHANDS_BIN:-$HOME/Applications/winhands.app/Contents/MacOS/winhands}"
MOD_SRC="$HERE/machands-approve"
MOD_DST="$HOME/.claude/mods/machands-approve"
HOOK_SRC="$HERE/approve-hook.sh"
HOOK_DST="$HOME/.local/share/machands/approve-hook.sh"
SETTINGS="$HOME/.claude/settings.json"
DRY=0

for a in "$@"; do
  case "$a" in
    --dry-run) DRY=1 ;;
    -h|--help)
      echo "Usage: install-claude.sh [--dry-run]"
      echo "Registers the machands MCP server, installs the approval mod and chat hook, edits ~/.claude/settings.json (with a backup)."
      exit 0 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

say() { echo "==> $*"; }
run() { if [ "$DRY" = 1 ]; then echo "[dry-run] $*"; else "$@"; fi; }

command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 1; }
[ -f "$HOOK_SRC" ] && [ -d "$MOD_SRC" ] || { echo "run this from a full checkout (missing $MOD_SRC or $HOOK_SRC)" >&2; exit 1; }
if [ ! -x "$APP_BIN" ]; then
  echo "winhands is not installed at $APP_BIN" >&2
  echo "Run packaging/macos/install.sh first (or set MACHANDS_BIN)." >&2
  [ "$DRY" = 1 ] || exit 1
fi

# (a) MCP server
say "MCP server 'machands'"
if ! command -v claude >/dev/null; then
  echo "claude CLI not found on PATH. Install Claude Code, then run:" >&2
  echo "  claude mcp add --scope user machands -- \"$APP_BIN\" mcp" >&2
elif claude mcp get machands >/dev/null 2>&1; then
  echo "already registered, skipping"
else
  run claude mcp add --scope user machands -- "$APP_BIN" mcp
fi

# (b) mod
say "Approval pop-up mod -> $MOD_DST"
run mkdir -p "$MOD_DST"
run cp -R "$MOD_SRC/." "$MOD_DST/"

# (c) chat hook
say "Chat hook -> $HOOK_DST"
run mkdir -p "$(dirname "$HOOK_DST")"
run cp "$HOOK_SRC" "$HOOK_DST"
run chmod +x "$HOOK_DST"

# settings.json merge (plugin dir, UserPromptSubmit hook, deny rule). Backup first, write atomically.
say "Settings merge -> $SETTINGS"
DRY="$DRY" SETTINGS="$SETTINGS" MOD_DST="$MOD_DST" HOOK_DST="$HOOK_DST" python3 - <<'PY'
import json, os, shutil, sys, time

path, mod, hook = os.environ["SETTINGS"], os.environ["MOD_DST"], os.environ["HOOK_DST"]
dry = os.environ["DRY"] == "1"
deny_rule = "Bash(*winhands approve*)"

if os.path.exists(path):
    try:
        with open(path) as f:
            data = json.load(f)
    except ValueError as e:
        sys.exit(f"{path} is not valid JSON ({e}); fix it and re-run. Nothing was changed.")
    if not isinstance(data, dict):
        sys.exit(f"{path} is not a JSON object. Nothing was changed.")
else:
    data = {}
before = json.dumps(data, sort_keys=True)

env = data.setdefault("env", {})
dirs = [d for d in env.get("CLAUDE_CODE_PLUGIN_DIRS", "").split(":") if d]
if mod not in dirs:
    dirs.append(mod)
env["CLAUDE_CODE_PLUGIN_DIRS"] = ":".join(dirs)

ups = data.setdefault("hooks", {}).setdefault("UserPromptSubmit", [])
if not any(h.get("command") == hook for g in ups for h in g.get("hooks", [])):
    ups.append({"hooks": [{"type": "command", "command": hook}]})

deny = data.setdefault("permissions", {}).setdefault("deny", [])
if deny_rule not in deny:
    deny.append(deny_rule)

if json.dumps(data, sort_keys=True) == before:
    print("settings already up to date")
elif dry:
    print(f"[dry-run] would update {path}: plugin dir, UserPromptSubmit hook, deny rule {deny_rule}")
else:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    if os.path.exists(path):
        bak = f"{path}.bak-machands-{time.strftime('%Y%m%d-%H%M%S')}"
        shutil.copy2(path, bak)
        print(f"backup: {bak}")
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(data, f, indent=2)
        f.write("\n")
    os.replace(tmp, path)
    print(f"updated {path}")
PY

echo
echo "Done. Restart Claude Code (new session) so it loads the MCP server, the mod and the hook."
echo "Check: 'claude mcp list' shows machands connected."
