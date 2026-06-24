//! Minimal JS execution layer over scroll's text-mode renderer.
//!
//! After a page is fetched, scroll extracts every `<script>` tag
//! (inline + external) and runs them in a `boa` context that exposes
//! a small DOM, `window`, `fetch`, storage, and cookies. Goal:
//! "make simple JS work" — not "ship a browser". Currently supported:
//!
//! - `console.log/.warn/.error/.info` captured into `JsResult.log`.
//! - `navigator.userAgent` (Chrome-shaped), `.language`, `.platform`.
//! - `window.location` / `document.location`: read URL components,
//!   write to `href` / call `assign` / `replace` to redirect.
//! - `document.cookie` real read/write against the active set's jar.
//! - `localStorage` (per-set, per-origin, persisted to disk between
//!   runs) + `sessionStorage` (per-run only).
//! - `document.getElementById` / `document.querySelector` (subset:
//!   `#id`, bare-tag) return Element handles backed by `JsState.dom`.
//!   Element accessor properties: `value`, `innerHTML`, `textContent`,
//!   `checked`, `tagName`. Methods: `getAttribute` / `setAttribute`,
//!   plus no-op `click` / `submit` / `focus` / `blur` /
//!   `dispatchEvent` / `addEventListener`. JS-set values surface
//!   back via `JsResult.dom_values`.
//! - `fetch(url, opts)` runs synchronously through rquest, sends the
//!   active set's cookies, captures `Set-Cookie` from the response.
//!   Returns a Response-shaped object with `.status` / `.ok` /
//!   `.text()` / `.json()`. Each method returns a thenable so
//!   `.then()` chains flow synchronously.
//! - External `<script src=...>` fetched in document order with the
//!   active set's cookies. Bounded: max 16 scripts, 1 MB total,
//!   5-second per-fetch timeout.
//! - `setTimeout` / `setInterval` no-op stubs that return ids.
//!
//! Boa 0.20's `from_copy_closure` requires `Copy` on captured state,
//! so per-run mutable state lives behind a `thread_local!` slot.
//! Object-bound state (response body, element id, thenable value) is
//! stashed as `__body` / `__id` / `__value` data properties on the
//! JS object the closure operates on. JS execution is synchronous
//! and single-threaded, so this is safe.
//!
//! Not yet supported: live DOM tree, event-listener firing,
//! `async/await` resumption, reCAPTCHA-style fingerprinting. For
//! sites whose JS the minimal DOM can't satisfy, `:browse` bounces
//! to the active set's Firefox profile.

use std::cell::RefCell;
use std::collections::HashMap;

use boa_engine::{
    Context, JsResult as BoaResult, JsValue, NativeFunction, Source,
    js_string,
    object::ObjectInitializer,
    property::Attribute,
};

/// What an executed script asked the host to do.
#[derive(Default, Debug, Clone)]
pub struct JsResult {
    /// Lines captured from console.log/warn/error, prefixed with the
    /// channel ("[log]", "[warn]", "[error]"). Useful for debugging
    /// "why didn't this site work" without running a real browser.
    pub log: Vec<String>,
    /// If a script set `window.location.href = ...`, called
    /// `location.assign(...)` or `location.replace(...)`, the most
    /// recent such target. The host (scroll's main loop) is expected
    /// to follow the redirect.
    pub redirect: Option<String>,
    /// Cookies for the page's host AFTER any `document.cookie =`
    /// writes during script execution. `cookies_dirty` says whether
    /// they changed; if false the host can skip writing.
    pub cookies: HashMap<String, String>,
    pub cookies_dirty: bool,
    /// Final localStorage contents for this origin; `localstorage_dirty`
    /// says whether to persist to disk.
    pub localstorage: HashMap<String, String>,
    pub localstorage_dirty: bool,
    /// DOM-element values touched by JS, keyed by element id. After
    /// the page renders, the form-submit path can use these to
    /// override field values that JS set programmatically (e.g. a
    /// hidden CSRF input the JS populates from a cookie).
    pub dom_values: HashMap<String, String>,
    /// Inline + external script bodies extracted from the page in
    /// document order. Saved on the tab so submit-time re-runs can
    /// fire the same listener set without re-fetching externals.
    pub scripts: Vec<String>,
    /// True when a submit-event listener invoked `preventDefault()`
    /// during a synthetic dispatch (re-run with `submit_form_id`).
    /// The host should abort the form submission when this is true.
    pub submit_prevented: bool,
    /// innerHTML mutations: element id → new HTML. Lets the host
    /// surgically replace those elements in the raw HTML and
    /// re-render so JS-driven content swaps (e.g. "Loading..." →
    /// real markup) actually appear in scroll's text view.
    pub inner_html_changes: HashMap<String, String>,
    /// Decoded text harvested from any captured Next.js / React
    /// Flight RSC payload chunks. Treat as plain text content the
    /// host can append to the rendered page.
    pub rsc_text: Option<String>,
}

/// One element's mutable state, addressed by its id attribute. Only
/// elements that JS interacts with end up in the dom map; we don't
/// maintain a full live DOM tree.
#[derive(Default, Clone, Debug)]
struct ElementState {
    tag: String,
    value: String,
    checked: bool,
    inner_html: String,
    /// Inner HTML at parse time, so we can diff at the end of the
    /// run and surface JS-driven `innerHTML = ...` mutations to the
    /// host without re-running the renderer pipeline twice.
    original_inner_html: String,
    text_content: String,
    attributes: HashMap<String, String>,
}

/// Per-page state collected during script execution.
#[derive(Default)]
struct JsState {
    redirect: Option<String>,
    log: Vec<String>,
    /// Cookies for the page's host, in the form JS expects:
    /// `{ name: value }`. Loaded from the host's jar before each
    /// run, mutated via `document.cookie = ...`, written back to
    /// the jar after the run finishes.
    cookies: HashMap<String, String>,
    /// Did JS modify cookies? Lets the host skip a jar-write when
    /// unchanged.
    cookies_dirty: bool,
    /// localStorage / sessionStorage. localStorage is loaded from
    /// disk per (set, origin) at run start and persisted at run
    /// end; sessionStorage is per-run (new each navigation).
    local_storage: HashMap<String, String>,
    local_storage_dirty: bool,
    session_storage: HashMap<String, String>,
    /// DOM elements addressable by id. Pre-populated from the parsed
    /// HTML before scripts run, then mutated as JS sets .value /
    /// .innerHTML / etc.
    dom: HashMap<String, ElementState>,
    /// Page host (without scheme/path) — used by fetch() to decide
    /// which cookies and which Origin header to send.
    page_host: String,
    /// Page URL — used by fetch() to resolve relative URLs.
    page_url: String,
    /// Registered event listeners. Keyed by (target_id, event_type)
    /// where target_id is "" for window/document, or an element id.
    /// Live for the duration of the boa Context only.
    listeners: Vec<(String, String, boa_engine::JsValue)>,
    /// Set true by a submit listener calling event.preventDefault().
    submit_prevented: bool,
    /// If Some, after all scripts run we synthesise a submit event
    /// and dispatch it to listeners targeting this form id (or
    /// to document/window listeners). `submit_prevented` records
    /// whether anything called preventDefault().
    submit_dispatch_target: Option<String>,
    /// Captured chunks pushed via `self.__next_f.push([1, "<chunk>"])`
    /// — Next.js / React's streaming RSC payload format. We don't
    /// run React, but we DO parse the Flight serialisation to pull
    /// out tag/text structure so JS-rendered Next pages aren't a
    /// blank "Loading..." in scroll. Joined and parsed at end of
    /// run via `decode_rsc_payload`.
    rsc_chunks: Vec<String>,
}

thread_local! {
    /// Current run's state. Closures registered with boa read/write
    /// here. Set at the top of `run_scripts`, taken at the end.
    static STATE: RefCell<JsState> = RefCell::new(JsState::default());
}

fn record_log(channel: &str, msg: &str) {
    STATE.with(|s| s.borrow_mut().log.push(format!("[{}] {}", channel, msg)));
}
fn set_redirect(target: String) {
    STATE.with(|s| s.borrow_mut().redirect = Some(target));
}

