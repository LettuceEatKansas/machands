//! Starting Brave in debug mode, and finding it again.
//!
//! Debugging is only ever on when this module turned it on: `launch` starts Brave with
//! `--remote-debugging-port=<random> --remote-debugging-address=127.0.0.1`, and `state` finds such an
//! instance again by reading the process list. A Brave that is already running WITHOUT the flag is
//! never killed or touched: the caller is told to ask the owner to quit it.

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::error::{BrowserError, Result};
use crate::http;

/// The app bundle.
pub const BRAVE_APP: &str = "/Applications/Brave Browser.app";

/// The main executable inside the bundle. Helper processes live deeper and carry `--type=`.
const MAIN_EXE: &str = "Brave Browser.app/Contents/MacOS/Brave Browser";

/// What the process list says about Brave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BraveState {
    /// No Brave main process.
    NotRunning,
    /// Running, debugging off. The normal state.
    RunningWithoutDebugging,
    /// Running with the debug port on loopback.
    Debugging {
        /// The port.
        port: u16,
    },
    /// Running with a debug port bound somewhere other than loopback. Never attached to.
    DebuggingExposed {
        /// The port.
        port: u16,
        /// The address flag as given.
        address: String,
    },
}

/// The value of `--flag=value` in a command line.
pub fn flag_value<'a>(cmdline: &'a str, flag: &str) -> Option<&'a str> {
    let key = format!("{flag}=");
    cmdline
        .split_whitespace()
        .find_map(|w| w.strip_prefix(key.as_str()))
}

/// Read `ps -axo command` output (one command line per line) and say what Brave is doing. The main
/// process is the one running the bundle's own executable without `--type=` (helpers all have it).
pub fn classify(ps_output: &str) -> BraveState {
    let main = ps_output
        .lines()
        .map(str::trim)
        .find(|l| is_main_exe(l) && !l.contains("--type=") && !l.contains("Helper"));
    let Some(line) = main else {
        return BraveState::NotRunning;
    };
    let Some(port) =
        flag_value(line, "--remote-debugging-port").and_then(|p| p.parse::<u16>().ok())
    else {
        return BraveState::RunningWithoutDebugging;
    };
    if port == 0 {
        // "pick one for me" would need the DevToolsActivePort file; we always pass an explicit port
        return BraveState::RunningWithoutDebugging;
    }
    match flag_value(line, "--remote-debugging-address") {
        None | Some("127.0.0.1" | "localhost" | "::1") => BraveState::Debugging { port },
        Some(other) => BraveState::DebuggingExposed {
            port,
            address: other.to_string(),
        },
    }
}

/// The command line runs Brave's executable itself, not a shell or tool whose arguments merely
/// mention the path (a `zsh -c "... Brave Browser.app/..."` line once read as a running Brave).
fn is_main_exe(line: &str) -> bool {
    line.starts_with('/') && line.find(MAIN_EXE).is_some_and(|i| !line[..i].contains(' '))
}

/// The arguments for a debug launch. `user_data_dir` is only for the case the stock profile
/// directory refuses the flag (see the README).
pub fn launch_args(port: u16, user_data_dir: Option<&str>) -> Vec<String> {
    let mut args = vec![
        format!("--remote-debugging-port={port}"),
        "--remote-debugging-address=127.0.0.1".to_string(),
    ];
    if let Some(dir) = user_data_dir.filter(|d| !d.is_empty()) {
        args.push(format!("--user-data-dir={dir}"));
    }
    args
}

/// A free loopback port, picked by the OS.
pub fn pick_port() -> Result<u16> {
    let l = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .map_err(|e| BrowserError::failed(format!("cannot pick a free port: {e}")))?;
    l.local_addr()
        .map(|a| a.port())
        .map_err(|e| BrowserError::failed(format!("cannot read the picked port: {e}")))
}

/// Look at the process list.
pub fn state() -> Result<BraveState> {
    if !cfg!(unix) {
        return Err(BrowserError::failed(
            "the Brave launcher only supports macOS for now",
        ));
    }
    let out = Command::new("ps")
        .args(["-axo", "command="])
        .output()
        .map_err(|e| BrowserError::failed(format!("cannot run ps: {e}")))?;
    Ok(classify(&String::from_utf8_lossy(&out.stdout)))
}

/// The message for a Brave that must be relaunched by the owner.
pub const RELAUNCH_NEEDED: &str = "Brave is running without remote debugging. It must be quit and \
    relaunched in debug mode. Ask the user to quit Brave (Cmd+Q), then call browser_launch. I will not \
    kill their browser.";

