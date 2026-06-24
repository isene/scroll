use crate::tab::{Link, Form, FormField, ImageRef};
use crust::style;
use scraper::{Html, ElementRef, Node};
use std::sync::OnceLock;

const IMG_RESERVE: usize = 10;

// Pre-compiled CSS selectors (compiled once, reused across all renders)
fn sel_title() -> &'static scraper::Selector { static S: OnceLock<scraper::Selector> = OnceLock::new(); S.get_or_init(|| scraper::Selector::parse("title").unwrap()) }
fn sel_body() -> &'static scraper::Selector { static S: OnceLock<scraper::Selector> = OnceLock::new(); S.get_or_init(|| scraper::Selector::parse("body").unwrap()) }
fn sel_img() -> &'static scraper::Selector { static S: OnceLock<scraper::Selector> = OnceLock::new(); S.get_or_init(|| scraper::Selector::parse("img").unwrap()) }
fn sel_option() -> &'static scraper::Selector { static S: OnceLock<scraper::Selector> = OnceLock::new(); S.get_or_init(|| scraper::Selector::parse("option").unwrap()) }
fn sel_tr() -> &'static scraper::Selector { static S: OnceLock<scraper::Selector> = OnceLock::new(); S.get_or_init(|| scraper::Selector::parse("tr").unwrap()) }
/// Children-only selectors for nested-table heuristics and row pickup.
fn sel_inner_table() -> &'static scraper::Selector { static S: OnceLock<scraper::Selector> = OnceLock::new(); S.get_or_init(|| scraper::Selector::parse("table").unwrap()) }
fn sel_td_th() -> &'static scraper::Selector { static S: OnceLock<scraper::Selector> = OnceLock::new(); S.get_or_init(|| scraper::Selector::parse("td, th").unwrap()) }
fn sel_th() -> &'static scraper::Selector { static S: OnceLock<scraper::Selector> = OnceLock::new(); S.get_or_init(|| scraper::Selector::parse("th").unwrap()) }

/// Regex for "elements rendered invisible by CSS." Single-level only
/// (the body uses `[^<]` so nested tags don't accidentally match).
/// Rust's regex crate has no backreferences, so we close-tag with the
/// same alternation set instead of pinning to the open-tag's name.
fn hidden_element_re() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(
            r#"(?is)<(?:div|span|p|td|tr|table|section)\b[^>]*\bstyle\s*=\s*"[^"]*\b(?:display\s*:\s*none|visibility\s*:\s*hidden|opacity\s*:\s*0(?:\.0+)?|max-height\s*:\s*0(?:px)?|max-width\s*:\s*0(?:px)?)[^"]*"[^>]*>[^<]*</\s*(?:div|span|p|td|tr|table|section)\s*>"#
        ).expect("hidden-element regex must compile")
    })
}
fn sel_style() -> &'static scraper::Selector { static S: OnceLock<scraper::Selector> = OnceLock::new(); S.get_or_init(|| scraper::Selector::parse("style").unwrap()) }

pub struct RenderResult {
    pub text: String,
    pub links: Vec<Link>,
    pub forms: Vec<Form>,
    pub images: Vec<ImageRef>,
    pub title: String,
    pub site_bg: Option<u8>,
    pub site_fg: Option<u8>,
}