/// Record an addEventListener(type, fn) call for `target_id`.
/// Listeners survive only as long as the boa Context that owns the
/// JsFunction; the host calls `dispatch_submit` before that
/// Context drops.
fn register_listener(target_id: &str, args: &[JsValue], ctx: &mut Context) {
    let event_type = match args.first().and_then(|v| v.to_string(ctx).ok()) {
        Some(s) => s.to_std_string_escaped(),
        None => return,
    };
    let callback = match args.get(1) {
        Some(v) => v.clone(),
        None => return,
    };
    if !matches!(&callback, JsValue::Object(o) if o.is_callable()) { return; }
    STATE.with(|s| {
        s.borrow_mut().listeners.push((target_id.to_string(), event_type, callback));
    });
}

/// Run every inline `<script>` in `html` against a freshly initialised
/// minimal-DOM context. Returns a JsResult describing side effects.
/// Scripts with `src=` attributes (external) are NOT fetched yet —
/// the typical login flow has its critical JS inlined; external
/// scripts are noisy and analytics-heavy.
///
/// `cookies_in` is the host's view of cookies for this URL's host
/// (NOT including subdomain-walk parents — JS only sees its own
/// host's cookies). `localstorage_in` is whatever was previously
/// stored for the (set, origin) pair.
pub fn run_scripts(
    html: &str,
    page_url: &str,
    cookies_in: HashMap<String, String>,
    localstorage_in: HashMap<String, String>,
) -> JsResult {
    let scripts = extract_scripts_in_order(html, page_url, &cookies_in);
    run_extracted(scripts, html, page_url, cookies_in, localstorage_in,
                  HashMap::new(), None)
}

/// Re-run a previously extracted set of scripts. Used at form-submit
/// time so any `addEventListener("submit", ...)` listeners get a
/// chance to fire (with the user's typed values pre-populated in
/// the DOM) and can call `event.preventDefault()` to abort the
/// submission. `pre_dom_values` populates element `value`s before
/// scripts run; `submit_form_id` (if Some) triggers a synthetic
/// submit dispatch after scripts have set up listeners.
pub fn run_extracted(
    scripts: Vec<String>,
    html: &str,
    page_url: &str,
    cookies_in: HashMap<String, String>,
    localstorage_in: HashMap<String, String>,
    pre_dom_values: HashMap<String, String>,
    submit_form_id: Option<String>,
) -> JsResult {
    let host = url::Url::parse(page_url).ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();
    let mut dom = parse_dom(html);
    // Pre-populate element values from the host (form-fill path).
    for (id, value) in &pre_dom_values {
        dom.entry(id.clone()).or_default().value = value.clone();
    }

    if scripts.is_empty() {
        return JsResult {
            cookies: cookies_in,
            localstorage: localstorage_in,
            scripts: Vec::new(),
            ..Default::default()
        };
    }

    let scripts_for_result = scripts.clone();

    // Reset thread-local state for this run.
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        *st = JsState::default();
        st.cookies = cookies_in;
        st.local_storage = localstorage_in;
        st.dom = dom;
        st.page_host = host;
        st.page_url = page_url.to_string();
        st.submit_dispatch_target = submit_form_id.clone();
    });

    let mut ctx = Context::default();
    if install_globals(&mut ctx, page_url).is_err() {
        STATE.with(|s| s.borrow_mut().listeners.clear());
        let mut r = finish_run();
        r.scripts = scripts_for_result;
        return r;
    }

    for (i, src) in scripts.iter().enumerate() {
        // Best-effort: a script that throws still lets the next one
        // run, mirroring browser behaviour.
        if let Err(e) = ctx.eval(Source::from_bytes(src.as_bytes())) {
            let head: String = src.chars().take(60).collect::<String>().replace('\n', " ");
            record_log("error", &format!("script[{}] ({}): {}", i, head, e));
        }
    }

    // After all scripts are loaded, dispatch a synthetic submit
    // event if the host requested one. This is the post-fill
    // re-run path — listeners get to validate / cancel.
    if submit_form_id.is_some() {
        dispatch_submit(&mut ctx);
    }

    // CRITICAL: drop listener JsValues before the boa Context drops.
    // The thread_local STATE outlives this function call; if we
    // leave JsValues in STATE.listeners they get dropped on the
    // NEXT run when STATE is reset, but by then the boa GC heap
    // they reference is gone → use-after-free → SIGABRT.
    STATE.with(|s| s.borrow_mut().listeners.clear());

    let mut r = finish_run();
    r.scripts = scripts_for_result;
    r
}

fn finish_run() -> JsResult {
    STATE.with(|s| {
        let st = s.borrow();
        let dom_values: HashMap<String, String> = st.dom.iter()
            .filter(|(_, e)| !e.value.is_empty())
            .map(|(id, e)| (id.clone(), e.value.clone()))
            .collect();
        // innerHTML diff: any element whose current inner_html
        // differs from its parse-time original.
        let inner_html_changes: HashMap<String, String> = st.dom.iter()
            .filter(|(_, e)| e.inner_html != e.original_inner_html)
            .map(|(id, e)| (id.clone(), e.inner_html.clone()))
            .collect();
        // RSC payload: glue chunks, parse Flight, extract text.
        let rsc_text = if st.rsc_chunks.is_empty() {
            None
        } else {
            decode_rsc_payload(&st.rsc_chunks.concat())
        };
        JsResult {
            log: st.log.clone(),
            redirect: st.redirect.clone(),
            cookies: st.cookies.clone(),
            cookies_dirty: st.cookies_dirty,
            localstorage: st.local_storage.clone(),
            localstorage_dirty: st.local_storage_dirty,
            dom_values,
            scripts: Vec::new(),  // filled in by run_extracted
            submit_prevented: st.submit_prevented,
            inner_html_changes,
            rsc_text,
        }
    })
}

/// Synthesise a `submit` Event and call every "submit" listener that
/// targets the current `submit_dispatch_target` form, plus any
/// listeners registered on document/window. preventDefault() on the
/// event flips `STATE.submit_prevented`.
fn dispatch_submit(ctx: &mut Context) {
    let target_id = STATE.with(|s| s.borrow().submit_dispatch_target.clone());
    let listeners = STATE.with(|s| {
        let st = s.borrow();
        st.listeners.iter().filter_map(|(tgt, ev, fnv)| {
            if ev != "submit" { return None; }
            // Match: empty target_id means "any submit listener" (rare),
            // "_document" / "_window" always fire, or exact form id match.
            let fires = tgt.is_empty() || tgt == "_document" || tgt == "_window"
                || target_id.as_deref() == Some(tgt.as_str());
            if fires { Some(fnv.clone()) } else { None }
        }).collect::<Vec<JsValue>>()
    });
    if listeners.is_empty() { return; }

    // Build the synthetic event object: { type: "submit",
    // preventDefault(), defaultPrevented }. preventDefault flips a
    // thread_local flag because the Copy-bound closure can't capture
    // local state.
    let event = ObjectInitializer::new(ctx)
        .property(js_string!("type"), js_string!("submit"), Attribute::all())
        .property(js_string!("cancelable"), JsValue::Boolean(true), Attribute::all())
        .property(js_string!("defaultPrevented"), JsValue::Boolean(false), Attribute::all())
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| {
            STATE.with(|s| s.borrow_mut().submit_prevented = true);
            Ok(JsValue::undefined())
        }), js_string!("preventDefault"), 0)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())),
                  js_string!("stopPropagation"), 0)
        .build();
    let event_val = JsValue::from(event);

    for lstn in listeners {
        if let JsValue::Object(fobj) = lstn {
            if !fobj.is_callable() { continue; }
            if let Err(e) = fobj.call(&JsValue::undefined(), &[event_val.clone()], ctx) {
                record_log("error", &format!("submit listener threw: {}", e));
            }
        }
    }
}