/// The port to drive, if Brave is already in debug mode. Errors say what to do next.
pub fn attach_port() -> Result<u16> {
    match state()? {
        BraveState::Debugging { port } => {
            http::version(port).map_err(|e| {
                BrowserError::failed(format!(
                    "Brave was started with --remote-debugging-port={port} but the port does not \
                     answer ({e}). Brave may be ignoring the flag for the stock profile \
                     directory; see the wh-browser README."
                ))
            })?;
            Ok(port)
        }
        BraveState::NotRunning => Err(BrowserError::failed(
            "Brave is not running. Call browser_launch to start it in debug mode.",
        )),
        BraveState::RunningWithoutDebugging => Err(BrowserError::failed(RELAUNCH_NEEDED)),
        BraveState::DebuggingExposed { port, address } => Err(BrowserError::refused(format!(
            "Brave has a debug port ({port}) bound to {address}, not loopback. Anything on the \
             network could drive it. Ask the user to quit Brave and relaunch it with browser_launch"
        ))),
    }
}

/// Start Brave in debug mode if it is not running at all. Returns the port. If a debug instance is
/// already up this just returns its port.
pub fn launch(user_data_dir: Option<&str>) -> Result<u16> {
    match state()? {
        BraveState::Debugging { port } => return Ok(port),
        BraveState::RunningWithoutDebugging => return Err(BrowserError::failed(RELAUNCH_NEEDED)),
        BraveState::DebuggingExposed { .. } => return attach_port(),
        BraveState::NotRunning => {}
    }
    if !std::path::Path::new(BRAVE_APP).exists() {
        return Err(BrowserError::failed(format!("{BRAVE_APP} not found")));
    }
    let port = pick_port()?;
    // `open` hands the arguments to a fresh Brave and returns; Brave outlives this process.
    Command::new("open")
        .arg("-a")
        .arg(BRAVE_APP)
        .arg("--args")
        .args(launch_args(port, user_data_dir))
        .spawn()
        .map_err(|e| BrowserError::failed(format!("cannot start Brave: {e}")))?;
    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        if http::version(port).is_ok() {
            return Ok(port);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(BrowserError::failed(format!(
        "Brave was started with --remote-debugging-port={port} but nothing answers on it after \
         25 s. Newer Chromium ignores the flag for the default profile directory; relaunch with a \
         non-default user data dir (see the wh-browser README)."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: &str = "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser";
    const HELPER: &str = "/Applications/Brave Browser.app/Contents/Frameworks/Brave Browser Framework.framework/Versions/155.1.97.56/Helpers/Brave Browser Helper (Renderer).app/Contents/MacOS/Brave Browser Helper (Renderer) --type=renderer --lang=en-US";

    #[test]
    fn no_brave_process_is_not_running() {
        assert_eq!(
            classify("/usr/bin/foo\n/bin/zsh -l\n"),
            BraveState::NotRunning
        );
        assert_eq!(classify(""), BraveState::NotRunning);
        assert_eq!(
            classify("/bin/zsh -c pgrep -f \"/Applications/Brave Browser.app/Contents/MacOS/Brave Browser\"\n"),
            BraveState::NotRunning
        );
    }

    #[test]
    fn a_plain_launch_has_debugging_off_even_with_helpers_around() {
        let ps = format!("/bin/zsh\n{MAIN}\n{HELPER}\n");
        assert_eq!(classify(&ps), BraveState::RunningWithoutDebugging);
    }

    #[test]
    fn a_helper_alone_is_not_the_main_process() {
        assert_eq!(classify(HELPER), BraveState::NotRunning);
    }

    #[test]
    fn a_debug_launch_is_found_by_its_port() {
        let ps = format!(
            "{MAIN} --remote-debugging-port=51234 --remote-debugging-address=127.0.0.1\n{HELPER}\n"
        );
        assert_eq!(classify(&ps), BraveState::Debugging { port: 51234 });
    }

    #[test]
    fn a_port_without_an_address_defaults_to_loopback() {
        let ps = format!("{MAIN} --remote-debugging-port=51234");
        assert_eq!(classify(&ps), BraveState::Debugging { port: 51234 });
    }

    #[test]
    fn a_non_loopback_address_is_never_attached_to() {
        let ps = format!("{MAIN} --remote-debugging-port=9222 --remote-debugging-address=0.0.0.0");
        assert_eq!(
            classify(&ps),
            BraveState::DebuggingExposed {
                port: 9222,
                address: "0.0.0.0".into()
            }
        );
    }

    #[test]
    fn port_zero_counts_as_off() {
        let ps = format!("{MAIN} --remote-debugging-port=0");
        assert_eq!(classify(&ps), BraveState::RunningWithoutDebugging);
    }

    #[test]
    fn launch_args_bind_loopback_only() {
        let a = launch_args(4242, None);
        assert_eq!(
            a,
            [
                "--remote-debugging-port=4242",
                "--remote-debugging-address=127.0.0.1"
            ]
        );
        let b = launch_args(4242, Some("/tmp/x"));
        assert_eq!(b.last().unwrap(), "--user-data-dir=/tmp/x");
        assert!(!a.iter().any(|x| x.contains("0.0.0.0")));
    }

    #[test]
    fn picked_ports_are_nonzero_and_not_in_use() {
        let p = pick_port().unwrap();
        assert!(p > 0);
        assert!(!http::port_open(p));
    }
}
