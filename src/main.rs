mod config;
mod fetcher;
mod renderer;
mod tab;

use crust::{Crust, Pane, Input};
use crust::style;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use config::Config;
use fetcher::Fetcher;
use tab::Tab;

/// Shared state for async image downloads
struct ImgDownloadState {
    pending: Vec<(String, String)>,  // (url, cache_path) pairs to download
    ready: Vec<String>,              // cache paths that finished downloading
}

struct App {
    info: Pane,
    tab_bar: Pane,
    main: Pane,
    status: Pane,
    cols: u16,
    rows: u16,
    conf: Config,
    fetcher: Fetcher,
    tabs: Vec<Tab>,
    current_tab: usize,
    closed_tabs: Vec<Tab>,
    focus_index: i32,
    search_term: String,
    search_matches: Vec<usize>,
    search_index: usize,
    g_pressed: bool,
    h_scroll: usize,
    running: bool,
    bookmarks: Vec<config::Bookmark>,
    quickmarks: HashMap<String, (String, String)>,
    passwords: HashMap<String, (String, String)>,
    image_display: Option<glow::Display>,
    img_state: Arc<Mutex<ImgDownloadState>>,
    img_thread: Option<std::thread::JoinHandle<()>>,
}

fn main() {
    config::ensure_dirs();

    let initial_url = std::env::args().nth(1).unwrap_or_else(|| "about:home".into());

    Crust::init();
    let (cols, rows) = Crust::terminal_size();

    let conf = Config::load();
    let show_imgs = conf.show_images;
    let img_mode = conf.image_mode.clone();
    let main_h = rows.saturating_sub(3);

    let mut app = App {
        info: Pane::new(1, 1, cols, 1, conf.c_info_fg, conf.c_info_bg),
        tab_bar: Pane::new(1, 2, cols, 1, conf.c_tab_fg, conf.c_tab_bg),
        main: Pane::new(1, 3, cols, main_h, conf.c_content_fg, conf.c_content_bg),
        status: Pane::new(1, rows, cols, 1, conf.c_status_fg, conf.c_status_bg),
        cols,
        rows,
        conf,
        fetcher: Fetcher::new(),
        tabs: Vec::new(),
        current_tab: 0,
        closed_tabs: Vec::new(),
        focus_index: -1,
        search_term: String::new(),
        search_matches: Vec::new(),
        search_index: 0,
        g_pressed: false,
        h_scroll: 0,
        running: true,
        bookmarks: config::load_bookmarks(),
        quickmarks: config::load_quickmarks(),
        passwords: config::load_passwords(),
        image_display: if show_imgs {
            Some(glow::Display::with_mode(&img_mode))
        } else { None },
        img_state: Arc::new(Mutex::new(ImgDownloadState { pending: Vec::new(), ready: Vec::new() })),
        img_thread: None,
    };

    app.main.scroll = true;

    // Create initial tab and navigate
    app.tabs.push(Tab::new("about:blank"));
    app.navigate(&initial_url);
    app.render_all();

    while app.running {
        // Check for newly downloaded images on each loop iteration
        app.check_new_images();

        let Some(key) = Input::getchr(Some(1)) else {
            // Timeout: check images again, continue
            app.check_new_images();
            continue;
        };

        if app.g_pressed {
            app.g_pressed = false;
            if key == "g" {
                app.main.ix = 0;
                app.render_main();
                continue;
            }
        }

        match key.as_str() {
            // Scrolling
            "j" | "DOWN" => { app.scroll_down(1); }
            "k" | "UP" => { app.scroll_up(1); }
            " " | "PgDOWN" => { app.page_down(); }
            "PgUP" => { app.page_up(); }
            "g" => { app.g_pressed = true; }
            "G" | "END" => { app.scroll_bottom(); }
            "HOME" => { app.main.ix = 0; app.render_main(); }
            "C-D" => { app.scroll_down(app.rows as usize / 2); }
            "C-U" => { app.scroll_up(app.rows as usize / 2); }
            "<" => { if app.h_scroll >= 10 { app.h_scroll -= 10; } else { app.h_scroll = 0; } app.render_main(); }
            ">" => { app.h_scroll += 10; app.render_main(); }

            // Tab management
            "J" | "RIGHT" => { app.next_tab(); }
            "K" | "LEFT" => { app.prev_tab(); }
            "d" => { app.close_tab(); }
            "u" => { app.undo_close_tab(); }

            // Navigation
            "o" => { app.open_url_prompt(); }
            "O" => { app.edit_url_prompt(); }
            "t" => { app.open_in_new_tab(); }
            "H" | "BACK" => { app.go_back(); }
            "L" | "DEL" => { app.go_forward(); }
            "r" => { app.reload(); }

            // Links & forms
            "TAB" => { app.focus_next(); }
            "S-TAB" => { app.focus_prev(); }
            "ENTER" => { app.follow_focused(); }
            "f" => { app.fill_form(); }

            // Search
            "/" => { app.search_prompt(); }
            "n" => { app.search_next(); }
            "N" => { app.search_prev(); }

            // Bookmarks & quickmarks
            "b" => { app.bookmark_current(); }
            "B" => { app.show_bookmarks(); }
            "m" => { app.set_quickmark(); }
            "'" => { app.goto_quickmark(); }

            // Clipboard
            "y" => { app.copy_url(); }
            "Y" => { app.copy_focused_url(); }

            // Images
            "i" => { app.toggle_images(); }

            // Preferences & help
            "P" => { app.show_preferences(); }
            "?" => { app.show_help(); }
            "I" => { app.ai_summary(); }
            "C-L" => { app.force_redraw(); }

            // Commands
            ":" => { app.command_mode(); }

            // Quit
            "q" => { app.running = false; }

            // Resize
            "RESIZE" => { app.handle_resize(); }

            _ => {}
        }
    }

    Crust::cleanup();
}

