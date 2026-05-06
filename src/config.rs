use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

fn scroll_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".scroll")
}

pub fn config_path() -> PathBuf { scroll_dir().join("config.json") }
pub fn bookmarks_path() -> PathBuf { scroll_dir().join("bookmarks.json") }
pub fn quickmarks_path() -> PathBuf { scroll_dir().join("quickmarks.json") }
pub fn passwords_path() -> PathBuf { scroll_dir().join("passwords.json") }
pub fn cookies_path() -> PathBuf { scroll_dir().join("cookies.json") }
pub fn cookies_dir() -> PathBuf { scroll_dir().join("cookies") }
pub fn adblock_path() -> PathBuf { scroll_dir().join("adblock.txt") }

/// Per-set cookie-jar file path. Sanitises the set name so a stray
/// `/` or `..` can't escape the cookies dir.
pub fn cookie_jar_path(set_name: &str) -> PathBuf {
    let safe: String = set_name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let safe = if safe.is_empty() || safe.starts_with('.') {
        format!("_{}", safe)
    } else {
        safe
    };
    cookies_dir().join(format!("{}.json", safe))
}

pub fn ensure_dirs() {
    let _ = fs::create_dir_all(scroll_dir());
    let _ = fs::create_dir_all(cookies_dir());
}

/// Per-set, per-origin localStorage on disk. JS that stores tokens
/// in localStorage now persists across scroll runs the way a real
/// browser does.
pub fn localstorage_path(set_name: &str, host: &str) -> PathBuf {
    let safe_set: String = set_name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let safe_host: String = host.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    scroll_dir().join("localstorage").join(safe_set).join(format!("{}.json", safe_host))
}

