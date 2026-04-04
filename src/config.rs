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
pub fn adblock_path() -> PathBuf { scroll_dir().join("adblock.txt") }

pub fn ensure_dirs() {
    let _ = fs::create_dir_all(scroll_dir());
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
