mod config;
mod fetcher;
mod js;
mod renderer;
mod tab;

use crust::{Crust, Pane, Input};
use crust::style;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use config::Config;
use fetcher::Fetcher;
use tab::Tab;

/// Shared state for async image downloads
struct ImgDownloadState {
    pending: Vec<(String, String)>,  // (url, cache_path) pairs to download
    ready: Vec<String>,              // cache paths that finished downloading
}

/// One planned image placement. `full_w/full_h` is the image's stable
/// natural rendered size in cells (sent to glow as the cache key
/// stabilizer); `src_top/src_visible` is the cell-row crop window
/// into that source — varies as the image scrolls past viewport edges
/// without invalidating glow's image_cache.
#[derive(Clone, PartialEq)]
struct ImagePlanEntry {
    path: String,
    x: u16,
    y: u16,
    full_w: u16,
    full_h: u16,
    src_top: u16,
    src_visible: u16,
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
    /// Per-tab set membership — tab N is a member of set `tab_set[N]`.
    /// Default 3 sets: 0 = Personal, 1 = PassionFruits, 2 = Dualog.
    /// Index into `sets` (the names list).
    tab_set: Vec<usize>,
    /// Named sets. Persisted to `~/.scroll/sets.json`.
    sets: Vec<String>,
    /// Active set index — Right/Left tab cycle skips tabs not in this
    /// set; new tabs inherit this set.
    current_set: usize,
    closed_tabs: Vec<Tab>,
    closed_tab_sets: Vec<usize>,
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
    /// Track `(image_src, viewport_top, line, x, y, w, h)` for every
    /// image scroll has currently placed. Re-render skips re-transmission
    /// when the visible set + their geometry haven't changed — this
    /// cuts kitty image churn during pure-text scrolling, which was
    /// burning through glass's IMG_SLOTS (32) within a session.
    last_placed: Vec<ImagePlanEntry>,
    adblock_domains: HashSet<String>,
}

