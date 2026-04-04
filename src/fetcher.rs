use std::collections::HashMap;
use std::fs;
use std::io::Read;
use crate::config;

pub struct Fetcher {
    cookies: HashMap<String, HashMap<String, String>>,
    cache: Vec<(String, FetchResult)>,
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
    pub fn new() -> Self {
        let cookies = config::cookies_path()
            .to_str()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Fetcher { cookies, cache: Vec::new(), max_cache: 20 }
    }

    pub fn fetch(&mut self, url: &str, method: &str, params: Option<&HashMap<String, String>>) -> FetchResult {
        // Check cache for GET without params
        if method == "GET" && params.is_none() {
            if let Some(cached) = self.cache.iter().find(|(u, _)| u == url) {
                return cached.1.clone();
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

        // Add cookies
        if let Ok(parsed) = url::Url::parse(&fetch_url) {
            if let Some(domain) = parsed.host_str() {
                if let Some(domain_cookies) = self.cookies.get(domain) {
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
                    self.cache.push((url.to_string(), result.clone()));
                    if self.cache.len() > self.max_cache {
                        self.cache.remove(0);
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
        self.cache.retain(|(u, _)| u != url);
    }

    fn store_response_cookies(&mut self, url: &str, resp: &ureq::Response) {
        if let Ok(parsed) = url::Url::parse(url) {
            if let Some(domain) = parsed.host_str() {
                // ureq doesn't expose Set-Cookie easily; use header iteration
                let mut idx = 0;
                loop {
                    let key = "set-cookie";
                    let has_cookie = resp.headers_names().iter()
                        .any(|h| h.eq_ignore_ascii_case(key));
                    if !has_cookie { break; }
                    if let Some(val) = resp.header(key) {
                        if let Some((name_val, _)) = val.split_once(';') {
                            if let Some((name, value)) = name_val.split_once('=') {
                                let entry = self.cookies.entry(domain.to_string()).or_default();
                                entry.insert(name.trim().to_string(), value.trim().to_string());
                            }
                        }
                    }
                    break; // ureq only returns first header with same name
                }
                self.save_cookies();
            }
        }
    }

    fn save_cookies(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.cookies) {
            let _ = fs::write(config::cookies_path(), json);
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
