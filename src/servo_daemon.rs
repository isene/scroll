//! Client for the long-running `scroll-servo --daemon` helper.
//!
//! The daemon runs Servo + WebView in a separate process, listens on
//! a Unix socket, and accepts line-delimited JSON commands. This
//! module owns the socket connection lifetime and lazily spawns the
//! daemon when the user first hits `:servo`.
//!
//! Single connection at a time — scroll is the only client. Reusing
//! one connection across many commands keeps the page state between
//! calls (so the second `:servo` to the same site can use the JS
//! state from the first).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct DaemonClient {
    socket_path: PathBuf,
    conn: Option<UnixStream>,
    /// Cached BufReader so we can read line-delimited responses.
    /// Held in a parallel Option that gets reset alongside `conn`.
    reader: Option<BufReader<UnixStream>>,
}

#[derive(Debug)]
pub struct NavResult {
    pub html: String,
    pub url: String,
    pub frames: u64,
    /// True if the daemon hit its 15s load timeout; the HTML may be a
    /// partial-paint snapshot. Caller surfaces this so the user knows
    /// the rendering may be incomplete.
    pub timed_out: bool,
}

impl DaemonClient {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        DaemonClient {
            socket_path: PathBuf::from(format!("{home}/.scroll/servo.sock")),
            conn: None,
            reader: None,
        }
    }

    fn try_connect(&mut self) -> bool {
        match UnixStream::connect(&self.socket_path) {
            Ok(s) => {
                let _ = s.set_read_timeout(Some(Duration::from_secs(120)));
                let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
                let reader = BufReader::new(s.try_clone().unwrap());
                self.conn = Some(s);
                self.reader = Some(reader);
                true
            }
            Err(_) => false,
        }
    }

    /// Spawn `scroll-servo --daemon` and wait up to ~10 s for it to
    /// bind the socket. Returns Err if the daemon failed to come up.
    fn spawn_daemon(&mut self, ua: &str) -> Result<(), String> {
        // Detach: the child shouldn't tie its stdin/out to scroll's tty.
        // setsid is the cleanest separator on Unix; we stash stderr in
        // a log file so a daemon panic isn't lost to /dev/null.
        let log_path = format!(
            "{}/.scroll/servo-daemon.log",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())
        );
        if let Some(parent) = Path::new(&log_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| format!("opening daemon log: {e}"))?;

        std::process::Command::new("scroll-servo")
            .arg("--daemon")
            .env("SCROLL_SERVO_UA", ua)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(log))
            .spawn()
            .map_err(|e| format!("spawning scroll-servo daemon: {e}"))?;

        // Wait for the socket to appear and accept a connection.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.socket_path.exists() && self.try_connect() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(format!(
            "scroll-servo daemon didn't bind {} within 10s — check {log_path}",
            self.socket_path.display()
        ))
    }

    /// Establish a working connection, spawning the daemon if needed.
    fn ensure_connected(&mut self, ua: &str) -> Result<(), String> {
        // Drop a stale reader-only state; both must move together.
        if self.conn.is_none() && self.reader.is_some() { self.reader = None; }
        if self.conn.is_none() {
            if self.socket_path.exists() && self.try_connect() {
                return Ok(());
            }
            return self.spawn_daemon(ua);
        }
        Ok(())
    }

    fn send_command(&mut self, json: &str, ua: &str) -> Result<serde_json::Value, String> {
        // Up to two attempts: if the first send fails (stale daemon),
        // tear down, respawn, retry.
        for attempt in 0..2 {
            if attempt > 0 {
                self.conn = None;
                self.reader = None;
            }
            if let Err(e) = self.ensure_connected(ua) {
                if attempt == 1 { return Err(e); }
                continue;
            }
            if let Err(e) = self.write_line(json) {
                if attempt == 1 { return Err(format!("write: {e}")); }
                continue;
            }
            match self.read_response() {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if attempt == 1 { return Err(e); }
                    continue;
                }
            }
        }
        Err("send_command exhausted retries".into())
    }

    fn write_line(&mut self, json: &str) -> std::io::Result<()> {
        let conn = self.conn.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotConnected, "no conn")
        })?;
        conn.write_all(json.as_bytes())?;
        conn.write_all(b"\n")?;
        conn.flush()
    }

    fn read_response(&mut self) -> Result<serde_json::Value, String> {
        let reader = self.reader.as_mut().ok_or("no reader")?;
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => Err("daemon closed connection".into()),
            Ok(_) => serde_json::from_str(line.trim_end_matches('\n'))
                .map_err(|e| format!("bad json from daemon: {e} (line: {line})")),
            Err(e) => Err(format!("read: {e}")),
        }
    }

    /// Install cookies from `jar_path` (scroll's per-set jar JSON) for
    /// the host of `for_url`. Returns the count installed.
    pub fn install_cookies(&mut self, jar_path: &Path, for_url: &str, ua: &str) -> Result<u64, String> {
        let req = serde_json::json!({
            "cmd": "cookies",
            "jar": jar_path.to_string_lossy().to_string(),
            "for": for_url,
        });
        let resp = self.send_command(&req.to_string(), ua)?;
        if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return Err(resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown").to_string());
        }
        Ok(resp.get("installed").and_then(|v| v.as_u64()).unwrap_or(0))
    }

    pub fn navigate(&mut self, url: &str, ua: &str) -> Result<NavResult, String> {
        let req = serde_json::json!({"cmd": "navigate", "url": url});
        let resp = self.send_command(&req.to_string(), ua)?;
        if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return Err(resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown").to_string());
        }
        Ok(NavResult {
            html: resp.get("html").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            url: resp.get("url").and_then(|v| v.as_str()).unwrap_or(url).to_string(),
            frames: resp.get("frames").and_then(|v| v.as_u64()).unwrap_or(0),
            timed_out: resp.get("timed_out").and_then(|v| v.as_bool()).unwrap_or(false),
        })
    }

    /// Type `text` into the element matching `selector`. Returns the
    /// fresh outerHTML so the caller can re-render the tab and pick
    /// up any framework-driven UI changes (React's onChange wired to
    /// state changes, input mirroring, validation messages, etc.).
    pub fn type_into(&mut self, selector: &str, text: &str, ua: &str) -> Result<NavResult, String> {
        let req = serde_json::json!({"cmd": "type", "selector": selector, "text": text});
        let resp = self.send_command(&req.to_string(), ua)?;
        if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return Err(resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown").to_string());
        }
        match resp.get("result").and_then(|v| v.as_str()) {
            Some("ok") => Ok(NavResult {
                html: resp.get("html").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                url: String::new(),
                frames: 0,
                timed_out: false,
            }),
            Some("not_found") => Err(format!("no element matched selector {selector}")),
            other => Err(format!("unexpected result: {other:?}")),
        }
    }

    /// Click the element matching `selector`. Returns the post-click
    /// outerHTML and the (possibly new, if click navigated) URL.
    pub fn click(&mut self, selector: &str, ua: &str) -> Result<NavResult, String> {
        let req = serde_json::json!({"cmd": "click", "selector": selector});
        let resp = self.send_command(&req.to_string(), ua)?;
        if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return Err(resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown").to_string());
        }
        match resp.get("result").and_then(|v| v.as_str()) {
            Some("ok") => Ok(NavResult {
                html: resp.get("html").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                url: resp.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                frames: 0,
                timed_out: false,
            }),
            Some("not_found") => Err(format!("no element matched selector {selector}")),
            other => Err(format!("unexpected result: {other:?}")),
        }
    }

    /// Cooperative shutdown — daemon writes back "bye" then exits.
    pub fn shutdown(&mut self) -> Result<(), String> {
        if !self.socket_path.exists() {
            self.conn = None; self.reader = None;
            return Ok(()); // already gone
        }
        if self.conn.is_none() && !self.try_connect() {
            // Daemon left a stale socket; clean it up.
            let _ = std::fs::remove_file(&self.socket_path);
            return Ok(());
        }
        let _ = self.write_line(r#"{"cmd":"shutdown"}"#);
        // Read the "bye" response so the daemon doesn't see our half-
        // close before it has a chance to finish writing.
        let _ = self.read_response();
        self.conn = None;
        self.reader = None;
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.socket_path.exists()
    }
}
