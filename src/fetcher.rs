use std::collections::HashMap;
use std::fs;
use std::io::Read;
use crate::config;

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
        Fetcher {
            jars,
            active_set: set_name.to_string(),
            cache: HashMap::new(),
            cache_order: Vec::new(),
            max_cache: 20,
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

    fn save_active_jar(&self) {
        if let Some(jar) = self.jars.get(&self.active_set) {
            if let Ok(json) = serde_json::to_string_pretty(jar) {
                let _ = fs::create_dir_all(config::cookies_dir());
                let _ = fs::write(config::cookie_jar_path(&self.active_set), json);
            }
        }
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

        // Build request
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(15))
            .timeout_read(std::time::Duration::from_secs(15))
            .redirects(10)
            .build();

        let mut req = if method == "POST" {
            agent.post(&fetch_url)
        } else {
            agent.get(&fetch_url)
        };

        req = req.set("User-Agent", "scroll/0.1 (terminal browser)")
            .set("Accept", "text/html,application/xhtml+xml,*/*")
            .set("Accept-Language", "en-US,en;q=0.9");

        // Add cookies from the active set's jar.
        if let Ok(parsed) = url::Url::parse(&fetch_url) {
            if let Some(domain) = parsed.host_str() {
                if let Some(jar) = self.jars.get(&self.active_set) {
                    if let Some(domain_cookies) = jar.get(domain) {
                        let cookie_str: String = domain_cookies.iter()
                            .map(|(k, v)| format!("{}={}", k, v))
                            .collect::<Vec<_>>()
                            .join("; ");
                        if !cookie_str.is_empty() {
                            req = req.set("Cookie", &cookie_str);
                        }
                    }
                }
            }
        }

        // Send request
        let result = if method == "POST" {
            if let Some(p) = params {
                let form: Vec<(&str, &str)> = p.iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                req.send_form(&form)
            } else {
                req.call()
            }
        } else {
            req.call()
        };

        match result {
            Ok(resp) => {
                let final_url = resp.get_url().to_string();
                let ct = resp.content_type().to_string();
                let status = resp.status();

                // Store cookies from response
                self.store_response_cookies(&final_url, &resp);

                let body = resp.into_string().unwrap_or_default();

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
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_else(|_| format!("HTTP Error {}", code));
                FetchResult {
                    body,
                    content_type: "text/html".into(),
                    url: fetch_url,
                    status: code,
                }
            }
            Err(e) => FetchResult {
                body: format!("Error: {}", e),
                content_type: "text/plain".into(),
                url: fetch_url,
                status: 0,
            },
        }
    }

    /// Fetch binary data (for images) - returns raw bytes
    pub fn fetch_bytes(&self, url: &str) -> Option<Vec<u8>> {
        let fetch_url = if !url.starts_with("http://") && !url.starts_with("https://") {
            format!("https://{}", url)
        } else {
            url.to_string()
        };

        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(10))
            .timeout_read(std::time::Duration::from_secs(10))
            .redirects(10)
            .build();

        let resp = agent.get(&fetch_url)
            .set("User-Agent", "scroll/0.1 (terminal browser)")
            .call()
            .ok()?;

        let mut bytes = Vec::new();
        resp.into_reader().read_to_end(&mut bytes).ok()?;
        if bytes.is_empty() { None } else { Some(bytes) }
    }

    pub fn invalidate_cache(&mut self, url: &str) {
        self.cache.remove(url);
        self.cache_order.retain(|u| u != url);
    }

    fn store_response_cookies(&mut self, url: &str, resp: &ureq::Response) {
        if let Ok(parsed) = url::Url::parse(url) {
            if let Some(domain) = parsed.host_str() {
                // ureq 2.x only exposes first Set-Cookie via header()
                if let Some(val) = resp.header("set-cookie") {
                    if let Some((name_val, _)) = val.split_once(';') {
                        if let Some((name, value)) = name_val.split_once('=') {
                            let active = self.active_set.clone();
                            let jar = self.jars.entry(active).or_default();
                            let entry = jar.entry(domain.to_string()).or_default();
                            entry.insert(name.trim().to_string(), value.trim().to_string());
                        }
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
