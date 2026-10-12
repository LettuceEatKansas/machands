# wh-browser

A small Chrome DevTools Protocol (CDP) client that lets winhands read and drive the user's real Brave. Phase 1 of
`projects/life-automation/machands-browser-spec.md`.

No extension, no network relay, no User-Agent or header changes. The debug port is on `127.0.0.1` only, on a
random port, and exists only when Brave was started in debug mode.

## Actions (through the `computer` MCP tool)

| Call | Does |
|---|---|
| `{"action":"browser_status"}` | Brave state: not running, running without debugging, or debug port + tab count |
| `{"action":"browser_launch"}` | Start Brave in debug mode if it is not running. Never kills a running Brave |
| `{"action":"browser_navigate","url":"https://..."}` | Open a URL (http/https only), wait for load (`timeout` s, default 15) |
| `{"action":"browser_read"}` | Compact element tree with refs (`e3`), `limit` = char budget (default 8000), `filter` = `text` or `all` for main text |
| `{"action":"browser_find","text":"..."}` | Elements and text matching, with refs |
| `{"action":"browser_click","ref":"e3"}` | Click by ref (gated) |
| `{"action":"browser_type","ref":"e2","text":"..."}` | Replace a text field's contents (gated) |
| `{"action":"browser_scroll","scroll_direction":"down","scroll_amount":3}` | Scroll, or `ref` to bring an element into view |
| `{"action":"browser_tabs","op":"list"}` | `list`, `switch` (`nth` or `text`, `wake` to raise it), `new` (`url`), `close` (only tabs this module opened) |
| `{"action":"browser_screenshot"}` | JPEG of the visible page, only when asked |

Batch with `{"actions":[{...},{...}]}`. A batch is all browser steps or all desktop steps. It stops at the first
failure, like the desktop actions.

## Launching Brave in debug mode

The module does it: `browser_launch`. By hand:

    open -a "/Applications/Brave Browser.app" --args --remote-debugging-port=<random> --remote-debugging-address=127.0.0.1

Brave must not already be running (a running Brave ignores new flags). If it is, quit it first (Cmd+Q). The
module never kills Brave. A normal launch has debugging off.

Known risk (unverified on Brave 155): recent Chromium ignores `--remote-debugging-port` when the user data dir
is the default one. If `browser_launch` reports that nothing answers on the port, relaunch with a non-default
`--user-data-dir` (set `MACHANDS_BRAVE_USER_DATA_DIR` for `browser_launch`). That gives a profile without the user's
cookies, so it needs a decision (see the spec's profile choice).

## Safety gate

Refused with a message that says to ask the user in chat, on any site: clicks that submit a form; buttons labelled like
send, submit, buy, pay, delete, accept, agree, install, confirm and similar; typing into password or payment
fields; text with a line break (Enter submits); CAPTCHA widgets (never solved); non-http(s) URLs. A click is also
checked against the element that would really receive it, so an overlay cannot hide a risky button. Cookie
banners: prefer the "Reject" or "Necessary only" button; `browser_read` hints at its ref when both choices exist.

When the MCP client supports elicitation (Claude Code form mode), everything above except password or payment
typing and CAPTCHAs becomes an Allow/Deny card in the session instead of a flat refusal. Allow runs only that exact
step and target, once (single-use token, 5 minutes). Deny, decline, cancel, no answer in 120 s, or a client without
elicitation gives the refusal text unchanged. In a batch, the steps before the waiting one run, the card is shown, and
the batch continues from that step on Allow. Every card and answer is logged.

Page text comes back inside `<<UNTRUSTED_PAGE_CONTENT id=...>>` markers with a note that it is data, not
instructions.

## Logs

`~/Library/Logs/MacHands/browser/<date>.jsonl`, local only, files older than 14 days removed at start. Holds the
action, ref, host and outcome. Never page content or typed text (only its length).

## Tests

    cargo test -p wh-browser

The client is tested against a mock DevTools server (`tests/mock_cdp.rs`) and fixtures. No test starts or
touches a real browser.

## Phase 2 TODO

- Per-site allowlist: ask once per new domain, store in a local revocable list, with a management UI.
- macOS notification fallback when the user is away from the session.
- Terms and consent flow: show the site, what is accepted and a 3-line summary, click Accept only after Approve.
- CAPTCHA handoff: bring the tab forward or notify, detect the challenge cleared, resume.
- Downloads and browser permission or security prompts: detect and always ask (not intercepted yet).
- Session token on the debug port (Chromium has no built-in support; needs a local proxy or pipe transport).
- Select/dropdown, file upload, key presses, hover, wait-for-element actions.
- Iframes and shadow DOM in the reader (the AX tree covers the main frame).
- Report the active IP/VPN per session.
- Measure peak RAM and read latency on a real news page (acceptance 2 and 8).
- Windows/Linux Brave paths (launcher is macOS only).