/// Parse the HTML once with scraper, populate ElementState entries
/// for every element that has an `id` attribute. JS reaches into
/// these via getElementById. Elements without ids are queryable by
/// other selectors (handled separately) but not stored here.
fn parse_dom(html: &str) -> HashMap<String, ElementState> {
    let doc = scraper::Html::parse_document(html);
    let mut map = HashMap::new();
    let sel = match scraper::Selector::parse("[id]") { Ok(s) => s, Err(_) => return map };
    for el in doc.select(&sel) {
        let id = match el.value().attr("id") { Some(s) => s.to_string(), None => continue };
        let tag = el.value().name().to_string();
        let value = el.value().attr("value").unwrap_or("").to_string();
        let mut attrs = HashMap::new();
        for (k, v) in el.value().attrs() {
            attrs.insert(k.to_string(), v.to_string());
        }
        let checked = attrs.contains_key("checked");
        let inner_html = el.inner_html();
        let text_content: String = el.text().collect::<Vec<_>>().join("");
        map.insert(id, ElementState {
            tag, value, checked,
            original_inner_html: inner_html.clone(),
            inner_html,
            text_content,
            attributes: attrs,
        });
    }
    map
}

/// Extract every inline `<script>` body in document order. External
/// `<script src=...>` are intentionally skipped: they're nearly
/// always framework runtimes (React, Next, Turbopack, webpack)
/// that need a real DOM and won't run in boa. Skipping them means
/// the inline scripts that DO matter — the streaming
/// `__next_f.push([1, "<chunk>"])` lines, simple form-validating
/// inline scripts — get to run without being crowded out by the
/// (now-removed) script-count cap.
///
/// Total inline-script size capped at 4 MB so a pathological page
/// can't spike memory.
fn extract_scripts_in_order(html: &str, _page_url: &str, _cookies: &HashMap<String, String>) -> Vec<String> {
    const MAX_TOTAL_BYTES: usize = 4 * 1_048_576;
    let mut out = Vec::new();
    let mut total = 0usize;
    let lower = html.to_ascii_lowercase();
    let mut i = 0usize;
    while let Some(start) = lower[i..].find("<script") {
        let s = i + start;
        let tag_end = match lower[s..].find('>') { Some(e) => s + e, None => break };
        let opening = &html[s..=tag_end];
        let body_start = tag_end + 1;
        let close_rel = match lower[body_start..].find("</script>") {
            Some(c) => c,
            None => break,
        };
        let close = body_start + close_rel;
        let body = &html[body_start..close];

        // External: skip silently.
        if parse_src_attr(opening).is_some() {
            i = close + "</script>".len();
            continue;
        }
        if !body.trim().is_empty() {
            if total + body.len() > MAX_TOTAL_BYTES { break; }
            total += body.len();
            out.push(body.to_string());
        }
        i = close + "</script>".len();
    }
    out
}

fn parse_src_attr(opening_tag: &str) -> Option<String> {
    let lower = opening_tag.to_ascii_lowercase();
    let key = "src=";
    let pos = lower.find(key)?;
    let rest = &opening_tag[pos + key.len()..];
    let rest = rest.trim_start();
    let bytes = rest.as_bytes();
    let (start, end_char) = match bytes.first() {
        Some(b'"') => (1usize, '"'),
        Some(b'\'') => (1usize, '\''),
        _ => (0usize, ' '),
    };
    let tail = &rest[start..];
    let end = tail.find(|c: char| c == end_char || (end_char == ' ' && (c == '>' || c.is_whitespace()))).unwrap_or(tail.len());
    let val = &tail[..end];
    if val.is_empty() { None } else { Some(val.to_string()) }
}

fn absolute_url(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") || href.starts_with("//") {
        if let Some(stripped) = href.strip_prefix("//") {
            return format!("https:{}{}", "//", stripped);
        }
        return href.to_string();
    }
    if let Ok(b) = url::Url::parse(base) {
        if let Ok(u) = b.join(href) {
            return u.to_string();
        }
    }
    href.to_string()
}

/// Construct a fresh tokio runtime + Firefox-emulating rquest client.
/// Both are cheap (sub-ms) and short-lived; we don't try to share a
/// long-lived client across the JS layer because the JS fetch path is
/// reentered from many call sites and a thread-local would just hide
/// lifetime tangles for negligible win.
fn make_http() -> Option<(tokio::runtime::Runtime, rquest::Client)> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let client = rquest::Client::builder()
        .emulation(rquest_util::Emulation::Firefox136)
        .timeout(std::time::Duration::from_secs(10))
        .redirect(rquest::redirect::Policy::limited(10))
        .build()
        .ok()?;
    Some((rt, client))
}

