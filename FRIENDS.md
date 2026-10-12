# Mac hands for Claude Code: setup guide for friends

Mac hands (the program is called `winhands`) lets Claude Code see your screen, click, type and drive your Brave
browser on your own Mac. Anything risky (submitting a form, pressing Send or Buy, deleting) stops and asks you first,
with an Allow / Deny pop-up. Nothing leaves your computer except what Claude itself sees.

## What you need first

- A Mac, Apple Silicon or Intel, on a recent macOS.
- Xcode Command Line Tools: run `xcode-select --install` in Terminal.
- Rust: install from https://rustup.rs (one line in Terminal, accept the defaults), then open a new Terminal window.
- Claude Code (the `claude` command) with a working login.
- Brave browser, if you want Claude to use the web.

## Install (one command)

```
git clone https://github.com/LettuceEatKansas/machands.git ~/machands
cd ~/machands
./install.sh
```

This builds the app (a few minutes the first time), signs it with a local certificate, puts it in
`~/Applications/winhands.app`, starts it, and then connects it to Claude Code. It is safe to run again later to update.
It edits `~/.claude/settings.json` and saves a backup copy next to it first. To preview the Claude Code part without
changing anything: `./install.sh --dry-run`.

## Give it permission (macOS asks for two things)

The installer opens System Settings. Turn winhands ON in both lists:

1. Privacy & Security > Accessibility (lets it click and type).
2. Privacy & Security > Screen Recording (lets it see the screen).

Then start a fresh Claude Code session. `claude mcp list` should show `machands` as connected.

## Brave

Claude can only drive Brave when Brave runs in a special debug mode. You do not set that up by hand: ask Claude to use
the `browser_launch` tool. It asks you to quit Brave (Cmd+Q) first, then reopens it with a separate profile. Your normal
Brave profile and logins are not touched.

## The Allow pop-up

When a step is risky, Claude shows a small box: "Mac hands wants to: Click Send ... Allow it once?" with Deny and Allow.
Allow runs that one step only. Deny means it is not done and Claude is told not to look for another way. Claude cannot
press Allow for you: the deny rule added by the installer blocks it from running the approve command itself.

## Approving from your phone

Turn on Remote Control for your Claude Code session (in the Claude app, open the session from your phone). The same
Allow / Deny box appears there, so you can answer away from your desk.

## Fallback: type "allow CODE"

If no box can be shown, the refusal contains a 4-character code such as `K7QM`. Reply in the chat with `allow K7QM`
(or `deny K7QM`). A small hook sees that and unlocks the step, then Claude repeats the call. Type it exactly, in your
own message.

## Uninstall

```
~/machands/packaging/macos/uninstall.sh
claude mcp remove machands --scope user
rm -rf ~/.claude/mods/machands-approve ~/.local/share/machands
```

Then open `~/.claude/settings.json` and remove three things the installer added: the `machands-approve` entry in
`env.CLAUDE_CODE_PLUGIN_DIRS`, the `approve-hook.sh` entry under `hooks.UserPromptSubmit`, and the deny rule
`Bash(*winhands approve*)`. Backups are named `settings.json.bak-machands-*`.

## Troubleshooting

More detail, including how macOS ties permissions to the app, is in [docs/INSTALL-mac.md](docs/INSTALL-mac.md).