fn main() {
    config::ensure_dirs();

    let initial_url = std::env::args().nth(1).unwrap_or_else(|| "about:home".into());

    Crust::init();
    Crust::set_app_identity("Scroll");
    // Ask the terminal for distinct sequences for modified keys so
    // we can tell Shift+Backspace from plain Backspace (kitty kbd
    // protocol). Best-effort.
    Crust::enable_modifier_keys();
    let (cols, rows) = Crust::terminal_size();

    let conf = Config::load();
    let show_imgs = conf.show_images;
    let img_mode = conf.image_mode.clone();
    let main_h = rows.saturating_sub(3);

    let sets = config::load_sets();
    let initial_set = sets.first().cloned().unwrap_or_else(|| "Personal".into());
    let mut status_pane = Pane::new(1, rows, cols, 1, conf.c_status_fg, conf.c_status_bg);
    status_pane.record = true;  // shared history across all prompts (Up/Down recalls)
    let mut app = App {
        info: Pane::new(1, 1, cols, 1, conf.c_info_fg, conf.c_info_bg),
        tab_bar: Pane::new(1, 2, cols, 1, conf.c_tab_fg, conf.c_tab_bg),
        main: Pane::new(1, 3, cols, main_h, conf.c_content_fg, conf.c_content_bg),
        status: status_pane,
        cols,
        rows,
        conf,
        fetcher: Fetcher::new_with_set(&initial_set),
        tabs: Vec::new(),
        current_tab: 0,
        tab_set: Vec::new(),
        sets,
        current_set: 0,
        closed_tabs: Vec::new(),
        closed_tab_sets: Vec::new(),
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
        last_placed: Vec::new(),
        adblock_domains: load_adblock(),
    };

    app.main.scroll = true;

    // First-run FF cookie import for the initial set (set_active_set
    // wasn't called via activate_set on construction).
    if let Some(profile) = app.conf.firefox_profiles.get(&initial_set).cloned() {
        if !profile.is_empty() {
            let _ = app.fetcher.import_firefox_cookies(&profile);
        }
    }

    // Create initial tab in the current set and navigate.
    app.tabs.push(Tab::new("about:blank"));
    app.tab_set.push(app.current_set);
    app.navigate(&initial_url);
    app.render_all();

    while app.running {
        // Only poll frequently when images are downloading; otherwise block
        let has_pending = app.img_thread.as_ref().map(|h| !h.is_finished()).unwrap_or(false);
        if has_pending { app.check_new_images(); }
        let timeout = if has_pending { Some(1) } else { None };

        let Some(key) = Input::getchr(timeout) else {
            app.check_new_images();
            continue;
        };

        if app.g_pressed {
            app.g_pressed = false;
            match key.as_str() {
                "g" => { app.main.ix = 0; app.render_main(); continue; }
                // Tab-set admin under `g`. The cycling is on arrow keys.
                "n" => { app.rename_set(); continue; }
                "N" => { app.new_set(); continue; }
                "m" => { app.move_tab_to_set(); continue; }
                _ => {}
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
            "HOME" => { app.tabs[app.current_tab].ix = 0; app.main.ix = 0; app.render_main(); }
            "C-D" => { app.scroll_down(app.rows as usize / 2); }
            "C-U" => { app.scroll_up(app.rows as usize / 2); }
            "<" => { if app.h_scroll >= 10 { app.h_scroll -= 10; } else { app.h_scroll = 0; } app.render_main(); }
            ">" => { app.h_scroll += 10; app.render_main(); }

            // Tab management
            "J" | "RIGHT" => { app.next_tab(); }
            "K" | "LEFT"  => { app.prev_tab(); }
            "S-RIGHT" => { app.move_tab_right(); }
            "S-LEFT"  => { app.move_tab_left(); }
            "C-RIGHT" => { app.next_set(); }
            "C-LEFT"  => { app.prev_set(); }
            "d" => { app.close_tab(); }
            "D" => { app.delete_current_set(); }
            "u" => { app.undo_close_tab(); }

            // Navigation
            "o" => { app.open_url_prompt(); }
            "O" => { app.edit_url_prompt(); }
            "t" => { app.open_in_new_tab(); }
            "T" => { app.tabopen_focused(); }
            "H" | "BACK" => { app.go_back(); }
            "L" | "DEL" | "S-BACK" => { app.go_forward(); }
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

            // Edit
            "e" => { app.edit_source(); }
            "C-G" => { app.edit_form_field(); }

            // Passwords
            "p" => { app.show_password(); }

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

    /// Color for set at index `i`. Cycles through `conf.set_colors`;
    /// falls back to `c_active_tab` if the list is empty.
    fn set_color(&self, i: usize) -> u8 {
        if self.conf.set_colors.is_empty() {
            self.conf.c_active_tab as u8
        } else {
            self.conf.set_colors[i % self.conf.set_colors.len()] as u8
        }
    }

    fn render_tabs(&mut self) {
        // Show every set as a small chip, current set highlighted (bold
        // + brackets), other sets dim. Each set carries its own color
        // so identities (per-set cookie jars) are visually distinct.
        // Replaces the old "[Foo: 1 (+N elsewhere)]" wording, which
        // forced the user to mentally subtract.
        let in_set = self.tabs_in_current_set();
        let count_in_set = in_set.len();
        let mut chips: Vec<String> = Vec::new();
        for (i, name) in self.sets.iter().enumerate() {
            let count = self.tab_set.iter().filter(|&&s| s == i).count();
            let color = self.set_color(i);
            let label = format!("{}: {}", name, count);
            let chip = if i == self.current_set {
                style::bold(&style::fg(&format!(" [{}] ", label), color))
            } else {
                style::fg(&format!(" {} ", label), color)
            };
            chips.push(chip);
        }
        let chip_block = chips.join("");
        if count_in_set <= 1 {
            self.tab_bar.say(&chip_block);
            return;
        }
        let parts: Vec<String> = in_set.iter().map(|&i| {
            let t = &self.tabs[i];
            let label = if t.title.is_empty() {
                t.url.chars().take(20).collect::<String>()
            } else {
                t.title.chars().take(20).collect::<String>()
            };
            if i == self.current_tab {
                // Active tab: bold white — pops against the colored
                // set chips and any inactive tabs.
                style::bold(&style::fg(&format!(" {} ", label), 255))
            } else {
                // Inactive tabs: dim gray.
                style::fg(&format!(" {} ", label), 244)
            }
        }).collect();
        self.tab_bar.say(&format!("{}  {}", chip_block, parts.join("\u{2502}")));
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
        // Diff-based image management: `show_visible_images` decides
        // whether to clear+re-emit based on plan change. Don't pre-
        // clear here — that would force re-emission on every render
        // and burn glass's IMG_SLOTS table within a single browse
        // session.
        self.main.full_refresh();
        if self.conf.show_images { self.show_visible_images(); }
    }

    /// Start async download of all images on the page
    fn start_image_downloads(&mut self) {
        let images = self.tabs[self.current_tab].images.clone();
        if images.is_empty() { return; }

        // Queue all images for download (skip blocked ad domains)
        let mut pending = Vec::new();
        for img in &images {
            if self.is_blocked(&img.src) { continue; }
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
                    let ct = resp.content_type().to_string();
                    // oEmbed: JSON response containing thumbnail_url
                    if ct.contains("json") && url.contains("oembed") {
                        if let Ok(body) = resp.into_string() {
                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                                if let Some(thumb) = json["thumbnail_url"].as_str() {
                                    if let Ok(tr) = agent.get(thumb).call() {
                                        let mut bytes = Vec::new();
                                        if std::io::Read::read_to_end(&mut tr.into_reader(), &mut bytes).is_ok() && !bytes.is_empty() {
                                            let _ = std::fs::write(cache_path, &bytes);
                                            let mut s = state.lock().unwrap();
                                            s.ready.push(cache_path.clone());
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }
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

    /// Compute the placement plan for the current viewport without
    /// touching the terminal. Pure function over `tab.images` + scroll
    /// position + pane geometry. Used by `show_visible_images` for
    /// the diff check. Plan tuples now carry the FULL natural image
    /// dims `(img_w, img_h)` plus `(src_top, src_visible)` clip cells —
    /// this keeps the size stable across scroll lines so glow's cache
    /// hits and the same kitty image_id (and IMG_SLOT) gets reused.
    fn compute_image_plan(&self) -> Vec<ImagePlanEntry> {
        let viewport_top = self.tabs[self.current_tab].ix;
        let viewport_h = self.main.h as usize;
        let viewport_bottom = viewport_top + viewport_h;
        let mut plan = Vec::new();
        for img in &self.tabs[self.current_tab].images {
            if img.line + img.height <= viewport_top || img.line >= viewport_bottom { continue; }
            let cache_path = img_cache_path(&img.src);
            if !std::path::Path::new(&cache_path).exists() { continue; }
            let img_w = (self.main.w / 3).max(30).min(80);
            let img_h = img.height as u16;
            let (display_x, display_y, src_top, src_visible);
            if img.line < viewport_top {
                let clipped_top = (viewport_top - img.line) as u16;
                let visible_rows = img_h.saturating_sub(clipped_top);
                if visible_rows == 0 { continue; }
                display_x = self.main.x;
                display_y = self.main.y;
                src_top = clipped_top;
                src_visible = visible_rows.min(self.main.h);
            } else {
                let y_offset = (img.line - viewport_top) as u16;
                display_x = self.main.x;
                display_y = self.main.y + y_offset;
                src_top = 0;
                src_visible = img_h.min(self.main.h.saturating_sub(y_offset));
            }
            if src_visible == 0 { continue; }
            plan.push(ImagePlanEntry {
                path: cache_path,
                x: display_x,
                y: display_y,
                full_w: img_w,
                full_h: img_h,
                src_top,
                src_visible,
            });
        }
        plan
    }

    /// Show images that are in viewport AND already cached locally.
    /// Diff-based: skips re-transmission when the visible set + their
    /// geometry haven't changed since the last call. When the plan
    /// changes, clears the previous placements first and re-emits.
    /// This caps glass's IMG_SLOTS churn during pure-text scrolling.
    fn show_visible_images(&mut self) {
        if self.image_display.is_none() { return; }
        let supported = self.image_display.as_ref().map(|d| d.supported()).unwrap_or(false);
        if !supported { return; }
        let plan = self.compute_image_plan();
        if plan == self.last_placed {
            return;
        }
        // Plan changed. Per-image diff: only forget paths that
        // disappeared. For paths still in plan, call show_clipped:
        // glow keys its cache by full_w/full_h (stable across
        // viewport-edge clipping) so the same image_id is reused
        // every scroll line — kitty's source-rect crop handles the
        // visible portion. Net: no PNG re-transmits and no fresh
        // IMG_SLOTS burned during pure scrolling.
        let new_paths: std::collections::HashSet<&String> =
            plan.iter().map(|e| &e.path).collect();
        let gone: Vec<String> = self.last_placed.iter()
            .filter(|e| !new_paths.contains(&e.path))
            .map(|e| e.path.clone())
            .collect();
        if let Some(ref mut display) = self.image_display {
            for path in &gone {
                display.forget_path(path);
            }
            for e in &plan {
                display.show_clipped(&e.path, e.x, e.y, e.full_w, e.full_h, e.src_top, e.src_visible);
            }
        }
        self.last_placed = plan;
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
        // Forget the placement plan — anything previously on screen
        // is gone, so the next show_visible_images sees an empty
        // baseline and re-emits.
        self.last_placed.clear();
    }

    /// Single entry point for status-bar prompts. Always re-renders
    /// the status line afterward so cancelled prompts (ESC → empty
    /// return) don't leave the temp-bg prompt visible until the next
    /// unrelated render. Pane.record == true gives Up/Down history.
    fn prompt(&mut self, label: &str, initial: &str) -> String {
        let result = self.status.ask_with_bg(label, initial, 18);
        self.render_status();
        result
    }

    fn render_status(&mut self) {
        let tab = &self.tabs[self.current_tab];
        let n_links = tab.links.len();
        let left = if n_links > 0 {
            format!(" {} links | ? help | : command", n_links)
        } else {
            " ? help | : command".to_string()
        };
        let version = format!("scroll v{}", env!("CARGO_PKG_VERSION"));
        let url = &tab.url;
        let mid = format!(" | {}", url);
        let total_left = crust::display_width(&left) + crust::display_width(&mid);
        let pad = (self.cols as usize).saturating_sub(total_left + version.len() + 1);
        self.status.say(&format!("{}{}{}{}", left, mid, " ".repeat(pad), version));
    }

    // --- Navigation ---

    fn navigate(&mut self, url: &str) {
        self.clear_images();
        let resolved = resolve_search(url, &self.conf.search_engine);

        // Handle mailto: links - open in Kastrup
        if resolved.starts_with("mailto:") {
            Crust::cleanup();
            let _ = std::process::Command::new("kastrup").arg(&resolved).status();
            Crust::init();
            Crust::clear_screen();
            self.handle_resize();
            return;
        }

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
            let url = result.url;
            let filename = url.rsplit('/').next().unwrap_or("image").to_string();
            // Reserve blank lines for image, then show filename below
            let reserve = (self.main.h as usize).min(30);
            let mut content = String::new();
            for _ in 0..reserve { content.push('\n'); }
            content.push_str(&format!("\n{}\n{}",
                crust::style::fg(&filename, 81),
                crust::style::fg(&url, 245)));
            self.tab_mut().content = content;
            self.tab_mut().title = filename.clone();
            self.tab_mut().url = url.clone();
            self.tab_mut().images = vec![crate::tab::ImageRef {
                src: url, alt: filename, line: 0, height: reserve,
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
        self.check_autofill();
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
            // Run inline <script> tags first so a JS-driven redirect
            // (window.location.href = "...") gets honoured before the
            // user sees the placeholder page. Capped at one hop per
            // load to avoid infinite redirect loops.
            // Snapshot the page-host cookies + load localStorage so
            // JS sees a real document.cookie / localStorage. After
            // execution, merge any JS-driven changes back.
            let host = url::Url::parse(&result.url).ok()
                .and_then(|u| u.host_str().map(|h| h.to_string()))
                .unwrap_or_default();
            let cookies_in = self.fetcher.cookies_for_host(&host);
            let set_name = self.fetcher.active_set_name().to_string();
            let ls_in = config::load_localstorage(&set_name, &host);
            let js = js::run_scripts(&result.body, &result.url, cookies_in, ls_in);
            if js.cookies_dirty && !host.is_empty() {
                self.fetcher.replace_cookies_for_host(&host, js.cookies);
            }
            if js.localstorage_dirty && !host.is_empty() {
                config::save_localstorage(&set_name, &host, &js.localstorage);
            }
            // Hand JS-set field values to the form-fill path via the
            // tab — so a hidden input populated by inline JS rides
            // along on submit.
            if !js.dom_values.is_empty() {
                self.tab_mut().js_dom_values = js.dom_values.clone();
            }
            // Stash the captured console output and the extracted
            // script bodies on the tab. The submit-time re-run uses
            // the scripts (without re-fetching externals); :jslog
            // surfaces the log for debugging.
            self.tab_mut().js_log = js.log.clone();
            self.tab_mut().js_scripts = js.scripts.clone();
            self.tab_mut().raw_html = result.body.clone();
            if let Some(target) = js.redirect {
                if !target.is_empty() && target != result.url {
                    let resolved = renderer::resolve_url(&result.url, &target);
                    self.status.say(&format!(" JS redirect → {}", &resolved));
                    self.navigate(&resolved);
                    return;
                }
            }
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
        self.check_autofill();
    }

    // --- Scrolling ---

    fn scroll_down(&mut self, n: usize) {
        // No pre-clear: show_visible_images does a per-image diff and
        // glow's show() moves an existing kitty placement via per-id
        // delete + place, which keeps the same image_id (and therefore
        // the same IMG_SLOT in glass). Pre-clearing all ids would
        // force fresh ids on every line of scrolling — 3 visible
        // images × 85 lines = 256 = wedged.
        let lc = self.tab().content.lines().count();
        let page = self.main.h as usize;
        let max_ix = lc.saturating_sub(page);
        let old_ix = self.tabs[self.current_tab].ix;
        if old_ix >= max_ix { return; }
        let new_ix = (old_ix + n).min(max_ix);
        self.tabs[self.current_tab].ix = new_ix;
        self.main.ix = new_ix;
        let delta = (new_ix - old_ix) as i32;
        self.main.scroll_refresh(delta);
        if self.conf.show_images { self.show_visible_images(); }
    }

    fn scroll_up(&mut self, n: usize) {
        let old_ix = self.tabs[self.current_tab].ix;
        self.tabs[self.current_tab].ix = old_ix.saturating_sub(n);
        self.main.ix = self.tabs[self.current_tab].ix;
        let delta = -((old_ix - self.tabs[self.current_tab].ix) as i32);
        self.main.scroll_refresh(delta);
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
        let lc = self.tab().content.lines().count();
        let page = self.main.h as usize;
        self.tabs[self.current_tab].ix = lc.saturating_sub(page);
        self.main.ix = self.tabs[self.current_tab].ix;
        self.main.refresh();
        if self.conf.show_images { self.show_visible_images(); }
    }

    // --- Tabs ---

    /// Rotate the active tab one position to the right within its set
    /// (wraps). Other sets' tabs aren't disturbed.
    fn move_tab_right(&mut self) {
        let in_set = self.tabs_in_current_set();
        if in_set.len() <= 1 { return; }
        let pos = match in_set.iter().position(|&i| i == self.current_tab) {
            Some(p) => p, None => return,
        };
        let next_pos = (pos + 1) % in_set.len();
        let a = in_set[pos];
        let b = in_set[next_pos];
        self.tabs.swap(a, b);
        self.tab_set.swap(a, b);
        self.current_tab = b;
        self.render_all();
    }

    /// Rotate the active tab one position to the left within its set.
    fn move_tab_left(&mut self) {
        let in_set = self.tabs_in_current_set();
        if in_set.len() <= 1 { return; }
        let pos = match in_set.iter().position(|&i| i == self.current_tab) {
            Some(p) => p, None => return,
        };
        let n = in_set.len();
        let prev_pos = (pos + n - 1) % n;
        let a = in_set[pos];
        let b = in_set[prev_pos];
        self.tabs.swap(a, b);
        self.tab_set.swap(a, b);
        self.current_tab = b;
        self.render_all();
    }

    /// Indices (in `self.tabs`) of tabs in the current set.
    fn tabs_in_current_set(&self) -> Vec<usize> {
        (0..self.tabs.len())
            .filter(|&i| self.tab_set.get(i).copied() == Some(self.current_set))
            .collect()
    }

    /// Switch the active set index AND the fetcher's cookie jar so the
    /// next request uses that set's identity. All set-changing paths
    /// (next_set, prev_set, new_set, close-tab sync, move-tab-to-set,
    /// undo-close, etc.) route through this so we can't forget one.
    fn activate_set(&mut self, new_set: usize) {
        self.current_set = new_set;
        if let Some(name) = self.sets.get(new_set).cloned() {
            self.fetcher.set_active_set(&name);
            // If this set is mapped to a Firefox profile, refresh
            // the jar from that profile's cookies.sqlite. Lets the
            // user log in to Google (or anywhere JS-heavy) once in
            // Firefox per profile and have scroll inherit the
            // session per set, with no JS engine in scroll itself.
            if let Some(profile) = self.conf.firefox_profiles.get(&name).cloned() {
                if !profile.is_empty() {
                    let _ = self.fetcher.import_firefox_cookies(&profile);
                }
            }
        }
    }

    fn next_tab(&mut self) {
        let in_set = self.tabs_in_current_set();
        if in_set.len() <= 1 { return; }
        let pos = in_set.iter().position(|&i| i == self.current_tab).unwrap_or(0);
        self.current_tab = in_set[(pos + 1) % in_set.len()];
        self.render_all();
    }

    fn prev_tab(&mut self) {
        let in_set = self.tabs_in_current_set();
        if in_set.len() <= 1 { return; }
        let pos = in_set.iter().position(|&i| i == self.current_tab).unwrap_or(0);
        let n = in_set.len();
        self.current_tab = in_set[(pos + n - 1) % n];
        self.render_all();
    }

    fn close_tab(&mut self) {
        // Closing the only tab anywhere quits the app.
        if self.tabs.len() <= 1 {
            self.running = false;
            return;
        }
        let closed = self.tabs.remove(self.current_tab);
        let closed_set = self.tab_set.remove(self.current_tab);
        self.closed_tabs.push(closed);
        self.closed_tab_sets.push(closed_set);
        // Pick the next current_tab: prefer something in the current
        // set; otherwise jump to whatever's around.
        let in_set = self.tabs_in_current_set();
        if let Some(&i) = in_set.first() {
            self.current_tab = i;
        } else if !self.tabs.is_empty() {
            // No tabs left in current set — fall through to whatever
            // index we landed on (clamped). User can switch sets to
            // find their tabs.
            if self.current_tab >= self.tabs.len() {
                self.current_tab = self.tabs.len() - 1;
            }
            // Sync current_set to the new current_tab's set so the
            // tab bar shows the right thing AND so the cookie jar
            // follows the surviving tab's identity.
            self.activate_set(self.tab_set[self.current_tab]);
        }
        self.render_all();
    }

    fn undo_close_tab(&mut self) {
        if let Some(tab) = self.closed_tabs.pop() {
            let set = self.closed_tab_sets.pop().unwrap_or(self.current_set);
            self.tabs.insert(self.current_tab + 1, tab);
            self.tab_set.insert(self.current_tab + 1, set);
            self.current_tab += 1;
            self.activate_set(set);
            self.render_all();
        }
    }

    // --- Tab sets ---

    fn next_set(&mut self) {
        if self.sets.is_empty() { return; }
        self.activate_set((self.current_set + 1) % self.sets.len());
        // Park current_tab on the first tab of the new set, if any.
        if let Some(&i) = self.tabs_in_current_set().first() {
            self.current_tab = i;
        } else {
            // No tabs in this set yet — open one so the user can browse.
            self.tabs.push(Tab::new("about:blank"));
            self.tab_set.push(self.current_set);
            self.current_tab = self.tabs.len() - 1;
            self.navigate(&self.conf.homepage.clone());
        }
        self.render_all();
    }

    fn prev_set(&mut self) {
        if self.sets.is_empty() { return; }
        let n = self.sets.len();
        self.activate_set((self.current_set + n - 1) % n);
        if let Some(&i) = self.tabs_in_current_set().first() {
            self.current_tab = i;
        } else {
            self.tabs.push(Tab::new("about:blank"));
            self.tab_set.push(self.current_set);
            self.current_tab = self.tabs.len() - 1;
            self.navigate(&self.conf.homepage.clone());
        }
        self.render_all();
    }

    fn rename_set(&mut self) {
        let cur = self.sets.get(self.current_set).cloned().unwrap_or_default();
        let name = self.prompt("Rename set: ", &cur);
        let trimmed = name.trim();
        if !trimmed.is_empty() && trimmed != cur {
            self.sets[self.current_set] = trimmed.to_string();
            // Rename the on-disk cookie jar so the active identity stays
            // bound to the new set name.
            self.fetcher.rename_set(&cur, trimmed);
            config::save_sets(&self.sets);
        }
        self.render_all();
    }

    fn new_set(&mut self) {
        let name = self.prompt("New set name: ", "");
        let trimmed = name.trim();
        if trimmed.is_empty() { return; }
        self.sets.push(trimmed.to_string());
        config::save_sets(&self.sets);
        // activate_set initialises a fresh cookie jar for the new set
        // (no inheritance), which is exactly what "different identity"
        // needs.
        self.activate_set(self.sets.len() - 1);
        self.tabs.push(Tab::new("about:blank"));
        self.tab_set.push(self.current_set);
        self.current_tab = self.tabs.len() - 1;
        let home = self.conf.homepage.clone();
        self.navigate(&home);
        self.render_all();
    }

    /// Delete the current set: drops every tab in it, removes the set
    /// from the list, deletes the set's cookie-jar file, and parks on
    /// another set's first tab. Refuses if it would leave zero sets.
    /// Confirmation prompt expects exactly "yes" to proceed (so a
    /// stray Enter or 'y' doesn't blow away an identity).
    fn delete_current_set(&mut self) {
        if self.sets.len() <= 1 {
            self.status.say(" Cannot delete the last remaining set.");
            return;
        }
        let cur_idx = self.current_set;
        let cur_name = self.sets[cur_idx].clone();
        let in_set: Vec<usize> = self.tabs_in_current_set();
        let confirm = self.prompt(
            &format!(
                "Delete set \"{}\" and {} tab{}? Type 'yes' to confirm: ",
                cur_name,
                in_set.len(),
                if in_set.len() == 1 { "" } else { "s" },
            ),
            "",
        );
        if confirm.trim() != "yes" {
            self.status.say(&format!(" Cancelled — set \"{}\" not deleted", cur_name));
            return;
        }

        // 1. Drop every tab in this set (high index first so earlier
        //    indices don't shift before we remove them).
        let mut to_drop = in_set.clone();
        to_drop.sort_unstable();
        for &i in to_drop.iter().rev() {
            self.tabs.remove(i);
            self.tab_set.remove(i);
        }
        // Adjust set indices in tab_set: any set index > cur_idx
        // shifts down by 1 because the set list will lose an entry.
        for s in self.tab_set.iter_mut() {
            if *s > cur_idx { *s -= 1; }
        }
        // 2. Remove the set itself + its cookie-jar file on disk.
        self.sets.remove(cur_idx);
        let jar_path = config::cookie_jar_path(&cur_name);
        let _ = std::fs::remove_file(&jar_path);
        config::save_sets(&self.sets);

        // 3. Pick a destination set: prefer the same-position index,
        //    falling back to the last available.
        let new_set = cur_idx.min(self.sets.len() - 1);
        // Make sure the destination has at least one tab; if not,
        // open the homepage there.
        let has_tab = self.tab_set.iter().any(|&s| s == new_set);
        if !has_tab {
            self.tabs.push(Tab::new("about:blank"));
            self.tab_set.push(new_set);
        }
        self.activate_set(new_set);
        // 4. Park current_tab on the first tab of the destination set.
        let dest_first = self.tab_set.iter().position(|&s| s == new_set).unwrap_or(0);
        self.current_tab = dest_first;
        // Out-of-band navigation if we just opened a blank one.
        if !has_tab {
            let home = self.conf.homepage.clone();
            self.navigate(&home);
        }
        self.render_all();
        self.status.say(&format!(" Deleted set \"{}\"", cur_name));
    }

    /// Move the currently active tab to a different set. Prompts for
    /// the target set name (case-insensitive prefix match).
    fn move_tab_to_set(&mut self) {
        let names = self.sets.join(", ");
        let prompt = format!("Move to set ({}): ", names);
        let answer = self.prompt(&prompt, "");
        let q = answer.trim().to_lowercase();
        if q.is_empty() { return; }
        let target = self.sets.iter().position(|n| n.to_lowercase().starts_with(&q));
        if let Some(t) = target {
            self.tab_set[self.current_tab] = t;
            self.activate_set(t);
            self.render_all();
        }
    }

    // --- URL prompts ---

    fn open_url_prompt(&mut self) {
        let url = self.prompt("Open: ", "");
        if !url.is_empty() {
            self.navigate(&url);
        }
    }

    fn edit_url_prompt(&mut self) {
        let current = self.tab().url.clone();
        let url = self.prompt("Open: ", &current);
        if !url.is_empty() {
            self.navigate(&url);
        }
    }

    fn open_in_new_tab(&mut self) {
        let url = self.prompt("Tab open: ", "");
        if !url.is_empty() {
            self.tabs.push(Tab::new("about:blank"));
            self.tab_set.push(self.current_set);
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

    /// Open `href` in a freshly-spawned tab parked in the current set,
    /// switch to it, and start the navigation. Same recipe used by
    /// `open_in_new_tab` but accepts a pre-resolved URL — keeps the
    /// link-prompt and focused-link paths in sync.
    fn open_href_in_new_tab(&mut self, href: &str) {
        self.tabs.push(Tab::new("about:blank"));
        self.tab_set.push(self.current_set);
        self.current_tab = self.tabs.len() - 1;
        self.navigate(href);
    }

    /// ENTER: if focused on a link, follow it. Otherwise prompt for
    /// link number. The prompt accepts a trailing `t` to open the
    /// link in a new tab instead of replacing the current one
    /// (e.g. `42t` = "open link 42 in a new tab"). Bare `42` keeps
    /// the original semantics (current tab).
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
        // Prompt for link number; trailing 't' = tab-open
        let input = self.prompt("Link #: ", "");
        let trimmed = input.trim();
        if trimmed.is_empty() { return; }
        let (num_str, tab_open) = if let Some(rest) = trimmed.strip_suffix(|c: char| c == 't' || c == 'T') {
            (rest.trim_end(), true)
        } else if let Some(rest) = trimmed.strip_prefix(|c: char| c == 't' || c == 'T') {
            (rest.trim_start(), true)
        } else {
            (trimmed, false)
        };
        let Ok(num) = num_str.parse::<usize>() else {
            self.status.say(&style::fg(&format!(" Invalid link spec: {}", trimmed), 196));
            return;
        };
        let Some(href) = self.tabs[self.current_tab].links.iter()
            .find(|l| l.index == num)
            .map(|l| l.href.clone())
        else {
            self.status.say(&style::fg(&format!(" Link {} not found", num), 196));
            return;
        };
        if tab_open {
            self.open_href_in_new_tab(&href);
        } else {
            self.navigate(&href);
        }
    }

    /// `T` on a focused link: open it in a new tab. Mirrors the `t`
    /// suffix in the link-number prompt so muscle memory works either
    /// direction (Tab-cycle a link then `T`, or `Enter 42t`).
    fn tabopen_focused(&mut self) {
        if self.focus_index < 0 { return; }
        let idx = self.focus_index as usize;
        let n_links = self.tabs[self.current_tab].links.len();
        if idx >= n_links { return; }
        let href = self.tabs[self.current_tab].links[idx].href.clone();
        self.open_href_in_new_tab(&href);
    }

    fn fill_form(&mut self) {
        if self.tab().forms.is_empty() { return; }
        let form = self.tab().forms[0].clone();
        let mut params = HashMap::new();

        // Resolve credentials for the current host once — used to
        // pre-fill both username-shaped fields AND password fields.
        let host = url::Url::parse(&self.tab().url).ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()));
        let creds: Option<(String, String)> = host.as_ref().and_then(|h| self.lookup_password(h));

        for field in &form.fields {
            // JS-typed value for this field (by element id) takes
            // precedence over the static value attribute. Lets a
            // field that an inline script populated from a cookie or
            // a fetch() ride along on submit.
            let js_value: Option<String> = if field.id.is_empty() {
                None
            } else {
                self.tab().js_dom_values.get(&field.id).cloned()
            };
            match field.field_type.as_str() {
                "hidden" => {
                    let v = js_value.unwrap_or_else(|| field.value.clone());
                    params.insert(field.name.clone(), v);
                }
                "password" => {
                    let default = js_value
                        .or_else(|| creds.as_ref().map(|(_, p)| p.clone()))
                        .unwrap_or_default();
                    let val = self.prompt(&format!("{}: ", field.name), &default);
                    params.insert(field.name.clone(), val);
                }
                "select" => {
                    let options: Vec<String> = field.options.iter().map(|(_, l)| l.clone()).collect();
                    let default = js_value.unwrap_or_default();
                    let val = self.prompt(&format!("{} ({}): ", field.name, options.join("/")), &default);
                    params.insert(field.name.clone(), val);
                }
                _ => {
                    let is_userish = field.name.contains("user")
                        || field.name.contains("email")
                        || field.name.contains("login");
                    let autofill = creds.as_ref()
                        .filter(|_| is_userish)
                        .map(|(u, _)| u.clone());
                    let default = js_value
                        .or(autofill)
                        .unwrap_or_else(|| field.value.clone());
                    let val = self.prompt(&format!("{}: ", field.placeholder), &default);
                    params.insert(field.name.clone(), val);
                }
            }
        }

        // Pre-submit JS hook: re-run the page's scripts with the
        // user's typed values mirrored into the dom map, then fire
        // a synthetic submit event. Listeners can call
        // event.preventDefault() to abort. Skips the re-run when
        // the page had no scripts (overwhelming majority of forms).
        if !self.tab().js_scripts.is_empty() {
            let host = url::Url::parse(&self.tab().url).ok()
                .and_then(|u| u.host_str().map(|h| h.to_string()))
                .unwrap_or_default();
            let cookies_in = self.fetcher.cookies_for_host(&host);
            let set_name = self.fetcher.active_set_name().to_string();
            let ls_in = config::load_localstorage(&set_name, &host);
            // Mirror filled values into dom by element id (where the
            // form field had one).
            let mut pre_dom: HashMap<String, String> = HashMap::new();
            for f in &form.fields {
                if !f.id.is_empty() {
                    if let Some(v) = params.get(&f.name) {
                        pre_dom.insert(f.id.clone(), v.clone());
                    }
                }
            }
            // Form id (we don't track it on Form yet — pass empty so
            // listeners on document/window still fire; element-level
            // form listeners that registered against the form's id
            // won't match).
            let scripts = self.tab().js_scripts.clone();
            let raw_html = self.tab().raw_html.clone();
            let url_for_js = self.tab().url.clone();
            let r = js::run_extracted(
                scripts, &raw_html, &url_for_js,
                cookies_in, ls_in, pre_dom, Some(String::new()),
            );
            // Persist any cookie / localStorage side-effects from
            // the listener.
            if r.cookies_dirty && !host.is_empty() {
                self.fetcher.replace_cookies_for_host(&host, r.cookies);
            }
            if r.localstorage_dirty && !host.is_empty() {
                config::save_localstorage(&set_name, &host, &r.localstorage);
            }
            if !r.log.is_empty() {
                self.tab_mut().js_log.extend(r.log);
            }
            if r.submit_prevented {
                self.status.say(&style::fg(" Submission cancelled by page JS (preventDefault)", 220));
                return;
            }
            // A submit listener may also have re-typed field values;
            // sync those back into params before navigating.
            for f in &form.fields {
                if !f.id.is_empty() {
                    if let Some(v) = r.dom_values.get(&f.id) {
                        params.insert(f.name.clone(), v.clone());
                    }
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
        let term = self.prompt("/", "");
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

    fn copy_url(&mut self) {
        let url = self.tab().url.clone();
        crust::clipboard_copy(&url, "clipboard");
        self.status.say(&format!(" Copied: {}", url));
    }

    fn copy_focused_url(&mut self) {
        if self.focus_index < 0 { return; }
        let idx = self.focus_index as usize;
        if idx >= self.tab().links.len() { return; }
        let href = self.tab().links[idx].href.clone();
        crust::clipboard_copy(&href, "clipboard");
        self.status.say(&format!(" Copied: {}", href));
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
        // Build the help content from the current keymap. Sections
        // mirror the dispatcher in main(); when keys change there,
        // update them here too. Grouped to fit a centered popup.
        let h = |t: &str| style::fg(&style::bold(t), 220);
        let lines: Vec<String> = vec![
            format!(" {}", style::fg(&style::bold("Scroll — Terminal Web Browser"), 81)),
            String::new(),
            format!(" {}", h("Scrolling")),
            "   j / k   ↓ / ↑      line down / up".into(),
            "   Space / PgDn       page down".into(),
            "   PgUp               page up".into(),
            "   C-d / C-u          half-page down / up".into(),
            "   gg / G  Home/End   top / bottom".into(),
            "   < / >              horizontal scroll".into(),
            String::new(),
            format!(" {}", h("Tabs")),
            "   →  /  ←            next / prev tab in current set".into(),
            "   S-→ / S-←          move active tab right / left".into(),
            "   t                  open in new tab".into(),
            "   d                  close tab".into(),
            "   u                  undo close".into(),
            String::new(),
            format!(" {}", h("Sets (per-set cookie jars / identities)")),
            "   C-→ / C-←          next / prev set".into(),
            "   gn                 rename current set".into(),
            "   gN                 new set".into(),
            "   gm                 move active tab to another set".into(),
            "   D                  delete current set (with confirmation)".into(),
            String::new(),
            format!(" {}", h("Navigation")),
            "   o                  open URL".into(),
            "   O                  edit current URL".into(),
            "   H  /  Backspace    back".into(),
            "   L  /  Delete       forward".into(),
            "   r                  reload".into(),
            String::new(),
            format!(" {}", h("Links & forms")),
            "   Tab / S-Tab        focus next / prev link or field".into(),
            "   Enter              follow focused link / edit field".into(),
            "   Enter then NN      follow link by [N] number (e.g. 42)".into(),
            "   Enter then NNt     open link [N] in a new tab (e.g. 42t)".into(),
            "   T                  open focused link in a new tab".into(),
            "   f                  fill and submit form".into(),
            "   e                  edit page source in $EDITOR".into(),
            "   C-g                edit focused form field in $EDITOR".into(),
            "   p                  show stored password for site".into(),
            "   y / Y              copy page URL / focused link URL".into(),
            String::new(),
            format!(" {}", h("Search")),
            "   /                  search page".into(),
            "   n / N              next / prev match".into(),
            String::new(),
            format!(" {}", h("Bookmarks & quickmarks")),
            "   b                  bookmark current page".into(),
            "   B                  show bookmarks".into(),
            "   m{key}             set quickmark".into(),
            "   '{key}             go to quickmark".into(),
            String::new(),
            format!(" {}", h("Other")),
            "   i                  toggle images".into(),
            "   I                  AI page summary".into(),
            "   P                  preferences".into(),
            "   C-l                hard redraw (resets kitty image state)".into(),
            "   :                  command mode (see below)".into(),
            "   ?                  this help".into(),
            "   q                  quit".into(),
            String::new(),
            format!(" {}", h(":commands")),
            "   :back / :forward / :reload / :help".into(),
            "   :password          save credentials for current site".into(),
            "   :adblock           update ad-block list".into(),
            "   :ffimport          re-import cookies from current set's FF profile".into(),
            "   :browse / :ff      open current URL in Firefox (uses set's profile)".into(),
            "   :jslog             show captured console output for this page".into(),
            String::new(),
            style::fg(" j/k or ↓/↑ scroll · ESC / q / ? close", 245),
        ];

        let pw = 72u16.min(self.cols.saturating_sub(4));
        let ph = 28u16.min(self.rows.saturating_sub(4));
        let px = (self.cols.saturating_sub(pw)) / 2;
        let py = (self.rows.saturating_sub(ph)) / 2;
        let mut popup = Pane::new(px, py, pw, ph, 255, 235);
        popup.border = true;
        popup.scroll = true;
        popup.set_text(&lines.join("\n"));
        popup.ix = 0;
        popup.border_refresh();
        popup.full_refresh();

        loop {
            let Some(key) = Input::getchr(None) else { continue };
            match key.as_str() {
                "ESC" | "q" | "?" => break,
                "j" | "DOWN" => {
                    let total = lines.len() as u16;
                    let view = ph.saturating_sub(2);
                    if (popup.ix as u16) + view < total {
                        popup.ix += 1;
                        popup.full_refresh();
                    }
                }
                "k" | "UP" => {
                    if popup.ix > 0 { popup.ix -= 1; popup.full_refresh(); }
                }
                "PgDOWN" | " " => {
                    let total = lines.len() as u16;
                    let view = ph.saturating_sub(2);
                    let step = view as usize;
                    if (popup.ix as u16) + view < total {
                        popup.ix = ((popup.ix + step) as u16).min(total.saturating_sub(view)) as usize;
                        popup.full_refresh();
                    }
                }
                "PgUP" => {
                    let view = ph.saturating_sub(2) as usize;
                    popup.ix = popup.ix.saturating_sub(view);
                    popup.full_refresh();
                }
                "g" => { popup.ix = 0; popup.full_refresh(); }
                "G" => {
                    let total = lines.len() as u16;
                    let view = ph.saturating_sub(2);
                    popup.ix = total.saturating_sub(view) as usize;
                    popup.full_refresh();
                }
                _ => {}
            }
        }

        // Restore the page underneath.
        self.render_all();
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
        let cmd = self.prompt(":", "");
        if cmd.is_empty() { return; }

        let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
        let command = parts[0];
        let args = parts.get(1).unwrap_or(&"").trim();

        match command {
            "open" | "o" => { if !args.is_empty() { self.navigate(args); } }
            "tabopen" | "to" => {
                if !args.is_empty() {
                    self.tabs.push(Tab::new("about:blank"));
                    self.tab_set.push(self.current_set);
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
            "password" | "pw" => { self.save_password_cmd(); }
            "adblock" => { self.update_adblock(); }
            "ffimport" => { self.ffimport_cmd(); }
            "browse" | "ff" => { self.browse_in_firefox(); }
            "jslog" => { self.show_jslog(); }
            _ => { self.status.say(&style::fg(&format!(" Unknown command: {}", command), 196)); }
        }
    }

    /// `:jslog` — popup viewer for the page's captured console
    /// output (console.log/.warn/.error/.info plus our own [error]
    /// entries from script throws and submit-listener throws).
    /// Lets you debug "why didn't this site work" without spinning
    /// up a real browser.
    fn show_jslog(&mut self) {
        let log = self.tab().js_log.clone();
        if log.is_empty() {
            self.status.say(" JS log is empty for this page");
            return;
        }
        let pw = (self.cols.saturating_sub(4)).min(120);
        let ph = (self.rows.saturating_sub(4)).min(40);
        let px = (self.cols.saturating_sub(pw)) / 2;
        let py = (self.rows.saturating_sub(ph)) / 2;
        let mut popup = Pane::new(px, py, pw, ph, 252, 235);
        popup.border = true;
        popup.scroll = true;
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!(" {}", style::fg(&style::bold("JS console"), 81)));
        lines.push(String::new());
        for entry in &log {
            // Color by channel.
            let colored = if entry.starts_with("[error]") {
                style::fg(entry, 196)
            } else if entry.starts_with("[warn]") {
                style::fg(entry, 220)
            } else if entry.starts_with("[info]") {
                style::fg(entry, 81)
            } else {
                entry.clone()
            };
            lines.push(format!(" {}", colored));
        }
        lines.push(String::new());
        lines.push(style::fg(" j/k or ↓/↑ scroll · ESC / q close", 245));

        popup.set_text(&lines.join("\n"));
        popup.ix = 0;
        popup.border_refresh();
        popup.full_refresh();

        loop {
            let Some(key) = Input::getchr(None) else { continue };
            match key.as_str() {
                "ESC" | "q" => break,
                "j" | "DOWN" => {
                    let total = lines.len() as u16;
                    let view = ph.saturating_sub(2);
                    if (popup.ix as u16) + view < total {
                        popup.ix += 1;
                        popup.full_refresh();
                    }
                }
                "k" | "UP" => {
                    if popup.ix > 0 { popup.ix -= 1; popup.full_refresh(); }
                }
                "PgDOWN" | " " => {
                    let total = lines.len() as u16;
                    let view = ph.saturating_sub(2);
                    let step = view as usize;
                    if (popup.ix as u16) + view < total {
                        popup.ix = ((popup.ix + step) as u16).min(total.saturating_sub(view)) as usize;
                        popup.full_refresh();
                    }
                }
                "PgUP" => {
                    let view = ph.saturating_sub(2) as usize;
                    popup.ix = popup.ix.saturating_sub(view);
                    popup.full_refresh();
                }
                "g" => { popup.ix = 0; popup.full_refresh(); }
                "G" => {
                    let total = lines.len() as u16;
                    let view = ph.saturating_sub(2);
                    popup.ix = total.saturating_sub(view) as usize;
                    popup.full_refresh();
                }
                _ => {}
            }
        }
        self.render_all();
    }

    /// `:browse` (alias `:ff`) — open the current URL in Firefox,
    /// targeting the active set's Firefox profile if one is mapped.
    /// The escape hatch for sites whose JS scroll's minimal DOM
    /// can't run (Google login, Trusted-Types-heavy SPAs, anything
    /// that needs reCAPTCHA). Uses the same profile mapping as
    /// `:ffimport`, so multi-account separation is preserved.
    fn browse_in_firefox(&mut self) {
        let url = self.tab().url.clone();
        if url.is_empty() || url == "about:home" || url == "about:blank" {
            self.status.say(&style::fg(" Nothing to browse — open a URL first", 220));
            return;
        }
        let set_name = self.sets.get(self.current_set).cloned().unwrap_or_default();
        let profile = self.conf.firefox_profiles.get(&set_name).cloned();
        let mut cmd = std::process::Command::new("firefox");
        if let Some(p) = profile.filter(|p| !p.is_empty()) {
            cmd.arg("-P").arg(&p);
        }
        cmd.arg("--new-tab").arg(&url);
        cmd.stdin(std::process::Stdio::null())
           .stdout(std::process::Stdio::null())
           .stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(_) => self.status.say(&format!(" Opened in Firefox ({}): {}", set_name, url)),
            Err(e) => self.status.say(&style::fg(&format!(" firefox spawn failed: {}", e), 196)),
        }
    }

    /// `:ffimport` — refresh the active jar from the configured
    /// Firefox profile right now, without waiting for the next set
    /// switch. Useful after logging in to a site in Firefox.
    fn ffimport_cmd(&mut self) {
        let set_name = self.sets.get(self.current_set).cloned().unwrap_or_default();
        let profile = match self.conf.firefox_profiles.get(&set_name).cloned() {
            Some(p) if !p.is_empty() => p,
            _ => {
                self.status.say(&style::fg(
                    &format!(" No firefox_profiles entry for set \"{}\" — set one in ~/.scroll/config.json",
                        set_name),
                    220,
                ));
                return;
            }
        };
        match self.fetcher.import_firefox_cookies(&profile) {
            Some(n) => self.status.say(&format!(" Imported {} cookies from FF profile \"{}\"", n, profile)),
            None => self.status.say(&style::fg(
                &format!(" FF import failed: profile \"{}\" not found or db locked", profile),
                196,
            )),
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
        // Step 1: hard-reset the terminal's kitty graphics state.
        // `\x1b_Ga=d\x1b\\` (no `d=`) tells kitty/glass to free EVERY
        // image record. This unsticks glass when its IMG_SLOTS table
        // (capped at 32) has filled up and silently dropped placements.
        // Without this, force_redraw just re-runs the same churn that
        // wedged the state in the first place.
        use std::io::Write as _;
        print!("\x1b_Ga=d\x1b\\");
        let _ = std::io::stdout().flush();

        // Step 2: drop and re-create our glow::Display so the local
        // image_cache + active_ids tables also start clean. Without
        // this, scroll's cached IDs from before the reset would be
        // treated as live and skip re-transmission.
        if self.image_display.is_some() {
            self.image_display = Some(glow::Display::with_mode(&self.conf.image_mode));
        }

        Crust::clear_screen();
        self.render_all();
    }

    // --- Edit source ---

    fn edit_source(&mut self) {
        let url = self.tab().url.clone();
        if url.starts_with("about:") { return; }
        let result = self.fetcher.fetch(&url, "GET", None);
        if result.status != 200 { return; }

        let tmpfile = format!("/tmp/scroll_edit_{}.html", std::process::id());
        if std::fs::write(&tmpfile, &result.body).is_err() { return; }

        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".into());
        Crust::cleanup();
        let _ = std::process::Command::new(&editor).arg(&tmpfile).status();
        Crust::init();
        Crust::clear_screen();

        if let Ok(edited) = std::fs::read_to_string(&tmpfile) {
            let width = self.main.w as usize;
            let rendered = renderer::render_html(&edited, width, &url, &self.conf);
            self.tab_mut().content = rendered.text;
            self.tab_mut().title = rendered.title;
            self.tab_mut().links = rendered.links;
            self.tab_mut().forms = rendered.forms;
            self.tab_mut().images = rendered.images;
            self.tab_mut().site_bg = rendered.site_bg;
            self.tab_mut().site_fg = rendered.site_fg;
            let _ = std::fs::remove_file(&tmpfile);
        }
        self.render_all();
    }

    // --- Edit form field ---

    fn edit_form_field(&mut self) {
        if self.tab().forms.is_empty() {
            self.status.say(&style::fg(" No forms on page", 220));
            return;
        }
        let form = &self.tab().forms[0];
        let editable: Vec<(usize, String, String)> = form.fields.iter().enumerate()
            .filter(|(_, f)| f.field_type != "hidden")
            .map(|(i, f)| (i, f.name.clone(), f.value.clone()))
            .collect();
        if editable.is_empty() {
            self.status.say(&style::fg(" No editable fields", 220));
            return;
        }
        let field_idx = if editable.len() == 1 {
            editable[0].0
        } else {
            let names: Vec<String> = editable.iter().enumerate()
                .map(|(i, (_, name, _))| format!("{}: {}", i + 1, name))
                .collect();
            let input = self.prompt(&format!("Field ({}) #: ", names.join(", ")), "");
            if input.is_empty() { return; }
            match input.parse::<usize>() {
                Ok(n) if n >= 1 && n <= editable.len() => editable[n - 1].0,
                _ => return,
            }
        };

        let value = self.tab().forms[0].fields[field_idx].value.clone();
        let tmpfile = format!("/tmp/scroll_field_{}.txt", std::process::id());
        if std::fs::write(&tmpfile, &value).is_err() { return; }

        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".into());
        Crust::cleanup();
        let _ = std::process::Command::new(&editor).arg(&tmpfile).status();
        Crust::init();
        Crust::clear_screen();

        if let Ok(edited) = std::fs::read_to_string(&tmpfile) {
            let name = self.tabs[self.current_tab].forms[0].fields[field_idx].name.clone();
            self.tabs[self.current_tab].forms[0].fields[field_idx].value = edited.trim().to_string();
            self.status.say(&style::fg(&format!(" Set {} from editor", name), 82));
            let _ = std::fs::remove_file(&tmpfile);
        }
        self.render_all();
    }

    // --- Passwords ---

    fn show_password(&mut self) {
        let host = url::Url::parse(&self.tab().url).ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()));
        let Some(host) = host else {
            self.status.say(&style::fg(" No host", 220));
            return;
        };
        if let Some((user, pass)) = self.lookup_password(&host) {
            self.status.say(&format!(" {} - user: {} pass: {}", host, user, pass));
        } else {
            self.status.say(&style::fg(&format!(" No password for {}", host), 220));
        }
    }

    /// Resolve a (username, password) for `host`. Tries the external
    /// `password_command` first if configured; falls back to the
    /// internal `passwords.json` store. The external command is
    /// invoked as `<cmd> <host>` and must print two lines on stdout:
    /// `username\npassword\n`.
    fn lookup_password(&self, host: &str) -> Option<(String, String)> {
        if !self.conf.password_command.is_empty() {
            // Run the configured command. Allow shell expansion via
            // `sh -c "<cmd> <host>"` so users can drop a one-liner
            // (pipes, env, etc.) into the config without shellsplit.
            let escaped_host = host.replace('\'', "'\\''");
            let cmdline = format!("{} '{}'", self.conf.password_command, escaped_host);
            if let Ok(out) = std::process::Command::new("sh")
                .arg("-c").arg(&cmdline).output()
            {
                if out.status.success() {
                    let s = String::from_utf8_lossy(&out.stdout);
                    let mut lines = s.lines();
                    if let (Some(u), Some(p)) = (lines.next(), lines.next()) {
                        if !u.is_empty() && !p.is_empty() {
                            return Some((u.to_string(), p.to_string()));
                        }
                    }
                }
            }
        }
        self.passwords.get(host).cloned()
    }

    fn save_password_cmd(&mut self) {
        let host = url::Url::parse(&self.tab().url).ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()));
        let Some(host) = host else {
            self.status.say(&style::fg(" No host", 220));
            return;
        };
        let user = self.status.ask_with_bg(&format!("Username for {}: ", host), "", 18);
        if user.is_empty() { return; }
        let pass = self.status.ask_with_bg(&format!("Password for {}: ", host), "", 18);
        if pass.is_empty() { return; }
        self.passwords.insert(host.clone(), (user, pass));
        config::save_passwords(&self.passwords);
        self.status.say(&style::fg(&format!(" Password saved for {}", host), 82));
    }

    fn check_autofill(&mut self) {
        let has_pw_form = self.tab().forms.iter()
            .any(|f| f.fields.iter().any(|ff| ff.field_type == "password"));
        if !has_pw_form { return; }
        let host = url::Url::parse(&self.tab().url).ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()));
        if let Some(host) = host {
            if self.lookup_password(&host).is_some() {
                self.status.say(&style::fg(
                    &format!(" Credentials available for {}. Press 'C-F' to fill form.", host), 82));
            }
        }
    }

    // --- Ad blocking ---

    fn update_adblock(&mut self) {
        self.status.say(" Downloading adblock list...");
        let result = self.fetcher.fetch(
            "https://raw.githubusercontent.com/StevenBlack/hosts/master/hosts",
            "GET", None,
        );
        if result.status != 200 {
            self.status.say(&style::fg(" Adblock update failed", 196));
            return;
        }
        let mut domains = Vec::new();
        for line in result.body.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[0] == "0.0.0.0" && parts[1] != "0.0.0.0" {
                domains.push(parts[1].to_string());
            }
        }
        let _ = std::fs::write(config::adblock_path(), domains.join("\n"));
        let count = domains.len();
        self.adblock_domains = domains.into_iter().collect();
        self.status.say(&style::fg(&format!(" Adblock updated: {} domains blocked", count), 82));
    }

    fn is_blocked(&self, url: &str) -> bool {
        if self.adblock_domains.is_empty() { return false; }
        url::Url::parse(url).ok()
            .and_then(|u| u.host_str().map(|h| self.adblock_domains.contains(h)))
            .unwrap_or(false)
    }
}

// --- Helpers ---

fn load_adblock() -> HashSet<String> {
    std::fs::read_to_string(config::adblock_path())
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.trim().to_string())
        .collect()
}

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