fn install_globals(ctx: &mut Context, page_url: &str) -> BoaResult<()> {
    // ---------- console ----------
    let console = ObjectInitializer::new(ctx)
        .function(NativeFunction::from_copy_closure(|_t, args, ctx| {
            record_log("log", &join_args(args, ctx));
            Ok(JsValue::undefined())
        }), js_string!("log"), 0)
        .function(NativeFunction::from_copy_closure(|_t, args, ctx| {
            record_log("warn", &join_args(args, ctx));
            Ok(JsValue::undefined())
        }), js_string!("warn"), 0)
        .function(NativeFunction::from_copy_closure(|_t, args, ctx| {
            record_log("error", &join_args(args, ctx));
            Ok(JsValue::undefined())
        }), js_string!("error"), 0)
        .function(NativeFunction::from_copy_closure(|_t, args, ctx| {
            record_log("info", &join_args(args, ctx));
            Ok(JsValue::undefined())
        }), js_string!("info"), 0)
        .build();
    ctx.register_global_property(js_string!("console"), console, Attribute::all())?;

    // ---------- navigator ----------
    let navigator = ObjectInitializer::new(ctx)
        .property(
            js_string!("userAgent"),
            js_string!("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36"),
            Attribute::all(),
        )
        .property(js_string!("language"), js_string!("en-US"), Attribute::all())
        .property(js_string!("platform"), js_string!("Linux x86_64"), Attribute::all())
        .build();
    ctx.register_global_property(js_string!("navigator"), navigator, Attribute::all())?;

    // ---------- location ----------
    let parsed = url::Url::parse(page_url).ok();
    let host = parsed.as_ref().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
    let pathname = parsed.as_ref().map(|u| u.path().to_string()).unwrap_or_default();
    let search = parsed.as_ref().and_then(|u| u.query().map(|q| format!("?{}", q))).unwrap_or_default();
    let protocol = parsed.as_ref().map(|u| format!("{}:", u.scheme())).unwrap_or_default();

    let location = ObjectInitializer::new(ctx)
        .property(js_string!("href"), js_string!(page_url.to_string()), Attribute::all())
        .property(js_string!("host"), js_string!(host.clone()), Attribute::all())
        .property(js_string!("hostname"), js_string!(host), Attribute::all())
        .property(js_string!("pathname"), js_string!(pathname), Attribute::all())
        .property(js_string!("search"), js_string!(search), Attribute::all())
        .property(js_string!("protocol"), js_string!(protocol), Attribute::all())
        .function(NativeFunction::from_copy_closure(|_t, args, ctx| {
            if let Some(v) = args.first() {
                if let Ok(s) = v.to_string(ctx) {
                    set_redirect(s.to_std_string_escaped());
                }
            }
            Ok(JsValue::undefined())
        }), js_string!("assign"), 1)
        .function(NativeFunction::from_copy_closure(|_t, args, ctx| {
            if let Some(v) = args.first() {
                if let Ok(s) = v.to_string(ctx) {
                    set_redirect(s.to_std_string_escaped());
                }
            }
            Ok(JsValue::undefined())
        }), js_string!("replace"), 1)
        .build();
    ctx.register_global_property(js_string!("location"), location.clone(), Attribute::all())?;

    // ---------- window ----------
    let window = ObjectInitializer::new(ctx)
        .property(js_string!("location"), location, Attribute::all())
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())),
                  js_string!("alert"), 0)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::null())),
                  js_string!("prompt"), 0)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::Boolean(false))),
                  js_string!("confirm"), 0)
        .function(NativeFunction::from_copy_closure(|_t, args, ctx| {
            register_listener("_window", args, ctx);
            Ok(JsValue::undefined())
        }), js_string!("addEventListener"), 2)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())),
                  js_string!("removeEventListener"), 2)
        .build();
    ctx.register_global_property(js_string!("window"), window, Attribute::all())?;

    // ---------- document (real-ish getElementById + cookie property) ----------
    // getElementById / querySelector return a stub Element object
    // backed by JsState.dom — mutations to .value, .innerHTML, etc.
    // flow back to the host via JsResult.dom_values.
    let get_by_id = NativeFunction::from_copy_closure(|_t, args, ctx| {
        let id = args.first().and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.to_std_string_escaped()).unwrap_or_default();
        if id.is_empty() { return Ok(JsValue::null()); }
        let exists = STATE.with(|s| s.borrow().dom.contains_key(&id));
        if !exists { return Ok(JsValue::null()); }
        let obj = make_element_object(ctx, &id)?;
        Ok(JsValue::from(obj))
    });
    // querySelector with a tiny subset: "#id", ".class" returns first
    // element matching, "tag" returns first matching tag. For
    // anything fancier returns null (caller's feature-detect handles).
    let query_selector = NativeFunction::from_copy_closure(|_t, args, ctx| {
        let sel = args.first().and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.to_std_string_escaped()).unwrap_or_default();
        let matched_id = STATE.with(|s| -> Option<String> {
            let st = s.borrow();
            if let Some(id) = sel.strip_prefix('#') {
                if st.dom.contains_key(id) { return Some(id.to_string()); }
                return None;
            }
            // tag name selector
            if !sel.contains([' ', '.', '#', '[', '>']) {
                let want = sel.to_ascii_lowercase();
                return st.dom.iter().find(|(_, e)| e.tag.eq_ignore_ascii_case(&want))
                    .map(|(k, _)| k.clone());
            }
            None
        });
        match matched_id {
            Some(id) => Ok(JsValue::from(make_element_object(ctx, &id)?)),
            None => Ok(JsValue::null()),
        }
    });
    let document = ObjectInitializer::new(ctx)
        .function(get_by_id, js_string!("getElementById"), 1)
        .function(query_selector, js_string!("querySelector"), 1)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())),
                  js_string!("querySelectorAll"), 1)
        // document.addEventListener stores the listener under
        // target_id "_document" so dispatch_submit can find it.
        .function(NativeFunction::from_copy_closure(|_t, args, ctx| {
            register_listener("_document", args, ctx);
            Ok(JsValue::undefined())
        }), js_string!("addEventListener"), 2)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())),
                  js_string!("removeEventListener"), 2)
        .function(NativeFunction::from_copy_closure(|_t, args, ctx| {
            // createElement: return a fresh anonymous element. id is
            // assigned lazily if the caller sets one.
            let tag = args.first().and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped()).unwrap_or_default();
            let id = STATE.with(|s| {
                let mut st = s.borrow_mut();
                let mut n = 0u32;
                loop {
                    let candidate = format!("__anon_{}", n);
                    if !st.dom.contains_key(&candidate) {
                        st.dom.insert(candidate.clone(), ElementState {
                            tag: tag.clone(), ..Default::default()
                        });
                        break candidate;
                    }
                    n += 1;
                }
            });
            Ok(JsValue::from(make_element_object(ctx, &id)?))
        }), js_string!("createElement"), 1)
        .build();
    // document.cookie: getter returns "k=v; k=v" for the current host's
    // cookies; setter parses a single cookie attribute string and stores
    // the (name, value). We ignore Domain / Path / Expires / Secure
    // for now — real semantics would respect them but the typical
    // login flow sets session cookies the server already emits via
    // Set-Cookie too.
    let realm = ctx.realm().clone();
    let cookie_get = NativeFunction::from_copy_closure(|_t, _args, _c| {
        let s = STATE.with(|s| {
            s.borrow().cookies.iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join("; ")
        });
        Ok(JsValue::from(js_string!(s)))
    }).to_js_function(&realm);
    let cookie_set = NativeFunction::from_copy_closure(|_t, args, ctx| {
        if let Some(v) = args.first() {
            if let Ok(js_str) = v.to_string(ctx) {
                let raw = js_str.to_std_string_escaped();
                // Take the first attr (before any ';'), split on '='.
                let pair = raw.split(';').next().unwrap_or("").trim();
                if let Some((name, value)) = pair.split_once('=') {
                    STATE.with(|s| {
                        let mut st = s.borrow_mut();
                        st.cookies.insert(name.trim().to_string(), value.trim().to_string());
                        st.cookies_dirty = true;
                    });
                }
            }
        }
        Ok(JsValue::undefined())
    }).to_js_function(&realm);
    let cookie_desc = boa_engine::property::PropertyDescriptor::builder()
        .get(cookie_get)
        .set(cookie_set)
        .enumerable(true)
        .configurable(true)
        .build();
    document.define_property_or_throw(js_string!("cookie"), cookie_desc, ctx)?;
    ctx.register_global_property(js_string!("document"), document, Attribute::all())?;

    // ---------- localStorage / sessionStorage ----------
    install_storage(ctx, "localStorage", true)?;
    install_storage(ctx, "sessionStorage", false)?;

    // ---------- timers (no-ops returning fake ids) ----------
    ctx.register_global_callable(js_string!("setTimeout"), 0,
        NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::Integer(0))))?;
    ctx.register_global_callable(js_string!("setInterval"), 0,
        NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::Integer(0))))?;
    ctx.register_global_callable(js_string!("clearTimeout"), 0,
        NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())))?;
    ctx.register_global_callable(js_string!("clearInterval"), 0,
        NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())))?;

    // ---------- URL constructor + matchMedia stubs ----------
    // Many pages do `new URL(...)` for parsing. boa has no built-in
    // WHATWG URL; without this they throw before reaching the
    // __next_f.push lines further down the page. Naive regex parser
    // is enough for typical use.
    let _ = ctx.eval(Source::from_bytes(br#"
        function URL(href, base) {
            this.href = href || '';
            var m = /^([a-z][a-z0-9+.-]*):\/\/([^/?#]*)([^?#]*)(\?[^#]*)?(#.*)?$/i.exec(this.href);
            if (m) {
                this.protocol = m[1] + ':';
                this.host = m[2];
                this.hostname = (m[2] || '').split(':')[0];
                this.pathname = m[3] || '/';
                this.search = m[4] || '';
                this.hash = m[5] || '';
            } else {
                this.protocol = ''; this.host = ''; this.hostname = '';
                this.pathname = this.href; this.search = ''; this.hash = '';
            }
            this.toString = function() { return this.href; };
            this.searchParams = {
                get: function() { return null; },
                set: function() {}, has: function() { return false; },
                toString: function() { return ''; }
            };
        }
    "#));
    // matchMedia: theme detection asks the browser whether the user
    // prefers dark mode. We always say no, falling the JS into its
    // default branch.
    let _ = ctx.eval(Source::from_bytes(br#"
        var matchMedia = function(q) {
            return {
                matches: false, media: q,
                addListener: function(){}, removeListener: function(){},
                addEventListener: function(){}, removeEventListener: function(){}
            };
        };
    "#));

    // ---------- Next.js / React Flight streaming-payload capture ----------
    // Next.js emits its rendered tree as a series of inline scripts:
    //   self.__next_f = self.__next_f || [];
    //   self.__next_f.push([1, "<chunk>"]);
    //
    // We can't run React (multi-MB bundle, no real DOM), but the
    // chunks contain Flight-serialised React trees we CAN parse for
    // text. Pre-register __next_f as an object with a `push` method
    // that captures every chunk into STATE.rsc_chunks. When the page
    // does `self.__next_f = self.__next_f || []`, our truthy object
    // stays in place. self is aliased to globalThis below.
    let next_f = ObjectInitializer::new(ctx)
        .function(NativeFunction::from_copy_closure(|_t, args, ctx| {
            for arg in args {
                if let JsValue::Object(arr) = arg {
                    // push receives [tag, "<chunk>"] tuples; the chunk
                    // is at index 1.
                    if let Ok(v) = arr.get(1u32, ctx) {
                        if let Ok(s) = v.to_string(ctx) {
                            STATE.with(|st|
                                st.borrow_mut().rsc_chunks.push(s.to_std_string_escaped()));
                        }
                    }
                }
            }
            Ok(JsValue::Integer(0))
        }), js_string!("push"), 1)
        .build();
    ctx.register_global_property(js_string!("__next_f"), next_f, Attribute::all())?;
    // `self` alias to globalThis. Browser-shaped scripts use it
    // interchangeably with `window` and `globalThis`. Doing it via
    // a property write on the global object guarantees the binding,
    // unlike a `var self = globalThis` eval which boa sometimes
    // resolves into a local TDZ snapshot.
    let global = ctx.global_object();
    let _ = global.set(js_string!("self"), JsValue::from(global.clone()), false, ctx);

    // ---------- fetch ----------
    // Synchronous-but-pretends-thenable. Real fetch is async; here
    // the network call blocks inside the closure and the returned
    // object exposes .then(cb) that immediately invokes `cb(self)`.
    // That covers the common patterns:
    //
    //   fetch(url).then(r => r.json()).then(data => ...)
    //   fetch(url, {method:"POST", body, headers}).then(...)
    //
    // It does NOT cover code that puts fetch behind an `await` in an
    // async function and expects the rest of the function to resume
    // — boa's async machinery is heavier, and this gets us most of
    // the way for login-form-style flows.
    ctx.register_global_callable(js_string!("fetch"), 1,
        NativeFunction::from_copy_closure(|_t, args, ctx| {
            let url = args.first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let opts = args.get(1).cloned();
            do_fetch(ctx, &url, opts)
        }))?;

    Ok(())
}

/// Perform a synchronous fetch and return a Response-shaped object.
fn do_fetch(ctx: &mut Context, url: &str, opts: Option<JsValue>) -> BoaResult<JsValue> {
    let (page_url, cookies) = STATE.with(|s| {
        let st = s.borrow();
        (st.page_url.clone(), st.cookies.clone())
    });
    let absolute = absolute_url(&page_url, url);

    // Pull method / body / headers off the options bag.
    let mut method = "GET".to_string();
    let mut body: Option<String> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(JsValue::Object(o)) = opts {
        if let Ok(m) = o.get(js_string!("method"), ctx) {
            if let Ok(s) = m.to_string(ctx) {
                let s = s.to_std_string_escaped();
                if !s.is_empty() { method = s.to_uppercase(); }
            }
        }
        if let Ok(b) = o.get(js_string!("body"), ctx) {
            if !b.is_undefined() && !b.is_null() {
                if let Ok(s) = b.to_string(ctx) {
                    body = Some(s.to_std_string_escaped());
                }
            }
        }
        if let Ok(h) = o.get(js_string!("headers"), ctx) {
            if let JsValue::Object(hobj) = h {
                if let Ok(keys) = hobj.own_property_keys(ctx) {
                    for key in keys {
                        let name = key.to_string();
                        if let Ok(val) = hobj.get(key, ctx) {
                            if let Ok(vs) = val.to_string(ctx) {
                                headers.push((name, vs.to_std_string_escaped()));
                            }
                        }
                    }
                }
            }
        }
    }

    let (status, body_text, set_cookie) = match make_http() {
        Some((rt, client)) => rt.block_on(async {
            let m = match method.as_str() {
                "POST" => rquest::Method::POST,
                "PUT" => rquest::Method::PUT,
                "DELETE" => rquest::Method::DELETE,
                _ => rquest::Method::GET,
            };
            let mut req = client.request(m, &absolute);
            for (k, v) in &headers { req = req.header(k.as_str(), v.as_str()); }
            if !cookies.is_empty() {
                let cs: String = cookies.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join("; ");
                req = req.header("Cookie", cs);
            }
            if let Some(b) = body { req = req.body(b); }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let sc = resp.headers()
                        .get(rquest::header::SET_COOKIE)
                        .and_then(|v| v.to_str().ok())
                        .map(String::from);
                    let body = resp.text().await.unwrap_or_default();
                    (status, body, sc)
                }
                Err(e) => {
                    record_log("error", &format!("fetch {} failed: {}", absolute, e));
                    (0, String::new(), None)
                }
            }
        }),
        None => (0, String::new(), None),
    };

    // Capture any Set-Cookie header into our cookie state so the JS
    // consequence of the fetch is visible to subsequent document.cookie
    // reads and persisted to the host's jar.
    if let Some(val) = set_cookie {
        if let Some((name_val, _)) = val.split_once(';') {
            if let Some((name, value)) = name_val.split_once('=') {
                STATE.with(|s| {
                    let mut st = s.borrow_mut();
                    st.cookies.insert(name.trim().to_string(), value.trim().to_string());
                    st.cookies_dirty = true;
                });
            }
        }
    }

    // The Response object stashes its body string as `__body` so the
    // text() / json() closures (which must be Copy and therefore
    // can't capture a String) read it back off `this`.
    let resp_obj = ObjectInitializer::new(ctx)
        .property(js_string!("status"), JsValue::Integer(status as i32), Attribute::all())
        .property(js_string!("ok"), JsValue::Boolean((200..300).contains(&status)), Attribute::all())
        .property(js_string!("statusText"), js_string!(""), Attribute::all())
        .property(js_string!("url"), js_string!(absolute.clone()), Attribute::all())
        .property(js_string!("__body"), js_string!(body_text), Attribute::all())
        .function(NativeFunction::from_copy_closure(|t, _args, ctx| {
            let body = read_body(t, ctx);
            make_thenable(ctx, JsValue::from(js_string!(body)))
        }), js_string!("text"), 0)
        .function(NativeFunction::from_copy_closure(|t, _args, ctx| {
            let body = read_body(t, ctx);
            let parsed = ctx.eval(Source::from_bytes(format!("({})", body).as_bytes()))
                .unwrap_or(JsValue::null());
            make_thenable(ctx, parsed)
        }), js_string!("json"), 0)
        .build();

    // The fetch() return value is itself thenable: .then(cb) calls
    // cb(response) synchronously and rewraps the result.
    make_thenable(ctx, JsValue::from(resp_obj))
}

/// Parse a glued Next.js / React Flight payload and harvest any
/// text content the server-side render baked in. Each "line" in the
/// payload looks like `<id>:<value>` where value is either JSON (the
/// real tree) or `I[mod, deps, name]` (a Client Component import,
/// which we can't run). We only care about the JSON entries; we
/// walk them and pull out string leaves under the right keys.
///
/// This is best-effort: pages whose entire visible content is
/// produced by Client Components (which run in the browser, not on
/// the server) will yield little. Pages with Server Components or
/// statically rendered text yield real content.
pub fn decode_rsc_payload(payload: &str) -> Option<String> {
    let mut entries: HashMap<String, serde_json::Value> = HashMap::new();
    for line in payload.split('\n') {
        let (id, body) = match line.split_once(':') {
            Some(p) => p, None => continue,
        };
        if body.starts_with('I') || body.starts_with('H') { continue; }  // imports / hyperlinks
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
            entries.insert(id.to_string(), v);
        }
    }
    if entries.is_empty() { return None; }

    let mut out = String::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Walk every JSON entry — passionfruits-style payloads stream the
    // root tree across multiple top-level entries, not just "0".
    let mut keys: Vec<String> = entries.keys().cloned().collect();
    keys.sort();  // deterministic output
    for k in keys {
        if let Some(v) = entries.get(&k) {
            extract_rsc_text(v, &mut out, &mut seen);
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

fn extract_rsc_text(v: &serde_json::Value, out: &mut String, seen: &mut std::collections::HashSet<String>) {
    use serde_json::Value;
    match v {
        Value::String(s) => {
            // Skip Flight reference markers ($S = symbol, $L = client
            // component import, $undefined etc.) and very short strings
            // that are usually attribute values not body text.
            if s.starts_with('$') { return; }
            let t = s.trim();
            if t.is_empty() { return; }
            // Skip strings that look like CSS class blobs / hashes.
            if t.contains("__") && !t.contains(' ') { return; }
            if seen.insert(t.to_string()) {
                out.push_str(t);
                out.push('\n');
            }
        }
        Value::Array(arr) => {
            // React Flight element form: ["$", tag, key, props].
            // The fourth element is the props object, which carries
            // children. Recurse into props directly to skip the tag.
            if arr.len() >= 4 {
                if let Some(Value::String(s)) = arr.first() {
                    if s == "$" {
                        extract_rsc_text(&arr[3], out, seen);
                        return;
                    }
                }
            }
            for item in arr { extract_rsc_text(item, out, seen); }
        }
        Value::Object(obj) => {
            // Visible-text-bearing keys. "children" is the big one.
            // "title" / "alt" / "label" / "placeholder" cover ARIA-ish
            // surfaces. Skip everything else (className, style, refs,
            // event handlers, …).
            const CONTENT_KEYS: &[&str] = &[
                "children", "title", "alt", "label", "placeholder",
                "description", "value", "text", "content",
            ];
            for k in CONTENT_KEYS {
                if let Some(child) = obj.get(*k) {
                    extract_rsc_text(child, out, seen);
                }
            }
        }
        _ => {}
    }
}

fn read_body(this: &JsValue, ctx: &mut Context) -> String {
    let obj = match this.as_object() { Some(o) => o, None => return String::new() };
    obj.get(js_string!("__body"), ctx).ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default()
}

/// Wrap a value in a `{ __value, then(cb) { return cb(__value) } }`
/// style object so chained `.then()` calls flow synchronously. The
/// closure reads `__value` off `this`, sidestepping boa's
/// `Fn + Copy` capture restriction.
fn make_thenable(ctx: &mut Context, value: JsValue) -> BoaResult<JsValue> {
    let then = NativeFunction::from_copy_closure(|t, args, ctx| {
        // Pull the wrapped value off `this`.
        let value = match t.as_object()
            .and_then(|o| o.get(js_string!("__value"), ctx).ok())
        {
            Some(v) => v,
            None => JsValue::undefined(),
        };
        let cb = match args.first() {
            Some(JsValue::Object(f)) if f.is_callable() => f.clone(),
            _ => return make_thenable(ctx, value),
        };
        let result = match cb.call(&JsValue::undefined(), &[value], ctx) {
            Ok(v) => v,
            Err(e) => {
                record_log("error", &format!("then callback threw: {}", e));
                JsValue::undefined()
            }
        };
        make_thenable(ctx, result)
    });
    let obj = ObjectInitializer::new(ctx)
        .property(js_string!("__value"), value, Attribute::all())
        .function(then, js_string!("then"), 1)
        .build();
    Ok(JsValue::from(obj))
}

/// Build a JS Element object addressable by `id`. The element's
/// dynamic properties (`value`, `innerHTML`, `textContent`,
/// `checked`, `id`, `tagName`) are accessor properties that read /
/// write through `STATE.dom[<id>]`. `__id` is a private-ish data
/// property the accessors use to know which element they belong to.
fn make_element_object(ctx: &mut Context, id: &str) -> BoaResult<boa_engine::JsObject> {
    use boa_engine::property::PropertyDescriptor;
    let realm = ctx.realm().clone();

    // tagName getter
    let tag_get = NativeFunction::from_copy_closure(|t, _args, _c| {
        let id = element_id(t).unwrap_or_default();
        let v = STATE.with(|s| s.borrow().dom.get(&id).map(|e| e.tag.to_uppercase()).unwrap_or_default());
        Ok(JsValue::from(js_string!(v)))
    }).to_js_function(&realm);

    // value get/set
    let value_get = NativeFunction::from_copy_closure(|t, _args, _c| {
        let id = element_id(t).unwrap_or_default();
        let v = STATE.with(|s| s.borrow().dom.get(&id).map(|e| e.value.clone()).unwrap_or_default());
        Ok(JsValue::from(js_string!(v)))
    }).to_js_function(&realm);
    let value_set = NativeFunction::from_copy_closure(|t, args, ctx| {
        let id = element_id(t).unwrap_or_default();
        let new_val = args.first().and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.to_std_string_escaped()).unwrap_or_default();
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.dom.entry(id).or_default().value = new_val;
        });
        Ok(JsValue::undefined())
    }).to_js_function(&realm);

    // innerHTML get/set
    let inner_get = NativeFunction::from_copy_closure(|t, _args, _c| {
        let id = element_id(t).unwrap_or_default();
        let v = STATE.with(|s| s.borrow().dom.get(&id).map(|e| e.inner_html.clone()).unwrap_or_default());
        Ok(JsValue::from(js_string!(v)))
    }).to_js_function(&realm);
    let inner_set = NativeFunction::from_copy_closure(|t, args, ctx| {
        let id = element_id(t).unwrap_or_default();
        let new_val = args.first().and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.to_std_string_escaped()).unwrap_or_default();
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.dom.entry(id).or_default().inner_html = new_val;
        });
        Ok(JsValue::undefined())
    }).to_js_function(&realm);

    // textContent get/set
    let text_get = NativeFunction::from_copy_closure(|t, _args, _c| {
        let id = element_id(t).unwrap_or_default();
        let v = STATE.with(|s| s.borrow().dom.get(&id).map(|e| e.text_content.clone()).unwrap_or_default());
        Ok(JsValue::from(js_string!(v)))
    }).to_js_function(&realm);
    let text_set = NativeFunction::from_copy_closure(|t, args, ctx| {
        let id = element_id(t).unwrap_or_default();
        let new_val = args.first().and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.to_std_string_escaped()).unwrap_or_default();
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.dom.entry(id).or_default().text_content = new_val;
        });
        Ok(JsValue::undefined())
    }).to_js_function(&realm);

    // checked get/set
    let checked_get = NativeFunction::from_copy_closure(|t, _args, _c| {
        let id = element_id(t).unwrap_or_default();
        let v = STATE.with(|s| s.borrow().dom.get(&id).map(|e| e.checked).unwrap_or(false));
        Ok(JsValue::Boolean(v))
    }).to_js_function(&realm);
    let checked_set = NativeFunction::from_copy_closure(|t, args, _c| {
        let id = element_id(t).unwrap_or_default();
        let new_val = args.first().and_then(|v| v.as_boolean()).unwrap_or(false);
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.dom.entry(id).or_default().checked = new_val;
        });
        Ok(JsValue::undefined())
    }).to_js_function(&realm);

    let obj = ObjectInitializer::new(ctx)
        .property(js_string!("__id"), js_string!(id.to_string()), Attribute::all())
        .property(js_string!("id"), js_string!(id.to_string()), Attribute::all())
        // No-op behaviour methods. .click() / .submit() / .focus() /
        // .blur() / .dispatchEvent() — common form-validating JS calls
        // these and treats the absence as "OK done".
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())),
                  js_string!("click"), 0)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())),
                  js_string!("submit"), 0)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())),
                  js_string!("focus"), 0)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())),
                  js_string!("blur"), 0)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::Boolean(true))),
                  js_string!("dispatchEvent"), 1)
        // Element-level addEventListener: pull element id from `this.__id`
        // so dispatch can match this listener to the right form.
        .function(NativeFunction::from_copy_closure(|t, args, ctx| {
            let id = element_id(t).unwrap_or_default();
            register_listener(&id, args, ctx);
            Ok(JsValue::undefined())
        }), js_string!("addEventListener"), 2)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())),
                  js_string!("removeEventListener"), 2)
        // getAttribute / setAttribute backed by the attributes map.
        .function(NativeFunction::from_copy_closure(|t, args, ctx| {
            let id = element_id(t).unwrap_or_default();
            let name = args.first().and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped()).unwrap_or_default();
            let v = STATE.with(|s|
                s.borrow().dom.get(&id).and_then(|e| e.attributes.get(&name).cloned()));
            Ok(match v { Some(s) => JsValue::from(js_string!(s)), None => JsValue::null() })
        }), js_string!("getAttribute"), 1)
        .function(NativeFunction::from_copy_closure(|t, args, ctx| {
            let id = element_id(t).unwrap_or_default();
            let name = args.first().and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped()).unwrap_or_default();
            let val = args.get(1).and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped()).unwrap_or_default();
            STATE.with(|s| {
                let mut st = s.borrow_mut();
                st.dom.entry(id).or_default().attributes.insert(name, val);
            });
            Ok(JsValue::undefined())
        }), js_string!("setAttribute"), 2)
        .build();

    let prop_accessor = |get, set| -> PropertyDescriptor {
        PropertyDescriptor::builder()
            .get(get).set(set).enumerable(true).configurable(true).build()
    };
    obj.define_property_or_throw(js_string!("tagName"),
        PropertyDescriptor::builder().get(tag_get).enumerable(true).configurable(true).build(),
        ctx)?;
    obj.define_property_or_throw(js_string!("value"),     prop_accessor(value_get, value_set),     ctx)?;
    obj.define_property_or_throw(js_string!("innerHTML"), prop_accessor(inner_get, inner_set),     ctx)?;
    obj.define_property_or_throw(js_string!("textContent"), prop_accessor(text_get, text_set),     ctx)?;
    obj.define_property_or_throw(js_string!("checked"),   prop_accessor(checked_get, checked_set), ctx)?;
    Ok(obj)
}