/// Highlight a specific link index in rendered content by reversing only the link text
pub fn highlight_link(content: &str, links: &[Link], focus_idx: usize) -> String {
    if focus_idx >= links.len() { return content.to_string(); }
    let target_line = links[focus_idx].line;
    let link_text = &links[focus_idx].text;
    let link_idx = links[focus_idx].index;
    // Build the decorated link string that appears in rendered content: "text[N]"
    let marker = format!("[{}]", link_idx);
    content.lines().enumerate().map(|(i, line)| {
        if i == target_line {
            let plain = crust::strip_ansi(line);
            if plain.contains(link_text.as_str()) {
                // Replace the link text with reversed version, keeping rest of line intact
                // Find the link text in the plain version to locate it
                if let Some(pos) = plain.find(link_text.as_str()) {
                    // Find end of link marker (text + [N])
                    let end = if let Some(mp) = plain[pos..].find(&marker) {
                        pos + mp + marker.len()
                    } else {
                        pos + link_text.len()
                    };
                    // Rebuild: prefix (with ANSI) + reversed link + suffix (with ANSI)
                    // Use character-level rebuild to handle ANSI codes
                    let mut result = String::new();
                    let mut visible = 0;
                    let mut in_escape = false;
                    let mut in_link = false;
                    let mut link_buf = String::new();
                    for ch in line.chars() {
                        if ch == '\x1b' { in_escape = true; }
                        if in_escape {
                            if in_link { link_buf.push(ch); } else { result.push(ch); }
                            if ch.is_ascii_alphabetic() { in_escape = false; }
                            continue;
                        }
                        if visible == pos && !in_link {
                            in_link = true;
                        }
                        if in_link {
                            link_buf.push(ch);
                            visible += 1;
                            if visible == end {
                                result.push_str(&style::reverse(&link_buf));
                                in_link = false;
                            }
                        } else {
                            result.push(ch);
                            visible += 1;
                        }
                    }
                    if in_link {
                        result.push_str(&style::reverse(&link_buf));
                    }
                    return result;
                }
            }
            line.to_string()
        } else {
            line.to_string()
        }
    }).collect::<Vec<_>>().join("\n")
}

/// Reverse-highlight a known plain `token` (a form field's render_token,
/// e.g. "[email: ________]" or "[Unsubscribe]") on `target_line`,
/// ANSI-aware. Same rebuild approach as highlight_link but for an exact
/// literal token rather than a link's text+marker. No-op if the token
/// isn't found on that line.
pub fn highlight_token(content: &str, target_line: usize, token: &str) -> String {
    if token.is_empty() { return content.to_string(); }
    content.lines().enumerate().map(|(i, line)| {
        if i != target_line { return line.to_string(); }
        let plain = crust::strip_ansi(line);
        let Some(byte_pos) = plain.find(token) else { return line.to_string(); };
        // Work in visible-char counts (tokens contain multi-byte ● / ▼).
        let char_pos = plain[..byte_pos].chars().count();
        let char_end = char_pos + token.chars().count();
        let mut result = String::new();
        let mut visible = 0;
        let mut in_escape = false;
        let mut in_tok = false;
        let mut buf = String::new();
        for ch in line.chars() {
            if ch == '\x1b' { in_escape = true; }
            if in_escape {
                if in_tok { buf.push(ch); } else { result.push(ch); }
                if ch.is_ascii_alphabetic() { in_escape = false; }
                continue;
            }
            if visible == char_pos && !in_tok { in_tok = true; }
            if in_tok {
                buf.push(ch);
                visible += 1;
                if visible == char_end {
                    result.push_str(&style::reverse(&buf));
                    in_tok = false;
                }
            } else {
                result.push(ch);
                visible += 1;
            }
        }
        if in_tok { result.push_str(&style::reverse(&buf)); }
        result
    }).collect::<Vec<_>>().join("\n")
}

