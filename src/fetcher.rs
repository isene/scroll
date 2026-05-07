use std::collections::HashMap;
use std::fs;
use crate::config;

use rquest::{Client, Method};
use rquest_util::Emulation;
use tokio::runtime::Runtime;

/// Cookie jar: domain → {name → value}. One per tab-set so the user
/// can be logged in as different identities in different sets.
type CookieJar = HashMap<String, HashMap<String, String>>;

pub struct Fetcher {
    /// All loaded jars, keyed by set name. Lazy-loaded on first
    /// `set_active_set` for that set.
    jars: HashMap<String, CookieJar>,
    /// Which set's jar is currently active for outgoing requests and
    /// incoming Set-Cookie storage. Always present in `jars`.
    active_set: String,
    cache: HashMap<String, FetchResult>,
    cache_order: Vec<String>,  // LRU order for eviction
    max_cache: usize,
    /// Long-lived current-thread tokio runtime. rquest is async-only,
    /// scroll's call sites are sync — `block_on` bridges the two.
    runtime: Runtime,
    /// Browser-impersonating HTTP client. The `Emulation::Firefox136`
    /// profile sets cipher order, ALPN, HTTP/2 SETTINGS frame order
    /// and the canonical Firefox header set, so Cloudflare's
    /// JA3-bound `cf_clearance` cookie keeps validating after import.
    client: Client,
}

#[derive(Clone)]
pub struct FetchResult {
    pub body: String,
    pub content_type: String,
    pub url: String,       // final URL after redirects
    pub status: u16,
}

impl Fetcher {
    /// Construct a fetcher with `set_name` as the active cookie jar.
    /// On first run, migrates any pre-existing single-jar
    /// `~/.scroll/cookies.json` into `~/.scroll/cookies/<set_name>.json`
    /// so existing logins survive the per-set isolation upgrade.
    pub fn new_with_set(set_name: &str) -> Self {
        Self::migrate_legacy_cookies_jar(set_name);
        let mut jars: HashMap<String, CookieJar> = HashMap::new();
        jars.insert(set_name.to_string(), Self::load_jar(set_name));

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let client = Client::builder()
            .emulation(Emulation::Firefox136)
            .timeout(std::time::Duration::from_secs(15))
            .redirect(rquest::redirect::Policy::limited(10))
            .build()
            .expect("rquest client");

        Fetcher {
            jars,
            active_set: set_name.to_string(),
            cache: HashMap::new(),
            cache_order: Vec::new(),
            max_cache: 20,
            runtime,
            client,
        }
    }