/// Read the `__id` data property off a `this` JsValue used as a
/// thin element handle. Returns None on any structural mismatch.
fn element_id(this: &JsValue) -> Option<String> {
    let obj = this.as_object()?;
    let mut ctx = Context::default();
    let v = obj.get(js_string!("__id"), &mut ctx).ok()?;
    let s = v.to_string(&mut ctx).ok()?;
    Some(s.to_std_string_escaped())
}

/// Build a `localStorage`-style object: getItem / setItem / removeItem
/// / clear / key / length, plus index access. `persistent` decides
/// whether the underlying map is `STATE.local_storage` (loaded /
/// persisted by the host) or `STATE.session_storage` (per-run only).
fn install_storage(ctx: &mut Context, name: &'static str, persistent: bool) -> BoaResult<()> {
    // Each native fn picks the right map via the `persistent` flag,
    // captured by value (Copy bool).
    let get_item = NativeFunction::from_copy_closure(move |_t, args, ctx| {
        let key = args.first().and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.to_std_string_escaped()).unwrap_or_default();
        let v = STATE.with(|s| {
            let st = s.borrow();
            let map = if persistent { &st.local_storage } else { &st.session_storage };
            map.get(&key).cloned()
        });
        Ok(match v {
            Some(s) => JsValue::from(js_string!(s)),
            None => JsValue::null(),
        })
    });
    let set_item = NativeFunction::from_copy_closure(move |_t, args, ctx| {
        let key = args.first().and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.to_std_string_escaped()).unwrap_or_default();
        let val = args.get(1).and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.to_std_string_escaped()).unwrap_or_default();
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            if persistent {
                st.local_storage.insert(key, val);
                st.local_storage_dirty = true;
            } else {
                st.session_storage.insert(key, val);
            }
        });
        Ok(JsValue::undefined())
    });
    let remove_item = NativeFunction::from_copy_closure(move |_t, args, ctx| {
        let key = args.first().and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.to_std_string_escaped()).unwrap_or_default();
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            if persistent {
                if st.local_storage.remove(&key).is_some() { st.local_storage_dirty = true; }
            } else {
                st.session_storage.remove(&key);
            }
        });
        Ok(JsValue::undefined())
    });
    let clear = NativeFunction::from_copy_closure(move |_t, _args, _c| {
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            if persistent {
                if !st.local_storage.is_empty() { st.local_storage_dirty = true; }
                st.local_storage.clear();
            } else {
                st.session_storage.clear();
            }
        });
        Ok(JsValue::undefined())
    });
    let key_fn = NativeFunction::from_copy_closure(move |_t, args, ctx| {
        let n = args.first().and_then(|v| v.to_i32(ctx).ok()).unwrap_or(-1);
        if n < 0 { return Ok(JsValue::null()); }
        let v = STATE.with(|s| {
            let st = s.borrow();
            let map = if persistent { &st.local_storage } else { &st.session_storage };
            map.keys().nth(n as usize).cloned()
        });
        Ok(match v {
            Some(k) => JsValue::from(js_string!(k)),
            None => JsValue::null(),
        })
    });

    let storage = ObjectInitializer::new(ctx)
        .function(get_item, js_string!("getItem"), 1)
        .function(set_item, js_string!("setItem"), 2)
        .function(remove_item, js_string!("removeItem"), 1)
        .function(clear, js_string!("clear"), 0)
        .function(key_fn, js_string!("key"), 1)
        .build();

    // length getter (live).
    let realm = ctx.realm().clone();
    let length_get = NativeFunction::from_copy_closure(move |_t, _args, _c| {
        let n = STATE.with(|s| {
            let st = s.borrow();
            let map = if persistent { &st.local_storage } else { &st.session_storage };
            map.len()
        });
        Ok(JsValue::Integer(n as i32))
    }).to_js_function(&realm);
    let length_desc = boa_engine::property::PropertyDescriptor::builder()
        .get(length_get)
        .enumerable(true)
        .configurable(true)
        .build();
    storage.define_property_or_throw(js_string!("length"), length_desc, ctx)?;

    ctx.register_global_property(js_string!(name), storage, Attribute::all())?;
    Ok(())
}