impl App {
    fn tab(&self) -> &Tab { &self.tabs[self.current_tab] }
    fn tab_mut(&mut self) -> &mut Tab { &mut self.tabs[self.current_tab] }

    // --- Rendering ---

    fn render_all(&mut self) {
        self.render_info();
        self.render_tabs();
        self.render_main();
        self.render_status();
    }

    fn render_info(&mut self) {
        let tab = &self.tabs[self.current_tab];
        let back = if tab.can_go_back() { "\u{25C0} " } else { "" };
        let fwd = if tab.can_go_forward() { " \u{25B6}" } else { "" };
        let title = if tab.title.is_empty() { &tab.url } else { &tab.title };
        self.info.say(&format!(" {}{}{}", back, title, fwd));
    }

    fn render_tabs(&mut self) {
        if self.tabs.len() <= 1 {
            self.tab_bar.say("");
            return;
        }
        let parts: Vec<String> = self.tabs.iter().enumerate().map(|(i, t)| {
            let label = if t.title.is_empty() {
                t.url.chars().take(20).collect::<String>()
            } else {
                t.title.chars().take(20).collect::<String>()
            };
            if i == self.current_tab {
                style::fg(&format!(" {} ", label), self.conf.c_active_tab as u8)
            } else {
                format!(" {} ", label)
            }
        }).collect();
        self.tab_bar.say(&parts.join("\u{2502}"));
    }

    fn render_main(&mut self) {
        let tab = &self.tabs[self.current_tab];

        // Apply site colors if enabled
        if self.conf.match_site_colors {
            if let Some(bg) = tab.site_bg {
                self.main.bg = bg as u16;
            } else {
                self.main.bg = self.conf.c_content_bg;
            }
            if let Some(fg) = tab.site_fg {
                self.main.fg = fg as u16;
            } else {
                self.main.fg = self.conf.c_content_fg;
            }
        }

        self.main.set_text(&tab.content);
        self.main.ix = tab.ix;
        self.main.full_refresh();

        // Show images in viewport
        if self.conf.show_images {
            self.show_visible_images();
        }
    }

    /// Start async download of all images on the page
    fn start_image_downloads(&mut self) {
        let images = self.tabs[self.current_tab].images.clone();
        if images.is_empty() { return; }

        // Queue all images for download
        let mut pending = Vec::new();
        for img in &images {
            let cache_path = img_cache_path(&img.src);
            if !std::path::Path::new(&cache_path).exists() {
                pending.push((img.src.clone(), cache_path));
            }
        }
        if pending.is_empty() { return; }

        {
            let mut state = self.img_state.lock().unwrap();
            state.pending = pending.clone();
            state.ready.clear();
        }

        // Spawn background thread to download all images
        let state = self.img_state.clone();
        self.img_thread = Some(std::thread::spawn(move || {
            let agent = ureq::AgentBuilder::new()
                .timeout_connect(std::time::Duration::from_secs(10))
                .timeout_read(std::time::Duration::from_secs(10))
                .redirects(10)
                .build();

            for (url, cache_path) in &pending {
                if std::path::Path::new(cache_path).exists() { continue; }
                let resp = agent.get(url)
                    .set("User-Agent", "scroll/0.1")
                    .call();
                if let Ok(resp) = resp {
                    let mut bytes = Vec::new();
                    if std::io::Read::read_to_end(&mut resp.into_reader(), &mut bytes).is_ok() && !bytes.is_empty() {
                        let _ = std::fs::write(cache_path, &bytes);
                        let mut s = state.lock().unwrap();
                        s.ready.push(cache_path.clone());
                    }
                }
            }
        }));
    }

    /// Show images that are in viewport AND already cached locally
    fn show_visible_images(&mut self) {
        let Some(ref mut display) = self.image_display else { return };
        if !display.supported() { return; }

        let viewport_top = self.tabs[self.current_tab].ix;
        let viewport_h = self.main.h as usize;
        let viewport_bottom = viewport_top + viewport_h;
        let images = self.tabs[self.current_tab].images.clone();

        for img in &images {
            if img.line + img.height <= viewport_top || img.line >= viewport_bottom {
                continue;
            }

            let cache_path = img_cache_path(&img.src);
            if std::path::Path::new(&cache_path).exists() {
                let y_offset = img.line.saturating_sub(viewport_top) as u16;
                let display_y = self.main.y + y_offset;
                let display_h = (img.height as u16).min(self.main.h.saturating_sub(y_offset));
                let img_w = (self.main.w / 3).max(30).min(80);
                display.show(&cache_path, self.main.x, display_y, img_w, display_h);
            }
        }
    }

    /// Check if any new images finished downloading, show them if in viewport
    fn check_new_images(&mut self) {
        let has_new = {
            let state = self.img_state.lock().unwrap();
            !state.ready.is_empty()
        };
        if has_new {
            let mut state = self.img_state.lock().unwrap();
            state.ready.clear();
            drop(state);
            self.show_visible_images();
        }
    }

    fn clear_images(&mut self) {
        if let Some(ref mut display) = self.image_display {
            display.clear(self.main.x, self.main.y, self.main.w, self.main.h, self.cols, self.rows);
        }
    }

    fn render_status(&mut self) {
        let tab = &self.tabs[self.current_tab];
        let n_links = tab.links.len();
        let msg = if n_links > 0 {
            format!(" {} links | {}", n_links, tab.url)
        } else {
            format!(" {}", tab.url)
        };
        self.status.say(&msg);
    }