pub fn render_html(html: &str, width: usize, base_url: &str, conf: &crate::config::Config) -> RenderResult {
    // Strip CSS-hidden elements before parsing. Newsletter HTML
    // (Substack, TLDR, GitHub digest, etc.) ships an inbox-preview
    // blob hidden via `display:none` / `max-height:0` / `opacity:0`
    // — the same content as the first headline, padded with
    // soft-hyphens and ZWNBSPs. Without this strip the user sees
    // both the preview AND the body, producing visible duplication.
    // Same regex shape kastrup uses in `html_to_text`. Single-level:
    // matches a `<div|span|p|td|tr|table|section>` whose style
    // contains the offending property and whose body has no nested
    // tags (so we don't accidentally swallow real content).
    let stripped = hidden_element_re().replace_all(html, "");
    let doc = Html::parse_document(&stripped);
    let mut ctx = RenderContext {
        lines: Vec::new(),
        current_line: String::new(),
        col: 0,
        width,
        links: Vec::new(),
        forms: Vec::new(),
        images: Vec::new(),
        link_index: 0,
        indent: 0,
        in_pre: false,
        list_stack: Vec::new(),
        base_url: base_url.to_string(),
        conf,
    };

    let title = doc.root_element()
        .select(sel_title())
        .next()
        .map(|t| t.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    // Skip site-color extraction for `file://` URLs. Those are
    // typically locally-generated documents (kastrup-launched email
    // HTML, exported reports, etc.) whose CSS is calibrated for an
    // email client / browser white background. In a terminal, the
    // user's configured content fg/bg gives much better contrast
    // than whatever #fafafa-on-#333 the email author chose.
    let (site_bg, site_fg) = if base_url.starts_with("file://") {
        (None, None)
    } else {
        extract_site_colors(&doc, html)
    };

    if let Some(body) = doc.select(sel_body()).next() {
        walk_element(&body, &mut ctx);
    } else {
        walk_element(&doc.root_element(), &mut ctx);
    }

    if !ctx.current_line.is_empty() {
        ctx.lines.push(std::mem::take(&mut ctx.current_line));
    }

    RenderResult {
        text: ctx.lines.join("\n"),
        links: ctx.links,
        forms: ctx.forms,
        images: ctx.images,
        title,
        site_bg,
        site_fg,
    }
}

struct RenderContext<'a> {
    lines: Vec<String>,
    current_line: String,
    col: usize,
    width: usize,
    links: Vec<Link>,
    forms: Vec<Form>,
    images: Vec<ImageRef>,
    link_index: usize,
    indent: usize,
    in_pre: bool,
    list_stack: Vec<ListType>,
    base_url: String,
    conf: &'a crate::config::Config,
}

enum ListType { Unordered, Ordered(usize) }

fn walk_element(el: &ElementRef, ctx: &mut RenderContext) {
    for child in el.children() {
        match child.value() {
            Node::Text(text) => {
                let t = text.text.as_ref();
                if ctx.in_pre {
                    // Preserve preformatted whitespace + the pre/code colour
                    // (186), so the <pre> child-walk doesn't lose styling for
                    // plain pre text (links inside still get their own colour).
                    for line in t.split('\n') {
                        ctx.append(&style::fg(line, 186));
                        ctx.newline();
                    }
                } else {
                    let collapsed = collapse_whitespace(t);
                    if !collapsed.is_empty() {
                        ctx.word_wrap(&collapsed);
                    }
                }
            }
            Node::Element(_) => {
                if let Some(child_el) = ElementRef::wrap(child) {
                    handle_element(&child_el, ctx);
                }
            }
            _ => {}
        }
    }
}