    fn load_jar(set_name: &str) -> CookieJar {
        config::cookie_jar_path(set_name)
            .to_str()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn migrate_legacy_cookies_jar(default_set: &str) {
        let legacy = config::cookies_path();
        let dest = config::cookie_jar_path(default_set);
        if legacy.exists() && !dest.exists() {
            let _ = fs::create_dir_all(config::cookies_dir());
            let _ = fs::rename(&legacy, &dest);
        }
    }

    /// Switch the active jar. Saves whatever was just being used,
    /// loads (or initialises) the destination.
    pub fn set_active_set(&mut self, set_name: &str) {
        if self.active_set == set_name { return; }
        self.save_active_jar();
        if !self.jars.contains_key(set_name) {
            self.jars.insert(set_name.to_string(), Self::load_jar(set_name));
        }
        self.active_set = set_name.to_string();
    }

    /// Rename a set's jar file when the user renames the set.
    pub fn rename_set(&mut self, old_name: &str, new_name: &str) {
        if old_name == new_name { return; }
        if let Some(jar) = self.jars.remove(old_name) {
            self.jars.insert(new_name.to_string(), jar);
        }
        if self.active_set == old_name {
            self.active_set = new_name.to_string();
        }
        let old_path = config::cookie_jar_path(old_name);
        let new_path = config::cookie_jar_path(new_name);
        if old_path.exists() {
            let _ = fs::create_dir_all(config::cookies_dir());
            let _ = fs::rename(&old_path, &new_path);
        }
    }

    /// Snapshot the cookies in the active jar that belong to the
    /// exact `host` (no subdomain walk — JS only sees its own host's
    /// cookies via `document.cookie`). Used by the JS layer.
    pub fn cookies_for_host(&self, host: &str) -> std::collections::HashMap<String, String> {
        self.jars.get(&self.active_set)
            .and_then(|jar| jar.get(host))
            .cloned()
            .unwrap_or_default()
    }

    /// Replace this host's cookie map with `cookies` in the active
    /// jar, then persist. Called after JS runs `document.cookie =`
    /// writes so the next request includes the JS-set cookies.
    pub fn replace_cookies_for_host(&mut self, host: &str, cookies: std::collections::HashMap<String, String>) {
        let active = self.active_set.clone();
        let jar = self.jars.entry(active).or_default();
        if cookies.is_empty() {
            jar.remove(host);
        } else {
            jar.insert(host.to_string(), cookies);
        }
        self.save_active_jar();
    }

    /// The active set's name. Used by the JS layer to scope per-set,
    /// per-origin localStorage on disk.
    pub fn active_set_name(&self) -> &str { &self.active_set }

    /// Import cookies from a Firefox profile into the currently
    /// active jar. `profile` may be a bare profile name (resolved
    /// against `~/.mozilla/firefox/profiles.ini`) or an absolute
    /// profile directory. Returns the number of cookies imported,
    /// or `None` on any failure (missing profile, locked db, etc.).
    /// Best-effort: if Firefox is running with an exclusive lock on
    /// `cookies.sqlite`, the import silently no-ops so scroll keeps
    /// working with whatever it imported last.
    pub fn import_firefox_cookies(&mut self, profile: &str) -> Option<usize> {
        let dir = Self::resolve_firefox_profile(profile)?;
        let db = dir.join("cookies.sqlite");
        if !db.exists() { return None; }

        // Firefox keeps recent writes in the WAL until checkpoint, and
        // holds an exclusive lock on the live db while running. Reading
        // with `immutable=1` ignores the WAL — we'd see only stale
        // committed pages, missing every session cookie set this hour.
        // Solution: copy db + WAL + SHM to a private temp dir and open
        // the copy normally. SQLite then rolls the WAL into the read
        // and we get exactly what Firefox sees.
        let tmp = std::env::temp_dir().join(format!(
            "scroll-cookies-{}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&tmp);
        let dst_db = tmp.join("cookies.sqlite");
        if fs::copy(&db, &dst_db).is_err() {
            let _ = fs::remove_dir_all(&tmp);
            return None;
        }
        for sidecar in ["cookies.sqlite-wal", "cookies.sqlite-shm"] {
            let src = dir.join(sidecar);
            if src.exists() { let _ = fs::copy(&src, tmp.join(sidecar)); }
        }

        let conn = rusqlite::Connection::open_with_flags(
            &dst_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ).ok();
        let count = if let Some(conn) = conn {
            let mut count = 0usize;
            // Order by lastAccessed DESC so the most-recently-used row
            // for each (host, name) lands first. Combined with
            // entry().or_insert() (first-wins), this picks the cookie
            // value the user's active Firefox container is currently
            // sending — even when there are stale rows from other
            // containers (Multi-Account Containers stores them all).
            // Without this, an anonymous-tab `s` value can clobber
            // the logged-in container's `s` for the same host.
            if let Ok(mut stmt) = conn.prepare(
                "SELECT host, name, value FROM moz_cookies ORDER BY lastAccessed DESC"
            ) {
                if let Ok(rows) = stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                }) {
                    let active = self.active_set.clone();
                    let jar = self.jars.entry(active).or_default();
                    for row in rows.flatten() {
                        let (mut host, name, value) = row;
                        // Firefox stores subdomain-shareable cookies with a
                        // leading dot (".google.com"); our outbound walk
                        // ascends from the full host, so store the bare form.
                        if host.starts_with('.') { host = host[1..].to_string(); }
                        jar.entry(host).or_default().entry(name).or_insert(value);
                        count += 1;
                    }
                }
            }
            self.save_active_jar();
            Some(count)
        } else {
            None
        };
        let _ = fs::remove_dir_all(&tmp);
        count
    }

    /// Path to the system's default Firefox profile, by reading the
    /// `[Install…] Default=…` line in `profiles.ini` (the same key
    /// Firefox itself uses to pick a profile when launched without
    /// `-P`). Used as the fallback when the user has no explicit
    /// `firefox_profiles` mapping for the active set.
    fn default_firefox_profile_path() -> Option<std::path::PathBuf> {
        let home = std::env::var("HOME").ok()?;
        let ff_root = std::path::PathBuf::from(&home).join(".mozilla/firefox");
        let ini = fs::read_to_string(ff_root.join("profiles.ini")).ok()?;
        let mut in_install = false;
        let mut default_path: Option<String> = None;
        for line in ini.lines() {
            let line = line.trim();
            if line.starts_with("[Install") { in_install = true; continue; }
            if line.starts_with('[') { in_install = false; continue; }
            if in_install {
                if let Some(v) = line.strip_prefix("Default=") {
                    default_path = Some(v.to_string());
                    break;
                }
            }
        }
        let p = default_path?;
        let resolved = ff_root.join(&p);
        if resolved.is_dir() { Some(resolved) } else { None }
    }

    fn resolve_firefox_profile(profile: &str) -> Option<std::path::PathBuf> {
        // Empty profile arg = use the system default.
        if profile.is_empty() {
            return Self::default_firefox_profile_path();
        }
        // Absolute path — use it directly.
        let p = std::path::Path::new(profile);
        if p.is_absolute() && p.is_dir() {
            return Some(p.to_path_buf());
        }
        // Otherwise treat `profile` as a profile NAME and look it up
        // in profiles.ini, like Firefox itself does.
        let home = std::env::var("HOME").ok()?;
        let ff_root = std::path::PathBuf::from(&home).join(".mozilla/firefox");
        let ini_path = ff_root.join("profiles.ini");
        let ini = fs::read_to_string(&ini_path).ok()?;
        let mut current_name: Option<String> = None;
        let mut current_path: Option<String> = None;
        let mut current_relative = true;
        let mut found: Option<(String, bool)> = None;
        for line in ini.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                if let (Some(n), Some(pa)) = (current_name.take(), current_path.take()) {
                    if n == profile {
                        found = Some((pa, current_relative));
                        break;
                    }
                }
                current_relative = true;
                continue;
            }
            if let Some(v) = line.strip_prefix("Name=") { current_name = Some(v.to_string()); }
            else if let Some(v) = line.strip_prefix("Path=") { current_path = Some(v.to_string()); }
            else if line == "IsRelative=0" { current_relative = false; }
        }
        // Tail section (no closing [Profile…] after it)
        if found.is_none() {
            if let (Some(n), Some(pa)) = (current_name, current_path) {
                if n == profile { found = Some((pa, current_relative)); }
            }
        }
        let (path, relative) = found?;
        let resolved = if relative { ff_root.join(path) } else { std::path::PathBuf::from(path) };
        if resolved.is_dir() { Some(resolved) } else { None }
    }

    fn save_active_jar(&self) {
        if let Some(jar) = self.jars.get(&self.active_set) {
            if let Ok(json) = serde_json::to_string_pretty(jar) {
                let _ = fs::create_dir_all(config::cookies_dir());
                let _ = fs::write(config::cookie_jar_path(&self.active_set), json);
            }
        }
    }

    fn save_jar(&self, set_name: &str) {
        if let Some(jar) = self.jars.get(set_name) {
            if let Ok(json) = serde_json::to_string_pretty(jar) {
                let _ = fs::create_dir_all(config::cookies_dir());
                let _ = fs::write(config::cookie_jar_path(set_name), json);
            }
        }
    }

    /// Move cookies for `host` (and any parent-domain entries up to the
    /// public suffix) from `source_set` to `target_set`. Used when the
    /// user moves a tab between sets so the site's login state follows.
    /// The first label is preserved (i.e. we don't move plain `com`).
    pub fn move_cookies_for_host(&mut self, host: &str, source_set: &str, target_set: &str) {
        if source_set == target_set { return; }
        if !self.jars.contains_key(source_set) {
            self.jars.insert(source_set.to_string(), Self::load_jar(source_set));
        }
        if !self.jars.contains_key(target_set) {
            self.jars.insert(target_set.to_string(), Self::load_jar(target_set));
        }

        let mut hosts_to_move: Vec<String> = Vec::new();
        if let Some(src) = self.jars.get(source_set) {
            let mut current: &str = host;
            loop {
                if src.contains_key(current) { hosts_to_move.push(current.to_string()); }
                match current.find('.') {
                    Some(i) if current[i + 1..].contains('.') => current = &current[i + 1..],
                    _ => break,
                }
            }
        }
        if hosts_to_move.is_empty() { return; }

        let mut moved: Vec<(String, HashMap<String, String>)> = Vec::new();
        if let Some(src) = self.jars.get_mut(source_set) {
            for h in &hosts_to_move {
                if let Some(c) = src.remove(h) { moved.push((h.clone(), c)); }
            }
        }
        if let Some(dst) = self.jars.get_mut(target_set) {
            for (h, c) in moved {
                dst.entry(h).or_default().extend(c);
            }
        }
        self.save_jar(source_set);
        self.save_jar(target_set);
    }

    pub fn fetch(&mut self, url: &str, method: &str, params: Option<&HashMap<String, String>>) -> FetchResult {
        // Check cache for GET without params (O(1) HashMap lookup)
        if method == "GET" && params.is_none() {
            if let Some(cached) = self.cache.get(url) {
                return cached.clone();
            }
        }

        // Handle file:// URLs
        if url.starts_with("file://") {
            let path = &url[7..];
            match fs::read_to_string(path) {
                Ok(body) => return FetchResult {
                    body,
                    content_type: guess_content_type(path),
                    url: url.to_string(),
                    status: 200,
                },
                Err(e) => return FetchResult {
                    body: format!("Error reading file: {}", e),
                    content_type: "text/plain".into(),
                    url: url.to_string(),
                    status: 404,
                },
            }
        }

        // Normalize URL
        let fetch_url = if !url.starts_with("http://") && !url.starts_with("https://") {
            format!("https://{}", url)
        } else {
            url.to_string()
        };

        // Build cookies up-front so the async block can be a pure send.
        // Walks the subdomain chain (accounts.google.com → google.com)
        // so cookies stored on `google.com` match a request to
        // `accounts.google.com`. Required for Firefox-imported cookies
        // which use bare-domain (leading-dot stripped on import).
        let cookie_str = self.build_cookie_header(&fetch_url);
        let m = if method == "POST" { Method::POST } else { Method::GET };

        let result: Result<rquest::Response, _> = self.runtime.block_on(async {
            let mut req = self.client.request(m, &fetch_url);
            if !cookie_str.is_empty() {
                req = req.header("Cookie", cookie_str);
            }
            if method == "POST" {
                if let Some(p) = params {
                    req.form(p).send().await
                } else {
                    req.send().await
                }
            } else {
                req.send().await
            }
        });

        match result {
            Ok(resp) => {
                let final_url = resp.url().to_string();
                let status = resp.status().as_u16();
                let ct = resp.headers()
                    .get(rquest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("text/html")
                    .to_string();
                let set_cookies: Vec<String> = resp.headers()
                    .get_all(rquest::header::SET_COOKIE)
                    .iter()
                    .filter_map(|v| v.to_str().ok().map(|s| s.to_string()))
                    .collect();

                let body = self.runtime.block_on(async {
                    resp.text().await.unwrap_or_default()
                });

                self.store_set_cookies(&final_url, &set_cookies);

                let result = FetchResult {
                    body,
                    content_type: ct,
                    url: final_url,
                    status,
                };

                // Cache GET results
                if method == "GET" && params.is_none() && status == 200 {
                    let key = url.to_string();
                    if !self.cache.contains_key(&key) {
                        self.cache_order.push(key.clone());
                    }
                    self.cache.insert(key, result.clone());
                    while self.cache.len() > self.max_cache {
                        if let Some(oldest) = self.cache_order.first().cloned() {
                            self.cache.remove(&oldest);
                            self.cache_order.remove(0);
                        } else { break; }
                    }
                }

                result
            }
            Err(e) => FetchResult {
                body: format!("Error: {}", e),
                content_type: "text/plain".into(),
                url: fetch_url,
                status: 0,
            },
        }
    }

    /// Fetch binary data (for images) - returns raw bytes.
    /// Uses the same Firefox-impersonating client as `fetch`, so
    /// `cf_clearance`-gated CDNs (e.g. github avatars) keep working.
    pub fn fetch_bytes(&self, url: &str) -> Option<Vec<u8>> {
        let fetch_url = if !url.starts_with("http://") && !url.starts_with("https://") {
            format!("https://{}", url)
        } else {
            url.to_string()
        };
        self.runtime.block_on(async {
            let resp = self.client.get(&fetch_url).send().await.ok()?;
            let bytes = resp.bytes().await.ok()?;
            if bytes.is_empty() { None } else { Some(bytes.to_vec()) }
        })
    }

    pub fn invalidate_cache(&mut self, url: &str) {
        self.cache.remove(url);
        self.cache_order.retain(|u| u != url);
    }

    /// Walk the subdomain chain to build a single `Cookie:` header for
    /// `url`, drawing from the active set's jar.
    fn build_cookie_header(&self, url: &str) -> String {
        let parsed = match url::Url::parse(url) {
            Ok(p) => p,
            Err(_) => return String::new(),
        };
        let host = match parsed.host_str() {
            Some(h) => h,
            None => return String::new(),
        };
        let jar = match self.jars.get(&self.active_set) {
            Some(j) => j,
            None => return String::new(),
        };
        let mut combined: HashMap<String, String> = HashMap::new();
        let mut current: &str = host;
        loop {
            if let Some(domain_cookies) = jar.get(current) {
                for (k, v) in domain_cookies {
                    combined.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
            match current.find('.') {
                Some(dot) => current = &current[dot + 1..],
                None => break,
            }
            if current.is_empty() || !current.contains('.') {
                if let Some(domain_cookies) = jar.get(current) {
                    for (k, v) in domain_cookies {
                        combined.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
                break;
            }
        }
        if combined.is_empty() { return String::new(); }
        combined.iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Store every `Set-Cookie` from a response. Unlike ureq, rquest
    /// surfaces all of them — so multi-cookie responses (every login
    /// flow under the sun) are now actually persisted.
    fn store_set_cookies(&mut self, url: &str, set_cookies: &[String]) {
        if set_cookies.is_empty() { return; }
        if let Ok(parsed) = url::Url::parse(url) {
            if let Some(domain) = parsed.host_str() {
                let active = self.active_set.clone();
                let jar = self.jars.entry(active).or_default();
                let entry = jar.entry(domain.to_string()).or_default();
                for sc in set_cookies {
                    let pair = sc.split(';').next().unwrap_or("").trim();
                    if let Some((name, value)) = pair.split_once('=') {
                        entry.insert(name.trim().to_string(), value.trim().to_string());
                    }
                }
                self.save_active_jar();
            }
        }
    }
}

fn guess_content_type(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext.to_lowercase().as_str() {
        "html" | "htm" => "text/html",
        "txt" => "text/plain",
        "json" => "application/json",
        "xml" => "application/xml",
        "css" => "text/css",
        "js" => "application/javascript",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        _ => "text/html",
    }.to_string()
}
