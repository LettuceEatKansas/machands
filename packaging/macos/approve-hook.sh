#!/bin/sh
# Claude Code UserPromptSubmit hook: when the user types "allow K7QM" or "deny K7QM" in chat, tell the Mac
# hands daemon, so the call Claude repeats with approval_code "K7QM" goes through (or is refused).
#
# Reads the hook JSON on stdin, looks only at the "prompt" text, and runs
#   winhands approve CODE   (for "allow CODE")   or   winhands deny CODE   (for "deny CODE")
# for every pair it finds. Prints nothing and ALWAYS exits 0, so it never blocks or changes a prompt.
#
# Codes are 4 characters from A-H J-N P-Z 2-9 (no 0, O, 1, I) with at least one digit, any case,
# whole words only.
# WINHANDS_BIN overrides the binary (tests).
#
# Register it in ~/.claude/settings.json:
#   "hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": "/path/to/approve-hook.sh"}]}]}

BIN="${WINHANDS_BIN:-$HOME/Applications/winhands.app/Contents/MacOS/winhands}"

# One line; JSON escapes (\n \t \r) become spaces so "...\nallow K7QM" still has a word edge; everything
# before the "prompt" key is dropped; then one alphanumeric token per line.
tr '\n' ' ' \
  | sed -e 's/\\[ntr]/ /g' -e 's/^.*"prompt"[[:space:]]*:[[:space:]]*//' \
  | tr -c 'A-Za-z0-9' '\n' \
  | awk '
      BEGIN { prev = "" }
      {
        w = tolower($0)
        if ((prev == "allow" || prev == "deny") && toupper($0) ~ /^[A-HJ-NP-Z2-9][A-HJ-NP-Z2-9][A-HJ-NP-Z2-9][A-HJ-NP-Z2-9]$/ && $0 ~ /[2-9]/) {
          code = toupper($0)
          if (!(prev " " code in seen)) { seen[prev " " code] = 1; print prev, code }
          prev = ""
        } else if (length($0) > 0) {
          prev = w
        }
      }' \
  | while read -r verb code; do
      case "$verb" in
        allow) "$BIN" approve "$code" >/dev/null 2>&1 || true ;;
        deny) "$BIN" deny "$code" >/dev/null 2>&1 || true ;;
      esac
    done

exit 0