fn handle_element(el: &ElementRef, ctx: &mut RenderContext) {
    let tag = el.value().name.local.as_ref();

    if matches!(tag, "script" | "style" | "noscript" | "svg" | "head" | "meta" | "link") {
        return;
    }

    match tag {
        "br" => ctx.newline(),
        "hr" => {
            ctx.ensure_blank_line();
            ctx.append(&style::fg(&"\u{2500}".repeat(ctx.width.min(60)), 240));
            ctx.newline();
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            ctx.ensure_blank_line();
            let text = el.text().collect::<String>().trim().to_string();
            let color = match tag {
                "h1" => ctx.conf.c_h1 as u8,
                "h2" => ctx.conf.c_h2 as u8,
                "h3" => ctx.conf.c_h3 as u8,
                _ => 252,
            };
            ctx.append(&style::bold(&style::fg(&text, color)));
            ctx.newline();
            if tag == "h1" {
                ctx.append(&style::fg(&"\u{2550}".repeat(text.len().min(ctx.width)), color));
                ctx.newline();
            }
            ctx.newline();
        }
        "p" | "div" | "section" | "article" | "main" | "header" | "footer" | "nav" | "aside" | "figure" | "figcaption" => {
            ctx.ensure_blank_line();
            walk_element(el, ctx);
            ctx.ensure_blank_line();
        }
        "blockquote" => {
            ctx.ensure_blank_line();
            ctx.indent += 2;
            walk_element(el, ctx);
            ctx.indent -= 2;
            ctx.ensure_blank_line();
        }
        "pre" => {
            ctx.ensure_blank_line();
            let was_pre = ctx.in_pre;
            ctx.in_pre = true;
            // Walk children (not el.text()) so inline elements inside <pre> —
            // notably <a> — register as real, followable links instead of being
            // flattened to plain text. in_pre preserves the whitespace.
            walk_element(el, ctx);
            ctx.in_pre = was_pre;
            ctx.ensure_blank_line();
        }
        "code" => {
            let text = el.text().collect::<String>();
            ctx.append(&style::fg(&text, 186));
        }
        "ul" => {
            ctx.ensure_blank_line();
            ctx.list_stack.push(ListType::Unordered);
            ctx.indent += 2;
            walk_element(el, ctx);
            ctx.indent -= 2;
            ctx.list_stack.pop();
        }
        "ol" => {
            ctx.ensure_blank_line();
            ctx.list_stack.push(ListType::Ordered(1));
            ctx.indent += 2;
            walk_element(el, ctx);
            ctx.indent -= 2;
            ctx.list_stack.pop();
        }
        "li" => {
            ctx.newline();
            let bullet = match ctx.list_stack.last_mut() {
                Some(ListType::Unordered) => "\u{2022} ".to_string(),
                Some(ListType::Ordered(n)) => { let s = format!("{}. ", n); *n += 1; s }
                None => "\u{2022} ".to_string(),
            };
            ctx.append(&bullet);
            walk_element(el, ctx);
        }
        "a" => {
            let href = el.value().attr("href").unwrap_or("");
            if href.is_empty() || href == "#" || href.starts_with("javascript:") {
                walk_element(el, ctx);
                return;
            }
            let resolved = resolve_url(&ctx.base_url, href);

            // Check if link contains an image
            let has_img = el.select(sel_img()).next().is_some();

            // Ensure space before link if text precedes it
            if ctx.col > ctx.indent && !ctx.current_line.ends_with(' ') {
                ctx.append(" ");
            }

            if has_img {
                // Render the image (reserved space) with a link reference
                for child_node in el.children() {
                    if let Some(child_el) = ElementRef::wrap(child_node) {
                        if child_el.value().name.local.as_ref() == "img" {
                            handle_element(&child_el, ctx);
                        }
                    }
                }
                // Add link reference after image
                ctx.link_index += 1;
                let idx = ctx.link_index;
                let line = ctx.lines.len();
                let text = el.text().collect::<String>().trim().to_string();
                let display = if text.is_empty() { "image link".to_string() } else { text };
                ctx.links.push(Link { index: idx, href: resolved, text: display.clone(), line });
                ctx.append(&style::fg(&format!("[{}]", idx), ctx.conf.c_link_num as u8));
            } else {
                let text = el.text().collect::<String>().trim().to_string();
                let display = if text.is_empty() { href.to_string() } else { text };
                ctx.link_index += 1;
                let idx = ctx.link_index;
                let line = ctx.lines.len();
                ctx.links.push(Link { index: idx, href: resolved, text: display.clone(), line });
                ctx.append(&format!("{}{}",
                    style::underline(&style::fg(&display, ctx.conf.c_link as u8)),
                    style::fg(&format!("[{}]", idx), ctx.conf.c_link_num as u8)));
            }
        }
        "strong" | "b" => {
            let text = el.text().collect::<String>();
            ctx.append(&style::bold(&text));
        }
        "em" | "i" => {
            let text = el.text().collect::<String>();
            ctx.append(&style::italic(&text));
        }
        "u" => {
            let text = el.text().collect::<String>();
            ctx.append(&style::underline(&text));
        }
        "s" | "del" | "strike" => {
            let text = el.text().collect::<String>();
            ctx.append(&style::fg(&text, 240));
        }
        "img" => {
            let src = el.value().attr("src").or_else(|| el.value().attr("data-src")).unwrap_or("");
            if src.is_empty() { return; }
            let resolved = resolve_url(&ctx.base_url, src);
            // Pick a reserve height proportional to the image's
            // declared dimensions. Marketing emails / Outlook are
            // *littered* with 1x1 trackers, 100x1 / 1x83 layout
            // spacers, and small icons — reserving the default 10
            // lines for each turns a 22-image message into 220 blank
            // lines of dead space. Concretely:
            //   - any dimension ≤ 5 → 0 lines  (tracker / spacer)
            //   - either ≤ 30       → 1 line   (inline icon)
            //   - else height known → ⌈height / 20⌉  capped at IMG_RESERVE
            //   - else              → IMG_RESERVE  (content image)
            let w = el.value().attr("width").and_then(|s| s.parse::<u32>().ok());
            let h = el.value().attr("height").and_then(|s| s.parse::<u32>().ok());
            let small = w.map(|x| x <= 5).unwrap_or(false)
                     || h.map(|x| x <= 5).unwrap_or(false);
            let inline_icon = w.map(|x| x <= 30).unwrap_or(false)
                           || h.map(|x| x <= 30).unwrap_or(false);
            let reserve = if small { 0 }
                else if inline_icon { 1 }
                else if let Some(hh) = h { ((hh as usize + 19) / 20).min(IMG_RESERVE).max(1) }
                else { IMG_RESERVE };
            if reserve == 0 { return; }
            if !ctx.current_line.is_empty() { ctx.newline(); }
            let line = ctx.lines.len();
            ctx.images.push(ImageRef { src: resolved, line, height: reserve });
            for _ in 0..reserve { ctx.lines.push(String::new()); }
            ctx.col = 0;
        }
        "form" => {
            let action = el.value().attr("action").unwrap_or("").to_string();
            let method = el.value().attr("method").unwrap_or("get").to_uppercase();
            ctx.forms.push(Form { action: resolve_url(&ctx.base_url, &action), method, fields: Vec::new() });
            ctx.ensure_blank_line();
            ctx.append(&style::bold(&style::fg("[Form]", 208)));
            ctx.newline();
            walk_element(el, ctx);
            ctx.ensure_blank_line();
        }
        "input" => {
            let itype = el.value().attr("type").unwrap_or("text").to_lowercase();
            let name = el.value().attr("name").unwrap_or("").to_string();
            let id = el.value().attr("id").unwrap_or("").to_string();
            let value = el.value().attr("value").unwrap_or("").to_string();
            let placeholder = el.value().attr("placeholder").unwrap_or(&name).to_string();
            let line = ctx.lines.len();
            match itype.as_str() {
                "hidden" => {
                    if let Some(form) = ctx.forms.last_mut() {
                        form.fields.push(FormField { field_type: "hidden".into(), name, id, value, line, ..Default::default() });
                    }
                }
                "submit" => {
                    let label = if value.is_empty() { "Submit".to_string() } else { value };
                    let token = format!("[{}]", label);
                    if let Some(form) = ctx.forms.last_mut() {
                        form.fields.push(FormField { field_type: "submit".into(), name, id, value: label.clone(), line, render_token: token.clone(), ..Default::default() });
                    }
                    ctx.append(&style::fb(&token, 0, 252));
                    ctx.append(" ");
                }
                "password" => {
                    let token = format!("[{}: \u{25CF}\u{25CF}\u{25CF}\u{25CF}]", placeholder);
                    if let Some(form) = ctx.forms.last_mut() {
                        form.fields.push(FormField { field_type: "password".into(), name, id, value, placeholder: placeholder.clone(), line, render_token: token.clone(), ..Default::default() });
                    }
                    ctx.append(&style::fg(&token, 252));
                    ctx.newline();
                }
                _ => {
                    let token = format!("[{}: ________]", placeholder);
                    if let Some(form) = ctx.forms.last_mut() {
                        form.fields.push(FormField { field_type: itype, name, id, value, placeholder: placeholder.clone(), line, render_token: token.clone(), ..Default::default() });
                    }
                    ctx.append(&style::fg(&token, 252));
                    ctx.newline();
                }
            }
        }
        "select" => {
            let name = el.value().attr("name").unwrap_or("").to_string();
            let id = el.value().attr("id").unwrap_or("").to_string();
            let line = ctx.lines.len();
            let options: Vec<(String, String)> = el.select(sel_option())
                .map(|opt| {
                    let val = opt.value().attr("value").unwrap_or("").to_string();
                    let label = opt.text().collect::<String>().trim().to_string();
                    (val, label)
                }).collect();
            let token = format!("[{} \u{25BC}]", name);
            if let Some(form) = ctx.forms.last_mut() {
                form.fields.push(FormField {
                    field_type: "select".into(), name: name.clone(), id,
                    value: options.first().map(|o| o.0.clone()).unwrap_or_default(),
                    options, line, render_token: token.clone(), ..Default::default()
                });
            }
            ctx.append(&style::fg(&token, 252));
            ctx.newline();
        }
        "textarea" => {
            let name = el.value().attr("name").unwrap_or("").to_string();
            let id = el.value().attr("id").unwrap_or("").to_string();
            let line = ctx.lines.len();
            let text = el.text().collect::<String>();
            let token = format!("[{}: ________]", name);
            if let Some(form) = ctx.forms.last_mut() {
                form.fields.push(FormField {
                    field_type: "textarea".into(), name: name.clone(), id,
                    value: text, line, render_token: token.clone(), ..Default::default()
                });
            }
            ctx.append(&style::fg(&token, 252));
            ctx.newline();
        }
        "button" => {
            // A <button> (default type=submit) is a form submitter. Render
            // it as a focusable [Label] field so TAB reaches it and ENTER
            // submits. Don't recurse into its children — that would
            // double-render the label text.
            let btype = el.value().attr("type").unwrap_or("submit").to_lowercase();
            let label_text = el.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
            let value = el.value().attr("value").unwrap_or("").to_string();
            let label = if !label_text.is_empty() { label_text }
                        else if !value.is_empty() { value }
                        else { "Submit".to_string() };
            let name = el.value().attr("name").unwrap_or("").to_string();
            let id = el.value().attr("id").unwrap_or("").to_string();
            let line = ctx.lines.len();
            let token = format!("[{}]", label);
            // type=button / type=reset don't submit; render but don't make
            // them submit targets.
            if btype != "button" && btype != "reset" {
                if let Some(form) = ctx.forms.last_mut() {
                    form.fields.push(FormField { field_type: "submit".into(), name, id, value: label.clone(), line, render_token: token.clone(), ..Default::default() });
                }
            }
            ctx.append(&style::fb(&token, 0, 252));
            ctx.append(" ");
        }
        "table" => {
            // Heuristic: a real data table has `<th>` AND no nested
            // `<table>`. Outlook / marketing email uses `<th>` inside
            // layout tables for alignment, and those layouts are
            // arbitrarily nested — descending into them with
            // render_table's descendant `<tr>` selector duplicates
            // every inner row at the outer level and rebuilds the
            // entire body of the message N times. Fall through to
            // walk_element for anything that looks like layout so
            // each descendant is visited exactly once.
            let has_nested_table = el.select(sel_inner_table()).next().is_some();
            if !has_nested_table && el.select(sel_th()).next().is_some() {
                ctx.ensure_blank_line();
                render_table(el, ctx);
                ctx.ensure_blank_line();
            } else {
                walk_element(el, ctx);
            }
        }
        "iframe" => {
            let src = el.value().attr("src").unwrap_or("");
            if src.contains("youtube.com/embed/") || src.contains("youtu.be/") {
                if let Some(vid_id) = extract_youtube_id(src) {
                    let yt_url = format!("https://www.youtube.com/watch?v={}", vid_id);
                    ctx.link_index += 1;
                    let idx = ctx.link_index;
                    let line = ctx.lines.len();
                    ctx.links.push(Link { index: idx, href: yt_url, text: "YouTube Video".into(), line });
                    ctx.append(&format!("{} {}",
                        style::fg("\u{25B6} YouTube Video", 196),
                        style::fg(&format!("[{}]", idx), ctx.conf.c_link_num as u8)));
                    ctx.newline();
                }
            }
        }
        "span" | "label" | "small" | "sup" | "sub" | "abbr" | "time" | "mark" | "q" | "cite" | "dfn" | "var" | "samp" | "kbd" | "data" | "ruby" | "rt" | "rp" | "bdi" | "bdo" | "wbr" => {
            walk_element(el, ctx);
        }
        _ => {
            walk_element(el, ctx);
        }
    }
}