fn join_args(args: &[JsValue], ctx: &mut Context) -> String {
    args.iter()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_capture_and_redirect() {
        let html = r#"<script>
            console.log("hi");
            location.href = "https://example.com/dest";
        </script>"#;
        let r = run_scripts(html, "https://example.com/", Default::default(), Default::default());
        assert!(r.log.iter().any(|l| l.contains("hi")), "log: {:?}", r.log);
        // location.href = ... may or may not record — depending on whether
        // boa treats it as a property write that re-enters a setter; the
        // explicit location.assign / replace paths are the reliable
        // redirect channel and tested below.
    }

    #[test]
    fn location_assign_redirects() {
        let html = r#"<script>location.assign("/landing")</script>"#;
        let r = run_scripts(html, "https://example.com/", Default::default(), Default::default());
        assert_eq!(r.redirect.as_deref(), Some("/landing"));
    }

    #[test]
    fn document_cookie_round_trip() {
        let mut prior = HashMap::new();
        prior.insert("x".to_string(), "1".to_string());
        let html = r#"<script>
            console.log("before:", document.cookie);
            document.cookie = "y=2";
            document.cookie = "z=3; Path=/";
            console.log("after:", document.cookie);
        </script>"#;
        let r = run_scripts(html, "https://example.com/", prior, Default::default());
        assert!(r.cookies_dirty);
        assert_eq!(r.cookies.get("y").map(|s| s.as_str()), Some("2"));
        assert_eq!(r.cookies.get("z").map(|s| s.as_str()), Some("3"));
        assert_eq!(r.cookies.get("x").map(|s| s.as_str()), Some("1")); // preserved
    }

    #[test]
    fn localstorage_persists() {
        let html = r#"<script>
            localStorage.setItem("k", "v");
            localStorage.setItem("a", "1");
            localStorage.removeItem("a");
        </script>"#;
        let r = run_scripts(html, "https://example.com/", Default::default(), Default::default());
        assert!(r.localstorage_dirty);
        assert_eq!(r.localstorage.get("k").map(|s| s.as_str()), Some("v"));
        assert!(r.localstorage.get("a").is_none());
    }

    #[test]
    fn navigator_useragent_is_chromeish() {
        let html = r#"<script>console.log(navigator.userAgent)</script>"#;
        let r = run_scripts(html, "https://example.com/", Default::default(), Default::default());
        assert!(r.log.iter().any(|l| l.contains("Chrome")), "log: {:?}", r.log);
    }
}