    // --- Navigation ---

    fn navigate(&mut self, url: &str) {
        self.clear_images();
        let resolved = resolve_search(url, &self.conf.search_engine);

        // Handle about: URLs
        if resolved == "about:blank" {
            self.tab_mut().navigate(&resolved);
            self.tab_mut().content = String::new();
            self.tab_mut().title = "New Tab".into();
            self.render_all();
            return;
        }
        if resolved == "about:home" {
            self.tab_mut().navigate(&resolved);
            self.tab_mut().content = format!(
                "{}\n\n{}\n\n{}\n{}\n{}\n",
                style::bold(&style::fg("scroll", 81)),
                "Terminal web browser",
                style::fg("o", 220).to_string() + " Open URL   " + &style::fg("t", 220) + " New tab   " + &style::fg("?", 220) + " Help",
                style::fg("b", 220).to_string() + " Bookmark   " + &style::fg("B", 220) + " Bookmarks  " + &style::fg("q", 220) + " Quit",
                style::fg(":", 220).to_string() + " Command    " + &style::fg("P", 220) + " Preferences",
            );
            self.tab_mut().title = "Home".into();
            self.render_all();
            return;
        }

        self.status.say(&format!(" Loading {}...", &resolved));

        self.tab_mut().navigate(&resolved);
        let result = self.fetcher.fetch(&resolved, "GET", None);

        if result.content_type.starts_with("image/") {
            // Image URL: download as binary and display
            let cache_path = img_cache_path(&result.url);
            if !std::path::Path::new(&cache_path).exists() {
                if let Some(bytes) = self.fetcher.fetch_bytes(&result.url) {
                    let _ = std::fs::write(&cache_path, &bytes);
                }
            }
            let filename = result.url.rsplit('/').next().unwrap_or("image");
            self.tab_mut().content = format!("\n\n{}\n\n{}",
                crust::style::fg(filename, 81),
                crust::style::fg(&result.url, 245));
            self.tab_mut().title = filename.to_string();
            let url_clone = result.url.clone();
            self.tab_mut().url = url_clone.clone();
            // Add as image at line 0 so glow displays it
            self.tab_mut().images = vec![crate::tab::ImageRef {
                src: url_clone, alt: filename.to_string(), line: 0, height: 10,
            }];
        } else if result.content_type.starts_with("text/html") || result.content_type.contains("html") {
            let width = self.main.w as usize;
            let rendered = renderer::render_html(&result.body, width, &result.url, &self.conf);
            self.tab_mut().content = rendered.text;
            self.tab_mut().title = rendered.title;
            self.tab_mut().links = rendered.links;
            self.tab_mut().forms = rendered.forms;
            self.tab_mut().images = rendered.images;
            self.tab_mut().url = result.url;
            self.tab_mut().site_bg = rendered.site_bg;
            self.tab_mut().site_fg = rendered.site_fg;
        } else if result.content_type.starts_with("application/pdf")
            || result.content_type.starts_with("application/zip")
            || result.content_type.starts_with("audio/")
            || result.content_type.starts_with("video/") {
            // Binary files: offer to download
            let filename = resolved.rsplit('/').next().unwrap_or("file");
            self.tab_mut().content = format!("\n{}\n\nType: {}\n\nPress ':download {}' to save",
                crust::style::bold(filename),
                result.content_type,
                resolved);
            self.tab_mut().title = filename.to_string();
        } else {
            self.tab_mut().content = result.body;
            self.tab_mut().title = resolved.clone();
        }

        self.focus_index = -1;
        self.search_term.clear();
        self.search_matches.clear();
        self.render_all();
        // Start async image downloads for the page
        self.start_image_downloads();
    }

    fn go_back(&mut self) {
        if let Some(url) = self.tab_mut().go_back() {
            let result = self.fetcher.fetch(&url, "GET", None);
            self.load_result(result);
        }
    }

    fn go_forward(&mut self) {
        if let Some(url) = self.tab_mut().go_forward() {
            let result = self.fetcher.fetch(&url, "GET", None);
            self.load_result(result);
        }
    }

    fn reload(&mut self) {
        let url = self.tab().url.clone();
        self.fetcher.invalidate_cache(&url);
        self.navigate(&url);
    }

    fn load_result(&mut self, result: fetcher::FetchResult) {
        let width = self.main.w as usize;
        if result.content_type.starts_with("text/html") || result.content_type.contains("html") {
            let rendered = renderer::render_html(&result.body, width, &result.url, &self.conf);
            self.tab_mut().content = rendered.text;
            self.tab_mut().title = rendered.title;
            self.tab_mut().links = rendered.links;
            self.tab_mut().forms = rendered.forms;
            self.tab_mut().images = rendered.images;
            self.tab_mut().site_bg = rendered.site_bg;
            self.tab_mut().site_fg = rendered.site_fg;
        } else {
            self.tab_mut().content = result.body;
        }
        self.tab_mut().url = result.url;
        self.focus_index = -1;
        self.render_all();
        self.start_image_downloads();
    }

    // --- Scrolling ---

    fn scroll_down(&mut self, n: usize) {
        self.clear_images();
        self.tabs[self.current_tab].ix += n;
        self.main.ix = self.tabs[self.current_tab].ix;
        self.main.full_refresh();
        if self.conf.show_images { self.show_visible_images(); }
    }

    fn scroll_up(&mut self, n: usize) {
        self.clear_images();
        self.tabs[self.current_tab].ix = self.tabs[self.current_tab].ix.saturating_sub(n);
        self.main.ix = self.tabs[self.current_tab].ix;
        self.main.full_refresh();
        if self.conf.show_images { self.show_visible_images(); }
    }