impl RenderContext<'_> {
    fn append(&mut self, text: &str) {
        if self.current_line.is_empty() && self.indent > 0 {
            self.current_line = " ".repeat(self.indent);
            self.col = self.indent;
        }
        self.current_line.push_str(text);
        self.col += crust::display_width(text);
    }

    fn newline(&mut self) {
        self.lines.push(std::mem::take(&mut self.current_line));
        self.col = 0;
    }

    fn ensure_blank_line(&mut self) {
        if !self.current_line.is_empty() { self.newline(); }
        if !self.lines.is_empty() && !self.lines.last().map(|l| l.is_empty()).unwrap_or(true) {
            self.lines.push(String::new());
        }
    }

    fn word_wrap(&mut self, text: &str) {
        for word in text.split_whitespace() {
            let wlen = crust::display_width(word);
            if self.col + wlen + 1 > self.width && self.col > self.indent {
                self.newline();
            }
            if self.col > self.indent { self.append(" "); }
            self.append(word);
        }
    }
}

pub fn resolve_url(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") || href.starts_with("file://") {
        return href.to_string();
    }
    if href.starts_with("//") { return format!("https:{}", href); }
    if let Ok(base_url) = url::Url::parse(base) {
        if let Ok(resolved) = base_url.join(href) {
            return resolved.to_string();
        }
    }
    href.to_string()
}