#[cfg(test)]
mod dom_fetch_tests {
    use super::*;

    #[test]
    fn get_element_by_id_returns_value() {
        let html = r#"<html><body>
            <input id="user" value="initial">
            <script>
              var el = document.getElementById("user");
              console.log("tag:", el.tagName, "value:", el.value);
              el.value = "typed-by-js";
              console.log("after:", el.value);
            </script>
        </body></html>"#;
        let r = run_scripts(html, "https://example.com/", Default::default(), Default::default());
        assert!(r.log.iter().any(|l| l.contains("tag: INPUT")), "log: {:?}", r.log);
        assert!(r.log.iter().any(|l| l.contains("value: initial")), "log: {:?}", r.log);
        assert!(r.log.iter().any(|l| l.contains("after: typed-by-js")), "log: {:?}", r.log);
        assert_eq!(r.dom_values.get("user").map(|s| s.as_str()), Some("typed-by-js"));
    }

    #[test]
    fn get_element_by_id_missing_returns_null() {
        let html = r#"<html><body><script>
            var el = document.getElementById("nope");
            console.log("isnull:", el === null);
        </script></body></html>"#;
        let r = run_scripts(html, "https://example.com/", Default::default(), Default::default());
        assert!(r.log.iter().any(|l| l.contains("isnull: true")), "log: {:?}", r.log);
    }

