//! Today's incident (2026-10-07) against a local server: the cached homepage still references a
//! CSS file the last build deleted. Expect one push per issue, silence on repeat, then resolved.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sitewatch::checks::Check;
use sitewatch::config::Config;
use sitewatch::run::{self, Event, Mode, Options, State};
use tiny_http::{Header, Response, Server};

fn page(css: &str, nav: &str) -> String {
    format!(
        r#"<!doctype html><html><head><title>t</title>
<link rel="stylesheet" href="/build/assets/{css}"><script type="module" src="/build/assets/site-B.js"></script></head>
<body><header><nav id="desktop-nav">{nav}</nav></header><main><h1>x</h1><p>text</p></main><footer>f</footer></body></html>"#
    )
}

const NAV: &str = r#"<a href="/rezepte">Rezepte</a><a href="/magazin">Magazin</a>"#;

fn serve(fixed: Arc<AtomicBool>) -> u16 {
    let server = Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let url = req.url().to_string();
            let path = url.split('?').next().unwrap_or_default().to_string();
            let (status, ctype, body) = match path.as_str() {
                "/build/manifest.json" => (200, "application/json", r#"{"resources/css/site.css":{"file":"assets/site-NEW.css"},"resources/js/site.js":{"file":"assets/site-B.js"}}"#.to_string()),
                "/build/assets/site-NEW.css" => (200, "text/css", "body{}".into()),
                "/build/assets/site-B.js" => (200, "application/javascript", "1".into()),
                "/sitemap.xml" => (200, "application/xml", format!("<urlset><url><loc>http://127.0.0.1:{port}/a</loc></url></urlset>")),
                "/rezepte" | "/a" => (200, "text/html; charset=utf-8", page("site-NEW.css", NAV)),
                "/" if fixed.load(Ordering::SeqCst) => (200, "text/html; charset=utf-8", page("site-NEW.css", NAV)),
                "/" => (200, "text/html; charset=utf-8", page("site-OLD.css", NAV)),
                _ => (404, "text/html", "not found".into()),
            };
            let resp = Response::from_string(body)
                .with_status_code(status)
                .with_header(Header::from_bytes("Content-Type", ctype).unwrap());
            let _ = req.respond(resp);
        }
    });
    port
}

#[test]
fn stale_cached_page_is_reported_once_then_resolved() {
    let fixed = Arc::new(AtomicBool::new(false));
    let port = serve(fixed.clone());
    let cfg = Config::parse(&format!(
        r##"
[notify]
enabled = false
[[sites]]
name = "test"
base_url = "http://127.0.0.1:{port}"
manifest = "/build/manifest.json"
sitemap = "/sitemap.xml"
fresh_reference = "/rezepte?sitewatch={{nonce}}"
layout_fingerprint = ["#desktop-nav a"]
quick_pages = ["/"]
quick_rate = 1000.0
[[sites.page_types]]
name = "any"
match = ".*"
require = ["header", "main", "footer"]
forbid_text = ["{{{{"]
"##
    ))
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let opts = Options { mode: Mode::Quick, state_dir: dir.path().into(), dry_run: false };
    let mut state = State::default();

    let out = run::run(&cfg, &opts, &mut state).unwrap();
    let mut new: Vec<Check> = out.events.iter().filter_map(|e| if let Event::New(i) = e { Some(i.check) } else { None }).collect();
    new.sort();
    assert_eq!(new, [Check::AssetMissing, Check::StaleBuild], "events: {:?}", out.events);
    assert!(state.issues.values().all(|i| i.healable && i.paths == ["/"]));
    assert_eq!(out.sites[0].checked, 2, "/ and the sitemap's /a");
    assert_eq!(sitewatch::notify::messages(&cfg, &out, &mut state).len(), 2);

    let out = run::run(&cfg, &opts, &mut state).unwrap();
    assert!(out.events.is_empty(), "repeat must be silent: {:?}", out.events);

    fixed.store(true, Ordering::SeqCst);
    let out = run::run(&cfg, &opts, &mut state).unwrap();
    assert_eq!(out.events.iter().filter(|e| matches!(e, Event::Resolved(_))).count(), 2);
    assert!(state.issues.is_empty());
    let msgs = sitewatch::notify::messages(&cfg, &out, &mut state);
    assert!(msgs.iter().all(|m| m.event == "sitewatch.resolved") && msgs.len() == 2);
}