fn extract_youtube_id(src: &str) -> Option<String> {
    let pos = src.find("/embed/")?;
    let rest = &src[pos + 7..];
    let id = rest.split(&['?', '&', '/'][..]).next()?;
    if id.is_empty() { None } else { Some(id.to_string()) }
}

fn collapse_whitespace(s: &str) -> String {
    let mut result = String::new();
    let mut last_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_ws && !result.is_empty() { result.push(' '); }
            last_ws = true;
        } else {
            result.push(c);
            last_ws = false;
        }
    }
    result
}

fn render_table(el: &ElementRef, ctx: &mut RenderContext) {
    // Caller guarantees a `<th>`-bearing data table. Layout tables
    // (no `<th>`) are short-circuited in handle_element so they never
    // hit this path — flattening their anchors to .text() would
    // strip links and the descendant-selector walk would explode on
    // nested-table HTML.
    let mut rows: Vec<Vec<String>> = Vec::new();
    for row in el.select(sel_tr()) {
        let cells: Vec<String> = row.select(sel_td_th())
            .map(|c| {
                // `.text()` returns concatenated descendant text with
                // the source HTML's literal whitespace + newlines
                // preserved. Outlook / marketing HTML is pretty-
                // printed, so a single `<td>` worth of text often
                // contains many `\n` plus runs of indentation spaces.
                // `.trim()` only strips leading/trailing — internal
                // whitespace would survive and break the column
                // widths in the box-drawn path AND emit "spaces-only
                // lines" in the vertical-layout fallback. Collapse
                // every internal whitespace run to a single space.
                let raw: String = c.text().collect();
                collapse_whitespace(&raw).trim().to_string()
            })
            .collect();
        // Drop rows where every cell collapsed to empty — they'd
        // otherwise emit a stray `ctx.newline()` in the vertical
        // path below for nothing.
        if cells.iter().any(|c| !c.is_empty()) { rows.push(cells); }
    }
    if rows.is_empty() { return; }
    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut col_widths = vec![0usize; num_cols];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            if i < col_widths.len() { col_widths[i] = col_widths[i].max(cell.len()); }
        }
    }
    let total: usize = col_widths.iter().sum::<usize>() + num_cols * 3 + 1;
    if total <= ctx.width {
        // Top border
        let top: String = col_widths.iter().enumerate().map(|(i, w)| {
            let bar = "\u{2500}".repeat(w + 2);
            if i == 0 { format!("\u{250C}{}", bar) }
            else { format!("\u{252C}{}", bar) }
        }).collect::<Vec<_>>().join("");
        ctx.append(&format!("{}\u{2510}", top)); ctx.newline();

        for (ri, row) in rows.iter().enumerate() {
            let mut line = "\u{2502}".to_string();
            for (ci, cell) in row.iter().enumerate() {
                let w = col_widths.get(ci).copied().unwrap_or(10);
                line.push_str(&format!(" {:<w$} \u{2502}", cell, w = w));
            }
            ctx.append(&line); ctx.newline();

            // Separator after header row
            if ri == 0 && rows.len() > 1 {
                let sep: String = col_widths.iter().enumerate().map(|(i, w)| {
                    let bar = "\u{2500}".repeat(w + 2);
                    if i == 0 { format!("\u{251C}{}", bar) }
                    else { format!("\u{253C}{}", bar) }
                }).collect::<Vec<_>>().join("");
                ctx.append(&format!("{}\u{2524}", sep)); ctx.newline();
            }
        }

        // Bottom border
        let bot: String = col_widths.iter().enumerate().map(|(i, w)| {
            let bar = "\u{2500}".repeat(w + 2);
            if i == 0 { format!("\u{2514}{}", bar) }
            else { format!("\u{2534}{}", bar) }
        }).collect::<Vec<_>>().join("");
        ctx.append(&format!("{}\u{2518}", bot)); ctx.newline();
    } else {
        // Vertical layout for wide tables
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                if !cell.is_empty() { ctx.append(&format!("{}: {}", i, cell)); ctx.newline(); }
            }
            ctx.newline();
        }
    }
}