    fn page_down(&mut self) {
        let page = self.main.h as usize;
        self.scroll_down(page.saturating_sub(2));
    }

    fn page_up(&mut self) {
        let page = self.main.h as usize;
        self.scroll_up(page.saturating_sub(2));
    }

    fn scroll_bottom(&mut self) {
        self.clear_images();
        let lc = self.tab().content.lines().count();
        let page = self.main.h as usize;
        self.tabs[self.current_tab].ix = lc.saturating_sub(page);
        self.main.ix = self.tabs[self.current_tab].ix;
        self.main.full_refresh();
        if self.conf.show_images { self.show_visible_images(); }
    }

    // --- Tabs ---

    fn next_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.current_tab = (self.current_tab + 1) % self.tabs.len();
            self.render_all();
        }
    }

    fn prev_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.current_tab = if self.current_tab == 0 { self.tabs.len() - 1 } else { self.current_tab - 1 };
            self.render_all();
        }
    }

    fn close_tab(&mut self) {
        if self.tabs.len() <= 1 {
            self.running = false;
            return;
        }
        let closed = self.tabs.remove(self.current_tab);
        self.closed_tabs.push(closed);
        if self.current_tab >= self.tabs.len() {
            self.current_tab = self.tabs.len() - 1;
        }
        self.render_all();
    }

    fn undo_close_tab(&mut self) {
        if let Some(tab) = self.closed_tabs.pop() {
            self.tabs.insert(self.current_tab + 1, tab);
            self.current_tab += 1;
            self.render_all();
        }
    }

    // --- URL prompts ---

    fn open_url_prompt(&mut self) {
        let url = self.status.ask_with_bg("Open: ", "", 18);
        if !url.is_empty() {
            self.navigate(&url);
        }
    }

    fn edit_url_prompt(&mut self) {
        let current = self.tab().url.clone();
        let url = self.status.ask_with_bg("Open: ", &current, 18);
        if !url.is_empty() {
            self.navigate(&url);
        }
    }

    fn open_in_new_tab(&mut self) {
        let url = self.status.ask_with_bg("Tab open: ", "", 18);
        if !url.is_empty() {
            self.tabs.push(Tab::new("about:blank"));
            self.current_tab = self.tabs.len() - 1;
            self.navigate(&url);
        }
    }

    // --- Focus / Links / Forms ---

    fn focus_next(&mut self) {
        let n_links = self.tabs[self.current_tab].links.len();
        if n_links == 0 { return; }
        // Only cycle through links (not form fields for simplicity)
        self.focus_index = ((self.focus_index + 1) as usize % n_links) as i32;
        self.scroll_to_focused();
    }

    fn focus_prev(&mut self) {
        let n_links = self.tabs[self.current_tab].links.len();
        if n_links == 0 { return; }
        self.focus_index = if self.focus_index <= 0 { n_links as i32 - 1 } else { self.focus_index - 1 };
        self.scroll_to_focused();
    }

    fn scroll_to_focused(&mut self) {
        let idx = self.focus_index as usize;
        let n_links = self.tabs[self.current_tab].links.len();
        if idx >= n_links { return; }

        let line = self.tabs[self.current_tab].links[idx].line;
        let link_idx = self.tabs[self.current_tab].links[idx].index;
        let link_text = self.tabs[self.current_tab].links[idx].text.clone();
        let href = self.tabs[self.current_tab].links[idx].href.clone();

        // Highlight the focused link on the page
        let content = self.tabs[self.current_tab].content.clone();
        let links = self.tabs[self.current_tab].links.clone();
        let highlighted = renderer::highlight_link(&content, &links, idx);

        self.clear_images();
        self.tabs[self.current_tab].ix = line.saturating_sub(3);
        self.main.set_text(&highlighted);
        self.main.ix = self.tabs[self.current_tab].ix;
        self.main.full_refresh();
        if self.conf.show_images { self.show_visible_images(); }

        // Show focused link info in status bar
        self.status.say(&format!(" {} {} {}",
            style::fg(&format!("[{}]", link_idx), self.conf.c_link_num as u8),
            style::reverse(&link_text),
            style::fg(&href, 245)));
    }

    /// ENTER: if focused on a link, follow it. Otherwise prompt for link number.
    fn follow_focused(&mut self) {
        if self.focus_index >= 0 {
            let idx = self.focus_index as usize;
            let n_links = self.tabs[self.current_tab].links.len();
            if idx < n_links {
                let href = self.tabs[self.current_tab].links[idx].href.clone();
                self.navigate(&href);
                return;
            }
        }
        // Prompt for link number
        let input = self.status.ask_with_bg("Link #: ", "", 18);
        if input.is_empty() { return; }
        if let Ok(num) = input.parse::<usize>() {
            if let Some(link) = self.tabs[self.current_tab].links.iter().find(|l| l.index == num) {
                let href = link.href.clone();
                self.navigate(&href);
            } else {
                self.status.say(&style::fg(&format!(" Link {} not found", num), 196));
            }
        }
    }

    fn fill_form(&mut self) {
        if self.tab().forms.is_empty() { return; }
        let form = self.tab().forms[0].clone();
        let mut params = HashMap::new();

        for field in &form.fields {
            match field.field_type.as_str() {
                "hidden" => { params.insert(field.name.clone(), field.value.clone()); }
                "password" => {
                    let val = self.status.ask_with_bg(&format!("{}: ", field.name), "", 18);
                    params.insert(field.name.clone(), val);
                }
                "select" => {
                    let options: Vec<String> = field.options.iter().map(|(_, l)| l.clone()).collect();
                    let val = self.status.ask_with_bg(&format!("{} ({}): ", field.name, options.join("/")), "", 18);
                    params.insert(field.name.clone(), val);
                }
                _ => {
                    let domain = url::Url::parse(&self.tab().url).ok()
                        .and_then(|u| u.host_str().map(|h| h.to_string()));
                    let autofill = domain.as_ref()
                        .and_then(|d| self.passwords.get(d))
                        .filter(|_| field.name.contains("user") || field.name.contains("email") || field.name.contains("login"))
                        .map(|(u, _)| u.clone());
                    let default = autofill.unwrap_or_else(|| field.value.clone());
                    let val = self.status.ask_with_bg(&format!("{}: ", field.placeholder), &default, 18);
                    params.insert(field.name.clone(), val);
                }
            }
        }

        let method = form.method.to_uppercase();
        let url = if method == "POST" {
            form.action.clone()
        } else {
            let qs: String = params.iter()
                .map(|(k, v)| format!("{}={}", k, urlencoding(v)))
                .collect::<Vec<_>>()
                .join("&");
            if form.action.contains('?') {
                format!("{}&{}", form.action, qs)
            } else {
                format!("{}?{}", form.action, qs)
            }
        };

        self.tab_mut().navigate(&url);
        let result = if method == "POST" {
            self.fetcher.fetch(&form.action, "POST", Some(&params))
        } else {
            self.fetcher.fetch(&url, "GET", None)
        };
        self.load_result(result);
    }

    // --- Search ---

    fn search_prompt(&mut self) {
        let term = self.status.ask_with_bg("/", "", 18);
        if term.is_empty() { return; }
        self.search_term = term.to_lowercase();
        self.search_matches.clear();
        let content = self.tabs[self.current_tab].content.clone();
        for (i, line) in content.lines().enumerate() {
            let plain = crust::strip_ansi(line);
            if plain.to_lowercase().contains(&self.search_term) {
                self.search_matches.push(i);
            }
        }
        self.search_index = 0;
        if !self.search_matches.is_empty() {
            self.tabs[self.current_tab].ix = self.search_matches[0].saturating_sub(3);
            self.main.ix = self.tabs[self.current_tab].ix;
            self.main.refresh();
            self.status.say(&format!(" Match 1/{}", self.search_matches.len()));
        } else {
            self.status.say(" No matches");
        }
    }

    fn search_next(&mut self) {
        if self.search_matches.is_empty() { return; }
        self.search_index = (self.search_index + 1) % self.search_matches.len();
        self.tabs[self.current_tab].ix = self.search_matches[self.search_index].saturating_sub(3);
        self.main.ix = self.tabs[self.current_tab].ix;
        self.main.refresh();
        self.status.say(&format!(" Match {}/{}", self.search_index + 1, self.search_matches.len()));
    }

    fn search_prev(&mut self) {
        if self.search_matches.is_empty() { return; }
        self.search_index = if self.search_index == 0 { self.search_matches.len() - 1 } else { self.search_index - 1 };
        self.tabs[self.current_tab].ix = self.search_matches[self.search_index].saturating_sub(3);
        self.main.ix = self.tabs[self.current_tab].ix;
        self.main.refresh();
        self.status.say(&format!(" Match {}/{}", self.search_index + 1, self.search_matches.len()));
    }

    // --- Bookmarks ---

    fn bookmark_current(&mut self) {
        let url = self.tab().url.clone();
        let title = self.tab().title.clone();
        self.bookmarks.push(config::Bookmark { url, title: title.clone() });
        config::save_bookmarks(&self.bookmarks);
        self.status.say(&style::fg(&format!(" Bookmarked: {}", title), 82));
    }

    fn show_bookmarks(&mut self) {
        if self.bookmarks.is_empty() {
            self.status.say(" No bookmarks");
            return;
        }
        let mut lines = vec![style::bold("Bookmarks"), String::new()];
        for (i, bm) in self.bookmarks.iter().enumerate() {
            lines.push(format!("  {} {} {}",
                style::fg(&format!("{:2}", i + 1), 220),
                style::underline(&style::fg(&bm.title, 81)),
                style::fg(&bm.url, 245)));
        }
        self.tab_mut().content = lines.join("\n");
        self.tab_mut().ix = 0;
        self.render_main();
    }

    fn set_quickmark(&mut self) {
        self.status.say(&style::fg(" Set quickmark (press key):", 220));
        if let Some(key) = Input::getchr(None) {
            if key.len() == 1 {
                let url = self.tab().url.clone();
                let title = self.tab().title.clone();
                self.quickmarks.insert(key.clone(), (url, title));
                config::save_quickmarks(&self.quickmarks);
                self.status.say(&style::fg(&format!(" Quickmark '{}' set", key), 82));
            }
        }
    }

    fn goto_quickmark(&mut self) {
        self.status.say(&style::fg(" Go to quickmark:", 220));
        if let Some(key) = Input::getchr(None) {
            if let Some((url, _)) = self.quickmarks.get(&key).cloned() {
                self.navigate(&url);
            } else {
                self.status.say(&style::fg(&format!(" Quickmark '{}' not set", key), 220));
            }
        }
    }

    // --- Clipboard ---

    fn copy_url(&self) {
        let url = &self.tab().url;
        print!("\x1b]52;c;{}\x07", base64_encode(url.as_bytes()));
        std::io::Write::flush(&mut std::io::stdout()).ok();
    }

    fn copy_focused_url(&self) {
        if self.focus_index >= 0 && (self.focus_index as usize) < self.tab().links.len() {
            let href = &self.tab().links[self.focus_index as usize].href;
            print!("\x1b]52;c;{}\x07", base64_encode(href.as_bytes()));
            std::io::Write::flush(&mut std::io::stdout()).ok();
        }
    }

    // --- Images ---

    fn toggle_images(&mut self) {
        self.conf.show_images = !self.conf.show_images;
        if self.conf.show_images {
            self.image_display = Some(glow::Display::with_mode(&self.conf.image_mode));
        } else {
            self.clear_images();
            self.image_display = None;
        }
        self.status.say(&format!(" Images: {}", if self.conf.show_images { "on" } else { "off" }));
    }

    // --- Preferences ---

    fn show_preferences(&mut self) {
        let mut items: Vec<PrefItem> = vec![
            PrefItem::Bool("Match site colors", self.conf.match_site_colors),
            PrefItem::Choice("Image mode", vec!["auto", "ascii", "off"], self.conf.image_mode.clone()),
            PrefItem::Bool("Show images", self.conf.show_images),
            PrefItem::Text("Homepage", self.conf.homepage.clone()),
            PrefItem::Choice("Search engine", vec!["g", "ddg", "w"], self.conf.search_engine.clone()),
            PrefItem::Text("Download folder", self.conf.download_folder.clone()),
            PrefItem::Color("Info bar fg", self.conf.c_info_fg),
            PrefItem::Color("Info bar bg", self.conf.c_info_bg),
            PrefItem::Color("Tab bar fg", self.conf.c_tab_fg),
            PrefItem::Color("Tab bar bg", self.conf.c_tab_bg),
            PrefItem::Color("Active tab", self.conf.c_active_tab),
            PrefItem::Color("Content fg", self.conf.c_content_fg),
            PrefItem::Color("Content bg", self.conf.c_content_bg),
            PrefItem::Color("Status fg", self.conf.c_status_fg),
            PrefItem::Color("Status bg", self.conf.c_status_bg),
            PrefItem::Color("Link color", self.conf.c_link),
            PrefItem::Color("Link numbers", self.conf.c_link_num),
            PrefItem::Color("Heading h1", self.conf.c_h1),
            PrefItem::Color("Heading h2", self.conf.c_h2),
            PrefItem::Color("Heading h3", self.conf.c_h3),
        ];

        let mut sel = 0usize;
        let mut dirty = false;

        // Create centered popup pane
        let pw = 56u16.min(self.cols - 4);
        let ph = (items.len() as u16 + 5).min(self.rows - 6);
        let px = (self.cols.saturating_sub(pw)) / 2;
        let py = (self.rows.saturating_sub(ph)) / 2;
        let mut popup = Pane::new(px, py, pw, ph, 255, 235);
        popup.border = true;
        popup.border_refresh();

        loop {
            let mut lines = Vec::new();
            lines.push(format!(" {}", style::fg(&style::bold("Preferences"), 81)));
            lines.push(String::new());

            for (i, item) in items.iter().enumerate() {
                let label = format!("{:<18}", item.label());
                let value_str = item.display();
                if i == sel {
                    lines.push(format!(" {} \u{25C0} {} \u{25B6}", style::reverse(&label), value_str));
                } else {
                    lines.push(format!(" {}   {}  ", label, value_str));
                }
            }
            lines.push(String::new());
            lines.push(style::fg(" j/k h/l Enter ESC", 245));

            popup.set_text(&lines.join("\n"));
            popup.ix = 0;
            popup.full_refresh();

            let Some(key) = Input::getchr(None) else { continue };
            match key.as_str() {
                "ESC" | "q" => break,
                "j" | "DOWN" => { if sel < items.len() - 1 { sel += 1; } }
                "k" | "UP" => { if sel > 0 { sel -= 1; } }
                "l" | "RIGHT" => { items[sel].next(); dirty = true; }
                "h" | "LEFT" => { items[sel].prev(); dirty = true; }
                "ENTER" => {
                    let label = items[sel].label().to_string();
                    match &mut items[sel] {
                        PrefItem::Text(_, val) | PrefItem::Choice(_, _, val) => {
                            let new_val = self.status.ask_with_bg(&format!("{}: ", label), val, 18);
                            if !new_val.is_empty() { *val = new_val; dirty = true; }
                        }
                        PrefItem::Color(_, c) => {
                            let new_val = self.status.ask_with_bg("Color (0-255): ", &c.to_string(), 18);
                            if let Ok(v) = new_val.parse::<u16>() { *c = v; dirty = true; }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        // Apply settings back
        if dirty {
            for item in &items {
                match item {
                    PrefItem::Bool("Match site colors", v) => self.conf.match_site_colors = *v,
                    PrefItem::Bool("Show images", v) => self.conf.show_images = *v,
                    PrefItem::Choice("Image mode", _, v) => self.conf.image_mode = v.clone(),
                    PrefItem::Choice("Search engine", _, v) => self.conf.search_engine = v.clone(),
                    PrefItem::Text("Homepage", v) => self.conf.homepage = v.clone(),
                    PrefItem::Text("Download folder", v) => self.conf.download_folder = v.clone(),
                    PrefItem::Color("Info bar fg", c) => self.conf.c_info_fg = *c,
                    PrefItem::Color("Info bar bg", c) => self.conf.c_info_bg = *c,
                    PrefItem::Color("Tab bar fg", c) => self.conf.c_tab_fg = *c,
                    PrefItem::Color("Tab bar bg", c) => self.conf.c_tab_bg = *c,
                    PrefItem::Color("Active tab", c) => self.conf.c_active_tab = *c,
                    PrefItem::Color("Content fg", c) => self.conf.c_content_fg = *c,
                    PrefItem::Color("Content bg", c) => self.conf.c_content_bg = *c,
                    PrefItem::Color("Status fg", c) => self.conf.c_status_fg = *c,
                    PrefItem::Color("Status bg", c) => self.conf.c_status_bg = *c,
                    PrefItem::Color("Link color", c) => self.conf.c_link = *c,
                    PrefItem::Color("Link numbers", c) => self.conf.c_link_num = *c,
                    PrefItem::Color("Heading h1", c) => self.conf.c_h1 = *c,
                    PrefItem::Color("Heading h2", c) => self.conf.c_h2 = *c,
                    PrefItem::Color("Heading h3", c) => self.conf.c_h3 = *c,
                    _ => {}
                }
            }
            self.conf.save();
            // Clear existing images before switching mode
            self.clear_images();
            // Recreate glow Display with new image mode
            let new_display = if self.conf.show_images {
                Some(glow::Display::with_mode(&self.conf.image_mode))
            } else {
                None
            };
            self.image_display = new_display;
            let mode_info = self.image_display.as_ref()
                .and_then(|d| d.protocol())
                .map(|p| format!("{:?}", p))
                .unwrap_or_else(|| "off".into());
            self.status.say(&style::fg(&format!(" Preferences saved (images: {})", mode_info), 82));
        }
        self.render_all();
    }

    // --- Help ---

    fn show_help(&mut self) {
        let help = format!("{}\n\n\
{}\n\
  j/k           Scroll down/up\n\
  Space/PgDn    Page down\n\
  PgUp          Page up\n\
  gg            Go to top\n\
  G             Go to bottom\n\
  C-D/C-U       Half page down/up\n\
  </> arrow     Scroll left/right\n\n\
{}\n\
  o             Open URL\n\
  O             Edit current URL\n\
  t             Open in new tab\n\
  H/Backspace   Go back\n\
  L/Delete      Go forward\n\
  r             Reload\n\n\
{}\n\
  J/Right       Next tab\n\
  K/Left        Previous tab\n\
  d             Close tab\n\
  u             Undo close\n\n\
{}\n\
  Tab/S-Tab     Next/prev link or field\n\
  Enter         Follow link / edit field\n\
  f             Fill and submit form\n\
  y             Copy page URL\n\
  Y             Copy focused link URL\n\n\
{}\n\
  /             Search page\n\
  n/N           Next/prev match\n\n\
{}\n\
  b             Bookmark page\n\
  B             Show bookmarks\n\
  m + key       Set quickmark\n\
  ' + key       Go to quickmark\n\n\
{}\n\
  i             Toggle images\n\
  I             AI page summary\n\
  P             Preferences\n\
  C-L           Force redraw\n\
  :             Command mode\n\
  q             Quit",
            style::bold("Scroll - Terminal Web Browser"),
            style::fg("Scrolling", 220),
            style::fg("Navigation", 220),
            style::fg("Tabs", 220),
            style::fg("Links & Forms", 220),
            style::fg("Search", 220),
            style::fg("Bookmarks", 220),
            style::fg("Other", 220),
        );
        self.tab_mut().content = help;
        self.tab_mut().ix = 0;
        self.render_main();
    }

    // --- AI ---

    fn ai_summary(&mut self) {
        if self.conf.ai_key.is_empty() {
            // Try /home/.safe/openai.txt
            if let Ok(key) = std::fs::read_to_string("/home/.safe/openai.txt") {
                self.conf.ai_key = key.trim().to_string();
            } else {
                self.status.say(&style::fg(" No AI key configured", 220));
                return;
            }
        }
        self.status.say(" Asking AI...");
        let content = crust::strip_ansi(&self.tab().content);
        let text = if content.len() > 4000 { &content[..4000] } else { &content };
        let body = serde_json::json!({
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": format!(
                "Summarize this web page concisely.\nTitle: {}\nURL: {}\n\n{}",
                self.tab().title, self.tab().url, text
            )}],
            "max_tokens": 600
        });
        let resp = std::process::Command::new("curl")
            .args(["-s", "-X", "POST", "https://api.openai.com/v1/chat/completions",
                   "-H", "Content-Type: application/json",
                   "-H", &format!("Authorization: Bearer {}", self.conf.ai_key),
                   "-d", &body.to_string()])
            .output();
        if let Ok(o) = resp {
            let json_str = String::from_utf8_lossy(&o.stdout);
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) {
                let summary = json["choices"][0]["message"]["content"].as_str().unwrap_or("No response");
                self.tab_mut().content = format!("{}\n\n{}", style::bold("AI Summary"), summary);
                self.tab_mut().ix = 0;
                self.render_main();
                return;
            }
        }
        self.status.say(&style::fg(" AI request failed", 196));
    }

    // --- Command mode ---

    fn command_mode(&mut self) {
        let cmd = self.status.ask_with_bg(":", "", 18);
        if cmd.is_empty() { return; }

        let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
        let command = parts[0];
        let args = parts.get(1).unwrap_or(&"").trim();

        match command {
            "open" | "o" => { if !args.is_empty() { self.navigate(args); } }
            "tabopen" | "to" => {
                if !args.is_empty() {
                    self.tabs.push(Tab::new("about:blank"));
                    self.current_tab = self.tabs.len() - 1;
                    self.navigate(args);
                }
            }
            "back" => { self.go_back(); }
            "forward" => { self.go_forward(); }
            "close" | "q" => { self.close_tab(); }
            "quit" | "qa" => { self.running = false; }
            "reload" => { self.reload(); }
            "help" => { self.show_help(); }
            "bookmark" | "bm" => {
                if args.is_empty() {
                    self.bookmark_current();
                } else {
                    // Search bookmarks by name
                    let query = args.to_lowercase();
                    if let Some(bm) = self.bookmarks.iter().find(|b| b.title.to_lowercase().contains(&query)) {
                        let url = bm.url.clone();
                        self.navigate(&url);
                    }
                }
            }
            "bookmarks" | "bms" => { self.show_bookmarks(); }
            "download" | "dl" => {
                if !args.is_empty() {
                    let result = self.fetcher.fetch(args, "GET", None);
                    let filename = args.rsplit('/').next().unwrap_or("download");
                    let path = format!("{}/{}", self.conf.download_folder, filename);
                    if std::fs::write(&path, &result.body).is_ok() {
                        self.status.say(&style::fg(&format!(" Downloaded: {}", path), 82));
                    } else {
                        self.status.say(&style::fg(" Download failed", 196));
                    }
                }
            }
            _ => { self.status.say(&style::fg(&format!(" Unknown command: {}", command), 196)); }
        }
    }

    // --- Resize ---

    fn handle_resize(&mut self) {
        let (cols, rows) = Crust::terminal_size();
        self.cols = cols;
        self.rows = rows;
        let main_h = rows.saturating_sub(3);
        self.info = Pane::new(1, 1, cols, 1, self.conf.c_info_fg, self.conf.c_info_bg);
        self.tab_bar = Pane::new(1, 2, cols, 1, self.conf.c_tab_fg, self.conf.c_tab_bg);
        self.main = Pane::new(1, 3, cols, main_h, self.conf.c_content_fg, self.conf.c_content_bg);
        self.main.scroll = true;
        self.status = Pane::new(1, rows, cols, 1, self.conf.c_status_fg, self.conf.c_status_bg);
        Crust::clear_screen();
        self.render_all();
    }

    fn force_redraw(&mut self) {
        Crust::clear_screen();
        self.render_all();
    }
}