pub fn load_localstorage(set_name: &str, host: &str) -> HashMap<String, String> {
    let p = localstorage_path(set_name, host);
    fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn session_path() -> PathBuf { scroll_dir().join("session.json") }

#[derive(Serialize, Deserialize, Clone)]
pub struct TabSnapshot {
    pub url: String,
    pub set: usize,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Session {
    pub current_tab: usize,
    pub current_set: usize,
    pub tabs: Vec<TabSnapshot>,
}

pub fn load_session() -> Option<Session> {
    fs::read_to_string(session_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

pub fn save_session(session: &Session) {
    if let Ok(json) = serde_json::to_string_pretty(session) {
        let _ = fs::write(session_path(), json);
    }
}

pub fn save_localstorage(set_name: &str, host: &str, data: &HashMap<String, String>) {
    let p = localstorage_path(set_name, host);
    if let Some(parent) = p.parent() { let _ = fs::create_dir_all(parent); }
    if data.is_empty() {
        // Empty store → remove the file rather than leave a {} shell.
        let _ = fs::remove_file(&p);
    } else if let Ok(json) = serde_json::to_string_pretty(data) {
        let _ = fs::write(&p, json);
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    #[serde(default = "default_homepage")]
    pub homepage: String,
    #[serde(default = "default_search_engine")]
    pub search_engine: String,
    #[serde(default = "default_download_folder")]
    pub download_folder: String,
    #[serde(default = "default_true")]
    pub match_site_colors: bool,
    #[serde(default = "default_image_mode")]
    pub image_mode: String,
    #[serde(default = "default_true")]
    pub show_images: bool,
    // Colors
    #[serde(default = "default_info_fg")]
    pub c_info_fg: u16,
    #[serde(default = "default_info_bg")]
    pub c_info_bg: u16,
    #[serde(default = "default_tab_fg")]
    pub c_tab_fg: u16,
    #[serde(default = "default_tab_bg")]
    pub c_tab_bg: u16,
    #[serde(default = "default_active_tab")]
    pub c_active_tab: u16,
    #[serde(default = "default_content_fg")]
    pub c_content_fg: u16,
    #[serde(default = "default_content_bg")]
    pub c_content_bg: u16,
    #[serde(default = "default_status_fg")]
    pub c_status_fg: u16,
    #[serde(default = "default_status_bg")]
    pub c_status_bg: u16,
    #[serde(default = "default_link_color")]
    pub c_link: u16,
    #[serde(default = "default_link_num")]
    pub c_link_num: u16,
    #[serde(default = "default_h1")]
    pub c_h1: u16,
    #[serde(default = "default_h2")]
    pub c_h2: u16,
    #[serde(default = "default_h3")]
    pub c_h3: u16,
    // AI
    #[serde(default)]
    pub ai_key: String,
    /// Optional external password command. Called as `<cmd> <host>`;
    /// must print exactly two lines on stdout: `username\npassword\n`.
    /// Empty string = disabled (use built-in `passwords.json` only).
    /// Example wrapper for the user's HyperList password file:
    ///   `~/bin/scroll-pass` reads `/home/.safe/.p.hl`, looks up
    ///   `$1`, emits `user\npass`.
    #[serde(default)]
    pub password_command: String,
    /// Per-set foreground colors (256-color codes). Indexed by set
    /// position; if a set's index is past the end of this list, it
    /// falls back to `c_active_tab`. Default cycle is six distinct
    /// hues so 1–3 sets are visually unambiguous out of the box.
    #[serde(default = "default_set_colors")]
    pub set_colors: Vec<u16>,
    /// Map a set name to a Firefox profile name (or absolute profile
    /// directory path). On set switch, scroll imports that profile's
    /// `cookies.sqlite` into the active cookie jar so the user can
    /// be logged in to a site (Google, etc.) as different identities
    /// in different sets without scroll having a JS engine. Log in
    /// once via Firefox per profile; scroll inherits the cookies.
    /// Empty / missing entry = no import; jar stays scroll-managed.
    /// Examples:
    ///   "Personal"      → "default"
    ///   "Dualog"        → "scroll-dualog"
    ///   "PassionFruits" → "/home/geir/.mozilla/firefox/abc.passionfruits"
    #[serde(default)]
    pub firefox_profiles: HashMap<String, String>,
}

fn default_homepage() -> String { "about:home".into() }
fn default_search_engine() -> String { "g".into() }
fn default_download_folder() -> String {
    format!("{}/Downloads", std::env::var("HOME").unwrap_or_default())
}
fn default_true() -> bool { true }
fn default_image_mode() -> String { "auto".into() }
fn default_info_fg() -> u16 { 255 }
fn default_info_bg() -> u16 { 236 }
fn default_tab_fg() -> u16 { 252 }
fn default_tab_bg() -> u16 { 234 }
fn default_active_tab() -> u16 { 220 }
fn default_content_fg() -> u16 { 252 }
fn default_content_bg() -> u16 { 0 }
fn default_status_fg() -> u16 { 252 }
fn default_status_bg() -> u16 { 236 }
fn default_link_color() -> u16 { 81 }
fn default_link_num() -> u16 { 39 }
fn default_h1() -> u16 { 220 }
fn default_h2() -> u16 { 214 }
fn default_h3() -> u16 { 208 }
fn default_set_colors() -> Vec<u16> {
    // Match the user's rsh dir_colors palette so set identity is
    // consistent across shell + browser:
    //   Personal       = 172 (orange,  MakeItSimple in ~/.rshrc)
    //   PassionFruits  = 171 (magenta, PassionFruit in ~/.rshrc)
    //   Dualog         =  72 (teal,    Dualog in ~/.rshrc)
    // Trailing entries are extras for any user-added sets.
    vec![172, 171, 72, 220, 121, 217]
}

impl Default for Config {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl Config {
    pub fn load() -> Self {
        fs::read_to_string(config_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(config_path(), json);
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Bookmark {
    pub url: String,
    pub title: String,
}

pub fn load_bookmarks() -> Vec<Bookmark> {
    fs::read_to_string(bookmarks_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_bookmarks(bm: &[Bookmark]) {
    if let Ok(json) = serde_json::to_string_pretty(bm) {
        let _ = fs::write(bookmarks_path(), json);
    }
}

pub fn load_quickmarks() -> HashMap<String, (String, String)> {
    fs::read_to_string(quickmarks_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_quickmarks(qm: &HashMap<String, (String, String)>) {
    if let Ok(json) = serde_json::to_string_pretty(qm) {
        let _ = fs::write(quickmarks_path(), json);
    }
}

pub fn load_passwords() -> HashMap<String, (String, String)> {
    fs::read_to_string(passwords_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_passwords(pw: &HashMap<String, (String, String)>) {
    if let Ok(json) = serde_json::to_string_pretty(pw) {
        let path = passwords_path();
        let _ = fs::write(&path, json);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
    }
}

fn sets_path() -> PathBuf {
    scroll_dir().join("sets.json")
}

/// Load named tab sets. Defaults to ["Personal", "PassionFruits",
/// "Dualog"] on first run; the rcfile can be edited freely.
pub fn load_sets() -> Vec<String> {
    if let Ok(s) = fs::read_to_string(sets_path()) {
        if let Ok(v) = serde_json::from_str::<Vec<String>>(&s) {
            if !v.is_empty() { return v; }
        }
    }
    vec!["Personal".into(), "PassionFruits".into(), "Dualog".into()]
}

pub fn save_sets(sets: &[String]) {
    if let Ok(json) = serde_json::to_string_pretty(sets) {
        let _ = fs::write(sets_path(), json);
    }
}
