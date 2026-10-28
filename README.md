# machands (Mac hands)

Fast, safe hands for computer-use agents. machands is a Rust MCP server that lets Claude Code, or any agent that
speaks MCP, see and drive native apps and a Brave browser on macOS and Windows. Risky steps stop for a one-click
Allow / Deny that you can answer from your phone.

## Why it exists

Most computer-use agents spend their time on screenshots. machands reads the screen as text first, from the
accessibility tree (the outline screen readers use), and only takes a screenshot when the agent asks for one. That
keeps each step cheap in tokens and fast in wall-clock time.

- **Text first.** `read` returns the window's accessibility tree with element references the agent can click
  directly. Screenshots default to 1024 px and can be skipped per call.
- **Native control.** Click, type, keys, scroll and wait on real apps, through macOS Accessibility or Windows UI
  Automation, with OCR as a fallback.
- **Browser.** Drives a separate Brave profile in debug mode, so your everyday profile and logins are never touched. Page text comes back
  wrapped as untrusted content, so instructions hidden in a web page are treated as data.
- **Safety gate.** Submit, Send, Buy, Delete and similar steps pause for approval. The agent cannot approve its own
  request.
- **Phone approvals.** With Claude Code Remote Control on, the same Allow / Deny box shows on your phone.

## Numbers

Measured on Windows against the earlier Python build, same machine, same local model.

| | Python | Rust (machands) |
|---|---|---|
| Time per step | 1.6 s | 0.93 s |
| Startup | not measured | 19 ms |
| Eval suite (grew from 29 to 34 tasks) | 22 of 29 | 29 of 34 |

## Install (Mac)

```
git clone https://github.com/LettuceEatKansas/machands.git ~/machands
cd ~/machands
./install.sh
```

The installer builds the app, signs it locally, installs it to `~/Applications/winhands.app` and connects it to Claude
Code. macOS then asks for two permissions: Accessibility (to click and type) and Screen Recording (to see the screen).
Step-by-step guide: [FRIENDS.md](FRIENDS.md). Details: [docs/INSTALL-mac.md](docs/INSTALL-mac.md).

## Layout

| Crate | What |
|---|---|
| `wh-core` | The `computer` tool, safety layer (approval gate, audit log, rate cap, pause when you touch the mouse), image pipeline. No OS code |
| `wh-mac` | macOS: CGEvent input, ScreenCaptureKit, the AX tree, Vision OCR |
| `wh-win` | Windows: SendInput, UI Automation, WinRT OCR |
| `wh-browser` | Brave over the Chrome DevTools Protocol, local port only, no extension or relay |
| `winhands` | HTTP daemon, MCP stdio server, `wh` CLI |

The macOS bundle id `com.lettuceatkansas.winhands` is the app's identity for macOS permissions and is kept as is.

## License

MIT. See [LICENSE](LICENSE).