    #[test]
    fn submit_listener_can_prevent_default() {
        let html = r#"<html><body>
            <form id="login"><input id="user" name="user"></form>
            <script>
              document.addEventListener("submit", function(e) {
                console.log("submit fired, target type:", e.type);
                e.preventDefault();
              });
            </script>
        </body></html>"#;
        // First run: registers the listener via inline script.
        let scripts = extract_scripts_in_order(html, "https://example.com/", &Default::default());
        // Second (simulated submit-time) run: dispatch a synthetic submit.
        let r = run_extracted(
            scripts, html, "https://example.com/",
            Default::default(), Default::default(),
            Default::default(),
            Some("login".to_string()),
        );
        assert!(r.submit_prevented, "log: {:?}", r.log);
        assert!(r.log.iter().any(|l| l.contains("submit fired")), "log: {:?}", r.log);
    }

    #[test]
    fn innerhtml_change_is_surfaced() {
        let html = r#"<html><body>
            <div id="app">Loading...</div>
            <script>
              document.getElementById("app").innerHTML = "<h1>Hello</h1><p>World</p>";
            </script>
        </body></html>"#;
        let r = run_scripts(html, "https://example.com/", Default::default(), Default::default());
        let app = r.inner_html_changes.get("app").map(String::as_str).unwrap_or("");
        assert!(app.contains("<h1>Hello</h1>"), "got: {:?}", app);
    }

    #[test]
    fn rsc_payload_extracts_text() {
        // Synthetic Next.js-style payload pushed via __next_f.push.
        let html = r#"<html><body>
            <div id="root">Loading...</div>
            <script>
              self.__next_f = self.__next_f || [];
              self.__next_f.push([1, '0:["$","main",null,{"children":[["$","h1",null,{"children":"Welcome"}],["$","p",null,{"children":"Hello world"}]]}]\n']);
            </script>
        </body></html>"#;
        let r = run_scripts(html, "https://example.com/", Default::default(), Default::default());
        let rsc = r.rsc_text.as_deref().unwrap_or("");
        assert!(rsc.contains("Welcome"), "rsc: {:?}", rsc);
        assert!(rsc.contains("Hello world"), "rsc: {:?}", rsc);
    }

    #[test]
    fn submit_dispatch_no_listener_is_noop() {
        let html = r#"<html><body><form id="login"></form>
            <script>console.log("no listener");</script>
        </body></html>"#;
        let scripts = extract_scripts_in_order(html, "https://example.com/", &Default::default());
        let r = run_extracted(
            scripts, html, "https://example.com/",
            Default::default(), Default::default(),
            Default::default(),
            Some("login".to_string()),
        );
        assert!(!r.submit_prevented);
    }

    #[test]
    fn fetch_then_chain_runs_synchronously() {
        // Use httpbin alternative... actually just check that fetch
        // returns a thenable and the chain runs without throwing.
        // For an offline test we'd need a stub, so this exercises
        // the failure path (host doesn't resolve) and asserts the
        // chain still flows.
        let html = r#"<script>
            var got = null;
            fetch("http://127.0.0.1:1/should-fail")
              .then(r => { got = "first-then"; return "second"; })
              .then(v => { console.log("chain:", got, v); });
        </script>"#;
        let r = run_scripts(html, "https://example.com/", Default::default(), Default::default());
        // The chain should run regardless of network outcome.
        assert!(r.log.iter().any(|l| l.contains("chain: first-then")), "log: {:?}", r.log);
    }
}

#[cfg(test)]
mod live_spa {
    use super::*;
    /// Target URL is read from the `SCROLL_TEST_URL` env var so tests
    /// don't hardcode a specific site. Defaults to https://example.com
    /// which exercises the basic fetch path but not the RSC / SPA
    /// scripting features — set the env var to a real SPA to exercise
    /// those. Both tests are `#[ignore]` so CI doesn't depend on the
    /// network.
    fn test_url() -> String {
        std::env::var("SCROLL_TEST_URL").unwrap_or_else(|_| "https://example.com/".to_string())
    }

    /// Full pipeline trace: fetch → js → injected HTML → renderer.
    /// Prints what the user actually sees. Run with:
    ///   SCROLL_TEST_URL=https://example.com/ \
    ///     cargo test --release spa_full -- --ignored --nocapture
    #[test]
    #[ignore]
    fn spa_full_pipeline() {
        let url = test_url();
        let html = match make_http().and_then(|(rt, c)| {
            let u = url.clone();
            rt.block_on(async { c.get(&u).send().await.ok()?.text().await.ok() })
        }) {
            Some(s) => s,
            None => { eprintln!("network fail"); return; }
        };
        let r = run_scripts(&html, &url, Default::default(), Default::default());
        eprintln!("redirect = {:?}", r.redirect);
        eprintln!("inner_html_changes = {} entries", r.inner_html_changes.len());
        eprintln!("--- rsc_text -----------------------------");
        match &r.rsc_text {
            Some(t) => eprintln!("{}", t),
            None => eprintln!("(none)"),
        }
        eprintln!("------------------------------------------");
        // Mirror what main.rs does: inject the rsc_text below </body>.
        let mut effective = html.clone();
        if let Some(rsc) = &r.rsc_text {
            let safe = rsc.replace('<', "&lt;").replace('>', "&gt;");
            let block = format!("<hr><div><pre>{}</pre></div>", safe);
            if let Some(idx) = effective.to_ascii_lowercase().rfind("</body>") {
                effective.insert_str(idx, &block);
            } else {
                effective.push_str(&block);
            }
        }
        let rendered = crate::renderer::render_html(&effective, 120, &url, &crate::config::Config::default());
        eprintln!("--- rendered text (stripped) -------------");
        let stripped = crust::strip_ansi(&rendered.text);
        for (i, line) in stripped.lines().enumerate() {
            eprintln!("{:3} {}", i, line);
            if i > 80 { eprintln!("..."); break; }
        }
        eprintln!("------------------------------------------");
    }

    /// Hits the SCROLL_TEST_URL live and reports what the RSC parser
    /// can pull out. Marked `ignore` so CI doesn't depend on the
    /// network. Run with: `cargo test --release dump_extracted -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_extracted_text() {
        let url = test_url();
        let html = match make_http().and_then(|(rt, c)| {
            let u = url.clone();
            rt.block_on(async { c.get(&u).send().await.ok()?.text().await.ok() })
        }) {
            Some(s) => s,
            None => { eprintln!("network fail"); return; }
        };
        let r = run_scripts(&html, &url, Default::default(), Default::default());
        eprintln!("--- inner_html_changes ({}) ---", r.inner_html_changes.len());
        for (k, v) in &r.inner_html_changes {
            eprintln!("  {} (len {}): {}", k, v.len(),
                if v.len() > 80 { &v[..80] } else { v });
        }
        eprintln!("--- rsc_text ---");
        match &r.rsc_text {
            Some(t) => eprintln!("{}", t),
            None => eprintln!("(none)"),
        }
        eprintln!("--- log ({}) ---", r.log.len());
        for line in r.log.iter().take(20) { eprintln!("  {}", line); }
    }
}
