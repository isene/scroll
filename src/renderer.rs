use crate::tab::{Link, Form, FormField, ImageRef};
use crust::style;
use scraper::{Html, ElementRef, Node};

const IMG_RESERVE: usize = 10;

pub struct RenderResult {
    pub text: String,
    pub links: Vec<Link>,
    pub forms: Vec<Form>,
    pub images: Vec<ImageRef>,
    pub title: String,
    pub site_bg: Option<u8>,
    pub site_fg: Option<u8>,
}

pub fn render_html(html: &str, width: usize, base_url: &str, conf: &crate::config::Config) -> RenderResult {
    let doc = Html::parse_document(html);
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
        .select(&scraper::Selector::parse("title").unwrap())
        .next()
        .map(|t| t.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    let (site_bg, site_fg) = extract_site_colors(&doc, html);

    let body_sel = scraper::Selector::parse("body").unwrap();
    if let Some(body) = doc.select(&body_sel).next() {
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
                    for line in t.split('\n') {
                        ctx.append(line);
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
            let text = el.text().collect::<String>();
            ctx.append(&style::fg(&text, 186));
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
            let img_sel = scraper::Selector::parse("img").unwrap();
            let has_img = el.select(&img_sel).next().is_some();

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
            let alt = el.value().attr("alt").unwrap_or("image");
            let resolved = resolve_url(&ctx.base_url, src);
            let line = ctx.lines.len();
            ctx.images.push(ImageRef { src: resolved, alt: alt.to_string(), line, height: IMG_RESERVE });
            for _ in 0..IMG_RESERVE { ctx.lines.push(String::new()); }
        }
        "form" => {
            let action = el.value().attr("action").unwrap_or("").to_string();
            let method = el.value().attr("method").unwrap_or("get").to_uppercase();
            let line = ctx.lines.len();
            ctx.forms.push(Form { action: resolve_url(&ctx.base_url, &action), method, fields: Vec::new(), line });
            ctx.ensure_blank_line();
            ctx.append(&style::bold(&style::fg("[Form]", 208)));
            ctx.newline();
            walk_element(el, ctx);
            ctx.ensure_blank_line();
        }
        "input" => {
            let itype = el.value().attr("type").unwrap_or("text").to_lowercase();
            let name = el.value().attr("name").unwrap_or("").to_string();
            let value = el.value().attr("value").unwrap_or("").to_string();
            let placeholder = el.value().attr("placeholder").unwrap_or(&name).to_string();
            let line = ctx.lines.len();
            match itype.as_str() {
                "hidden" => {
                    if let Some(form) = ctx.forms.last_mut() {
                        form.fields.push(FormField { field_type: "hidden".into(), name, value, placeholder: String::new(), options: Vec::new(), line });
                    }
                }
                "submit" => {
                    let label = if value.is_empty() { "Submit" } else { &value };
                    ctx.append(&style::fb(label, 0, 252));
                    ctx.append(" ");
                }
                "password" => {
                    if let Some(form) = ctx.forms.last_mut() {
                        form.fields.push(FormField { field_type: "password".into(), name, value, placeholder: placeholder.clone(), options: Vec::new(), line });
                    }
                    ctx.append(&style::fg(&format!("[{}: \u{25CF}\u{25CF}\u{25CF}\u{25CF}]", placeholder), 252));
                    ctx.newline();
                }
                _ => {
                    if let Some(form) = ctx.forms.last_mut() {
                        form.fields.push(FormField { field_type: itype, name, value, placeholder: placeholder.clone(), options: Vec::new(), line });
                    }
                    ctx.append(&style::fg(&format!("[{}: ________]", placeholder), 252));
                    ctx.newline();
                }
            }
        }
        "select" => {
            let name = el.value().attr("name").unwrap_or("").to_string();
            let line = ctx.lines.len();
            let opt_sel = scraper::Selector::parse("option").unwrap();
            let options: Vec<(String, String)> = el.select(&opt_sel)
                .map(|opt| {
                    let val = opt.value().attr("value").unwrap_or("").to_string();
                    let label = opt.text().collect::<String>().trim().to_string();
                    (val, label)
                }).collect();
            if let Some(form) = ctx.forms.last_mut() {
                form.fields.push(FormField {
                    field_type: "select".into(), name: name.clone(),
                    value: options.first().map(|o| o.0.clone()).unwrap_or_default(),
                    placeholder: String::new(), options, line,
                });
            }
            ctx.append(&style::fg(&format!("[{} \u{25BC}]", name), 252));
            ctx.newline();
        }
        "textarea" => {
            let name = el.value().attr("name").unwrap_or("").to_string();
            let line = ctx.lines.len();
            let text = el.text().collect::<String>();
            if let Some(form) = ctx.forms.last_mut() {
                form.fields.push(FormField {
                    field_type: "textarea".into(), name: name.clone(),
                    value: text, placeholder: String::new(), options: Vec::new(), line,
                });
            }
            ctx.append(&style::fg(&format!("[{}: ________]", name), 252));
            ctx.newline();
        }
        "table" => {
            ctx.ensure_blank_line();
            render_table(el, ctx);
            ctx.ensure_blank_line();
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

fn resolve_url(base: &str, href: &str) -> String {
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
    let row_sel = scraper::Selector::parse("tr").unwrap();
    let cell_sel = scraper::Selector::parse("td, th").unwrap();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for row in el.select(&row_sel) {
        let cells: Vec<String> = row.select(&cell_sel)
            .map(|c| c.text().collect::<String>().trim().to_string())
            .collect();
        if !cells.is_empty() { rows.push(cells); }
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
        for (ri, row) in rows.iter().enumerate() {
            let mut line = "\u{2502}".to_string();
            for (ci, cell) in row.iter().enumerate() {
                let w = col_widths.get(ci).copied().unwrap_or(10);
                line.push_str(&format!(" {:<w$} \u{2502}", cell, w = w));
            }
            ctx.append(&line); ctx.newline();
            if ri == 0 {
                let sep: String = col_widths.iter()
                    .map(|w| format!("\u{253C}{}", "\u{2500}".repeat(w + 2)))
                    .collect::<Vec<_>>().join("");
                ctx.append(&format!("\u{253C}{}", sep)); ctx.newline();
            }
        }
    } else {
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                if !cell.is_empty() { ctx.append(&format!("{}: {}", i, cell)); ctx.newline(); }
            }
            ctx.newline();
        }
    }
}

fn extract_site_colors(doc: &Html, _html: &str) -> (Option<u8>, Option<u8>) {
    let body_sel = scraper::Selector::parse("body").unwrap();
    if let Some(body) = doc.select(&body_sel).next() {
        if let Some(bg) = body.value().attr("bgcolor") {
            return (parse_color(bg), body.value().attr("text").and_then(parse_color));
        }
    }
    let style_sel = scraper::Selector::parse("style").unwrap();
    for s in doc.select(&style_sel) {
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
        let hex = &s[1..];
        let (r, g, b) = if hex.len() == 6 {
            (u8::from_str_radix(&hex[0..2], 16).ok()?, u8::from_str_radix(&hex[2..4], 16).ok()?, u8::from_str_radix(&hex[4..6], 16).ok()?)
        } else if hex.len() == 3 {
            (u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?, u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?, u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?)
        } else { return None; };
        return Some(rgb_to_xterm(r, g, b));
    }
    match s.to_lowercase().as_str() {
        "white" => Some(255), "black" => Some(0), "red" => Some(196),
        "green" => Some(46), "blue" => Some(21), "yellow" => Some(226),
        _ => None,
    }
}

fn rgb_to_xterm(r: u8, g: u8, b: u8) -> u8 {
    if r == g && g == b {
        if r < 8 { return 16; }
        if r > 248 { return 231; }
        return (((r as u16 - 8) * 24 / 247) as u8) + 232;
    }
    16 + 36 * (r as u8 / 51) + 6 * (g as u8 / 51) + (b as u8 / 51)
}
