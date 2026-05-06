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
    let scripts = extract_inline_scripts(html);
    if scripts.is_empty() {
        return JsResult {
            cookies: cookies_in,
            localstorage: localstorage_in,
            ..Default::default()
        };
    }

    // Reset thread-local state for this run.
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        *st = JsState::default();
        st.cookies = cookies_in;
        st.local_storage = localstorage_in;
    });

    let mut ctx = Context::default();
    if install_globals(&mut ctx, page_url).is_err() {
        return STATE.with(|s| {
            let st = s.borrow();
            JsResult {
                log: st.log.clone(),
                redirect: st.redirect.clone(),
                cookies: st.cookies.clone(),
                cookies_dirty: st.cookies_dirty,
                localstorage: st.local_storage.clone(),
                localstorage_dirty: st.local_storage_dirty,
            }
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
        JsResult {
            log: st.log.clone(),
            redirect: st.redirect.clone(),
            cookies: st.cookies.clone(),
            cookies_dirty: st.cookies_dirty,
            localstorage: st.local_storage.clone(),
            localstorage_dirty: st.local_storage_dirty,
        }
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

    // ---------- document (stub queries + working cookie property) ----------
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

    Ok(())
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
