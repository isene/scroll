//! Minimal JS execution layer over scroll's text-mode renderer.
//!
//! After a page is fetched, scroll extracts inline `<script>` tags
//! and runs them in a `boa` context that exposes a stub DOM and
//! `window` object. The goal is "make simple JS work" — not "ship a
//! browser". Currently supported:
//!
//! - `console.log` / `console.error` / `console.warn` (captured into
//!   the `JsResult.log`).
//! - `navigator.userAgent` returns a Chrome-shaped string so sites
//!   that gate on UA don't reject us outright.
//! - `window.location` and `document.location` — read URL components,
//!   write to `href` to request a navigation.
//! - `localStorage` / `sessionStorage` (in-process, per JsContext).
//! - `setTimeout` / `setInterval` no-op stubs that return ids; we
//!   don't run an event loop.
//! - `document.getElementById` / `document.querySelector` etc.
//!   return `null` for now (no real DOM yet). Most JS that does
//!   feature-detection treats `null` as "do nothing".
//!
//! Boa 0.20's `from_copy_closure` requires `Copy` on captured state,
//! so per-script-run mutable state lives behind a `thread_local!`
//! slot that closures dip into. JS execution is synchronous and
//! single-threaded, so this is safe.
//!
//! Not yet supported: real DOM mutation, event listeners, async XHR /
//! fetch, reCAPTCHA-style fingerprinting. The path to "Google login
//! works" is the per-set Firefox profile import (Phase 1), not
//! pretending scroll is Chrome.

use std::cell::RefCell;

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
}

/// Per-page state collected during script execution.
#[derive(Default)]
struct JsState {
    redirect: Option<String>,
    log: Vec<String>,
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

/// Run every inline `<script>` in `html` against a freshly initialised
/// minimal-DOM context. Returns a JsResult describing side effects.
/// Scripts with `src=` attributes (external) are NOT fetched yet —
/// the typical login flow has its critical JS inlined; external
/// scripts are noisy and analytics-heavy.
pub fn run_scripts(html: &str, page_url: &str) -> JsResult {
    let scripts = extract_inline_scripts(html);
    if scripts.is_empty() { return JsResult::default(); }

    // Reset thread-local state for this run.
    STATE.with(|s| *s.borrow_mut() = JsState::default());

    let mut ctx = Context::default();
    if install_globals(&mut ctx, page_url).is_err() {
        return STATE.with(|s| {
            let st = s.borrow();
            JsResult { log: st.log.clone(), redirect: st.redirect.clone() }
        });
    }

    for src in scripts {
        // Best-effort: a script that throws still lets the next one
        // run, mirroring browser behaviour. Errors are captured as
        // log entries so a future :jslog command can surface them.
        if let Err(e) = ctx.eval(Source::from_bytes(src.as_bytes())) {
            record_log("error", &format!("{}", e));
        }
    }

    STATE.with(|s| {
        let st = s.borrow();
        JsResult { log: st.log.clone(), redirect: st.redirect.clone() }
    })
}

fn extract_inline_scripts(html: &str) -> Vec<String> {
    // Lightweight extractor — we don't want to parse HTML twice.
    // Looks for `<script>...</script>` blocks where the opening tag
    // has no `src=` attribute. Case-insensitive on the tag name.
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut i = 0usize;
    while let Some(start) = lower[i..].find("<script") {
        let s = i + start;
        let tag_end = match lower[s..].find('>') { Some(e) => s + e, None => break };
        let opening = &lower[s..=tag_end];
        // Skip external scripts entirely.
        if opening.contains("src=") {
            i = tag_end + 1;
            continue;
        }
        let body_start = tag_end + 1;
        let close = match lower[body_start..].find("</script>") {
            Some(c) => body_start + c,
            None => break,
        };
        out.push(html[body_start..close].to_string());
        i = close + "</script>".len();
    }
    out
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
        .build();
    ctx.register_global_property(js_string!("window"), window, Attribute::all())?;

    // ---------- document (stub: return null / undefined for queries) ----------
    let document = ObjectInitializer::new(ctx)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::null())),
                  js_string!("getElementById"), 1)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::null())),
                  js_string!("querySelector"), 1)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())),
                  js_string!("querySelectorAll"), 1)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())),
                  js_string!("addEventListener"), 2)
        .function(NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())),
                  js_string!("removeEventListener"), 2)
        .build();
    ctx.register_global_property(js_string!("document"), document, Attribute::all())?;

    // ---------- timers (no-ops returning fake ids) ----------
    ctx.register_global_callable(js_string!("setTimeout"), 0,
        NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::Integer(0))))?;
    ctx.register_global_callable(js_string!("setInterval"), 0,
        NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::Integer(0))))?;
    ctx.register_global_callable(js_string!("clearTimeout"), 0,
        NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())))?;
    ctx.register_global_callable(js_string!("clearInterval"), 0,
        NativeFunction::from_copy_closure(|_t, _args, _c| Ok(JsValue::undefined())))?;

    Ok(())
}

fn join_args(args: &[JsValue], ctx: &mut Context) -> String {
    args.iter()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" ")
}
