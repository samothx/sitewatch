//! What sitewatch reads out of one HTML page.

use scraper::{ElementRef, Html, Node, Selector};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    Style,
    Script,
    Image,
}

pub struct Page {
    html: Html,
    raw_len: usize,
    ends_with_html: bool,
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("static selector")
}

impl Page {
    pub fn parse(text: &str) -> Self {
        let tail = text.trim_end().as_bytes();
        let ends_with_html = tail.len() >= 7 && tail[tail.len() - 7..].eq_ignore_ascii_case(b"</html>");
        Self { html: Html::parse_document(text), raw_len: text.len(), ends_with_html }
    }

    pub fn byte_len(&self) -> usize {
        self.raw_len
    }

    /// False when the document was cut off (no closing `</html>`).
    pub fn complete(&self) -> bool {
        self.ends_with_html
    }

    /// CSS/JS (and with `images`, `<img src>`) referenced by the page, resolved against `base`.
    pub fn assets(&self, base: &Url, images: bool) -> Vec<(Url, AssetKind)> {
        let mut out: Vec<(Url, AssetKind)> = vec![];
        let mut add = |href: Option<&str>, kind| {
            if let Some(u) = href.and_then(|h| base.join(h.trim()).ok())
                && matches!(u.scheme(), "http" | "https") && !out.iter().any(|(o, _)| *o == u) {
                    out.push((u, kind));
                }
        };
        for el in self.html.select(&sel("link[href]")) {
            let rel = el.value().attr("rel").unwrap_or_default().to_ascii_lowercase();
            let r#as = el.value().attr("as").unwrap_or_default();
            let kind = match () {
                _ if rel.split_whitespace().any(|r| r == "stylesheet") => AssetKind::Style,
                _ if rel.contains("modulepreload") => AssetKind::Script,
                _ if rel.contains("preload") && r#as == "style" => AssetKind::Style,
                _ if rel.contains("preload") && r#as == "script" => AssetKind::Script,
                _ => continue,
            };
            add(el.value().attr("href"), kind);
        }
        for el in self.html.select(&sel("script[src]")) {
            add(el.value().attr("src"), AssetKind::Script);
        }
        if images {
            for el in self.html.select(&sel("img[src]")) {
                let src = el.value().attr("src").unwrap_or_default();
                if !src.starts_with("data:") {
                    add(Some(src), AssetKind::Image);
                }
            }
        }
        out
    }

    /// Hrefs + texts of everything the selectors match, one line each; empty if nothing matched.
    pub fn fingerprint(&self, selectors: &[Selector]) -> String {
        let mut lines = vec![];
        for s in selectors {
            for el in self.html.select(s) {
                let href = el.value().attr("href").unwrap_or_default();
                let text = collapse(&visible_text(el));
                lines.push(format!("{href}\t{text}"));
            }
        }
        lines.join("\n")
    }

    pub fn has(&self, selector: &Selector) -> bool {
        self.html.select(selector).next().is_some()
    }

    /// Visible text of `<main>` (or the whole document without it), whitespace collapsed.
    pub fn main_text(&self) -> String {
        let root = self.html.select(&sel("main")).next().unwrap_or_else(|| self.html.root_element());
        collapse(&visible_text(root))
    }

    pub fn body_text(&self) -> String {
        let root = self.html.select(&sel("body")).next().unwrap_or_else(|| self.html.root_element());
        collapse(&visible_text(root))
    }
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Text nodes below `el`, skipping script/style/noscript/template (inline JS may contain `{{`).
fn visible_text(el: ElementRef) -> String {
    const HIDDEN: [&str; 4] = ["script", "style", "noscript", "template"];
    let mut out = String::new();
    for node in el.descendants() {
        if let Node::Text(t) = node.value() {
            let hidden = node.ancestors().any(|a| a.value().as_element().is_some_and(|e| HIDDEN.contains(&e.name())));
            if !hidden {
                out.push_str(t);
                out.push(' ');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html><html><head>
        <link rel="preload" as="style" href="https://x.test/build/assets/site-A.css">
        <link rel="stylesheet" href="https://x.test/build/assets/site-A.css">
        <link rel="modulepreload" href="/build/assets/site-B.js">
        <script type="module" src="/build/assets/site-B.js"></script>
        <script defer src="https://analytics.x.test/script.js"></script>
        <script>var t = "{{ not visible }}";</script>
        </head><body><header><nav id="desktop-nav"><a href="/rezepte">Rezepte</a><a href="/magazin"> Magazin </a></nav></header>
        <main><h1>Titel</h1><p>Text <img src="/img/a.jpg"> <img src="data:image/gif;base64,R0"></p></main>
        <footer>f</footer></body></html>
    "#;

    #[test]
    fn extracts_assets_dedups_and_resolves() {
        let p = Page::parse(PAGE);
        let base = Url::parse("https://x.test/rezepte/a").unwrap();
        let a: Vec<String> = p.assets(&base, false).iter().map(|(u, _)| u.to_string()).collect();
        assert_eq!(
            a,
            ["https://x.test/build/assets/site-A.css", "https://x.test/build/assets/site-B.js", "https://analytics.x.test/script.js"]
        );
        assert_eq!(p.assets(&base, true).iter().filter(|(_, k)| *k == AssetKind::Image).count(), 1);
    }

    #[test]
    fn text_skips_scripts_and_detects_truncation() {
        let p = Page::parse(PAGE);
        assert!(p.complete());
        assert!(!p.body_text().contains("{{"));
        assert_eq!(p.main_text(), "Titel Text");
        assert!(!Page::parse(&PAGE[..PAGE.len() - 30]).complete());
    }

    #[test]
    fn fingerprint_lists_links() {
        let p = Page::parse(PAGE);
        assert_eq!(p.fingerprint(&[sel("#desktop-nav a")]), "/rezepte\tRezepte\n/magazin\tMagazin");
    }
}