// --- Helpers ---

fn img_cache_path(src: &str) -> String {
    format!("/tmp/scroll_img_{}", src.replace('/', "_").replace(':', "_").replace('?', "_"))
}

fn resolve_search(input: &str, default_engine: &str) -> String {
    let input = input.trim();
    if input.starts_with("http://") || input.starts_with("https://") || input.starts_with("file://") || input.starts_with("about:") {
        return input.to_string();
    }
    if input.contains('.') && !input.contains(' ') {
        return format!("https://{}", input);
    }
    // Search engine
    let parts: Vec<&str> = input.splitn(2, ' ').collect();
    let (engine, query) = if parts.len() == 2 {
        match parts[0] {
            "g" | "ddg" | "w" => (parts[0], parts[1]),
            _ => (default_engine, input),
        }
    } else {
        (default_engine, input)
    };
    let base = match engine {
        "ddg" => "https://duckduckgo.com/?q=",
        "w" => "https://en.wikipedia.org/wiki/Special:Search?search=",
        _ => "https://www.google.com/search?q=",
    };
    format!("{}{}", base, urlencoding(query))
}

fn urlencoding(s: &str) -> String {
    let mut result = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char);
            }
            b' ' => result.push('+'),
            _ => result.push_str(&format!("%{:02X}", b)),
        }
    }
    result
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[(n >> 18 & 63) as usize] as char);
        result.push(CHARS[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 { result.push(CHARS[(n >> 6 & 63) as usize] as char); } else { result.push('='); }
        if chunk.len() > 2 { result.push(CHARS[(n & 63) as usize] as char); } else { result.push('='); }
    }
    result
}