fn extract_site_colors(doc: &Html, _html: &str) -> (Option<u8>, Option<u8>) {
    if let Some(body) = doc.select(sel_body()).next() {
        if let Some(bg) = body.value().attr("bgcolor") {
            return (parse_color(bg), body.value().attr("text").and_then(parse_color));
        }
    }
    for s in doc.select(sel_style()) {
        let css = s.text().collect::<String>();
        if let Some(bg) = extract_css_color(&css, "background-color")
            .or_else(|| extract_css_color(&css, "background")) {
            let fg = extract_css_color(&css, "color");
            return (parse_color(&bg), fg.and_then(|f| parse_color(&f)));
        }
    }
    (None, None)
}

fn extract_css_color(css: &str, prop: &str) -> Option<String> {
    let pos = css.find(&format!("{}:", prop))?;
    let rest = &css[pos + prop.len() + 1..];
    let end = rest.find(&[';', '}'][..])?;
    Some(rest[..end].trim().to_string())
}

fn parse_color(s: &str) -> Option<u8> {
    let s = s.trim();
    if s.starts_with('#') {
        let (r, g, b) = crust::style::parse_hex_color(s)?;
        return Some(crust::style::rgb_to_xterm(r, g, b));
    }
    match s.to_lowercase().as_str() {
        "white" => Some(255), "black" => Some(0), "red" => Some(196),
        "green" => Some(46), "blue" => Some(21), "yellow" => Some(226),
        _ => None,
    }
}