// --- Preferences item types ---

enum PrefItem {
    Bool(&'static str, bool),
    Choice(&'static str, Vec<&'static str>, String),
    Text(&'static str, String),
    Color(&'static str, u16),
}

impl PrefItem {
    fn label(&self) -> &str {
        match self {
            PrefItem::Bool(l, _) | PrefItem::Choice(l, _, _) | PrefItem::Text(l, _) | PrefItem::Color(l, _) => l,
        }
    }

    fn display(&self) -> String {
        match self {
            PrefItem::Bool(_, v) => {
                if *v { style::fg("YES", 82) } else { style::fg("NO", 196) }
            }
            PrefItem::Choice(_, _, v) => style::fg(v, 81),
            PrefItem::Text(_, v) => {
                if v.len() > 25 { format!("{}...", &v[..22]) } else { v.clone() }
            }
            PrefItem::Color(_, c) => {
                format!("{} {:>3}", style::fg("\u{2588}\u{2588}\u{2588}", *c as u8), c)
            }
        }
    }

    fn next(&mut self) {
        match self {
            PrefItem::Bool(_, v) => *v = !*v,
            PrefItem::Choice(_, opts, v) => {
                let idx = opts.iter().position(|&o| o == v.as_str()).unwrap_or(0);
                *v = opts[(idx + 1) % opts.len()].to_string();
            }
            PrefItem::Color(_, c) => *c = (*c + 1) % 256,
            _ => {}
        }
    }

    fn prev(&mut self) {
        match self {
            PrefItem::Bool(_, v) => *v = !*v,
            PrefItem::Choice(_, opts, v) => {
                let idx = opts.iter().position(|&o| o == v.as_str()).unwrap_or(0);
                *v = opts[(idx + opts.len() - 1) % opts.len()].to_string();
            }
            PrefItem::Color(_, c) => *c = if *c == 0 { 255 } else { *c - 1 },
            _ => {}
        }
    }
}
