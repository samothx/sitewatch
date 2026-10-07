//! The checks. `SiteCtx::prepare` loads what a fresh render looks like (manifest, layout
//! fingerprint of the cache-bypassing reference page); `check_page` compares one page against it.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use opskit::Severity;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::Site;
use crate::fetch::Fetcher;
use crate::page::{AssetKind, Page};

/// Images checked per page (CSS/JS always all): a listing page has ~100.
const MAX_IMAGES_PER_PAGE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Check {
    #[serde(rename = "site.down")]
    SiteDown,
    #[serde(rename = "asset.missing")]
    AssetMissing,
    #[serde(rename = "page.status")]
    PageStatus,
    #[serde(rename = "page.incomplete")]
    PageIncomplete,
    #[serde(rename = "cache.stale_build")]
    StaleBuild,
    #[serde(rename = "cache.stale_layout")]
    StaleLayout,
    #[serde(rename = "page.shrunk")]
    PageShrunk,
    #[serde(rename = "site.slow")]
    SiteSlow,
}

impl Check {
    pub fn id(self) -> &'static str {
        match self {
            Self::SiteDown => "site.down",
            Self::AssetMissing => "asset.missing",
            Self::PageStatus => "page.status",
            Self::PageIncomplete => "page.incomplete",
            Self::StaleBuild => "cache.stale_build",
            Self::StaleLayout => "cache.stale_layout",
            Self::PageShrunk => "page.shrunk",
            Self::SiteSlow => "site.slow",
        }
    }

    pub fn severity(self) -> Severity {
        match self {
            Self::SiteDown => Severity::Critical,
            Self::AssetMissing | Self::PageStatus | Self::PageIncomplete => Severity::High,
            Self::StaleBuild | Self::StaleLayout | Self::PageShrunk => Severity::Medium,
            Self::SiteSlow => Severity::Low,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::SiteDown => "site unreachable (manifest or fresh reference page)",
            Self::AssetMissing => "page references a CSS/JS/image file that does not load",
            Self::PageStatus => "page does not answer 200",
            Self::PageIncomplete => "page is incomplete (missing parts, cut off, error text)",
            Self::StaleBuild => "cached page references an old CSS/JS build",
            Self::StaleLayout => "cached page shows an older header menu than a fresh render",
            Self::PageShrunk => "page is much smaller than usual",
            Self::SiteSlow => "page answered slowly",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub check: Check,
    pub path: String,
    pub detail: String,
    /// Fixed by clearing the static cache (a stale reference, not a broken site).
    pub healable: bool,
}

impl Finding {
    fn new(check: Check, path: &str, detail: impl Into<String>, healable: bool) -> Self {
        Self { check, path: path.to_string(), detail: detail.into(), healable }
    }
}

/// CSS/JS files of the current build, as absolute paths (`/build/assets/site-X.css`).
#[derive(Debug, Default)]
pub struct Manifest {
    pub prefix: String,
    pub files: HashSet<String>,
}

impl Manifest {
    /// Vite manifest: `{ "resources/css/site.css": { "file": "assets/site-X.css", "css": [...] } }`;
    /// paths are relative to the manifest's directory.
    pub fn parse(json: &str, manifest_path: &str) -> anyhow::Result<Self> {
        let v: serde_json::Value = serde_json::from_str(json)?;
        let dir = manifest_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let prefix = format!("{dir}/");
        let mut files = HashSet::new();
        for entry in v.as_object().ok_or_else(|| anyhow::anyhow!("manifest is not an object"))?.values() {
            if let Some(f) = entry.get("file").and_then(|f| f.as_str()) {
                files.insert(format!("{prefix}{f}"));
            }
            for key in ["css", "assets"] {
                for f in entry.get(key).and_then(|a| a.as_array()).into_iter().flatten().filter_map(|f| f.as_str()) {
                    files.insert(format!("{prefix}{f}"));
                }
            }
        }
        if files.is_empty() {
            anyhow::bail!("manifest lists no files");
        }
        Ok(Self { prefix, files })
    }

    /// A build file (below the manifest's directory) that the current build does not have.
    pub fn is_stale(&self, path: &str) -> bool {
        path.starts_with(&self.prefix) && !path.ends_with("/manifest.json") && !self.files.contains(path)
    }
}

pub struct SiteCtx<'a> {
    pub site: &'a Site,
    pub fetcher: Fetcher,
    pub manifest: Option<Manifest>,
    pub reference_fp: Option<String>,
    pub images: bool,
    pub slow: Duration,
    assets: RefCell<HashMap<Url, Option<String>>>,
}

impl<'a> SiteCtx<'a> {
    /// Loads manifest + reference fingerprint. Err = `site.down` finding: page checks would only
    /// produce noise.
    pub fn prepare(site: &'a Site, fetcher: Fetcher, images: bool, slow: Duration) -> Result<Self, Finding> {
        let mut ctx = Self { site, fetcher, manifest: None, reference_fp: None, images, slow, assets: RefCell::default() };
        if let Some(m) = &site.manifest {
            let text = ctx.get_ok(m).map_err(|e| Finding::new(Check::SiteDown, m, e, false))?;
            ctx.manifest = Some(Manifest::parse(&text, m).map_err(|e| Finding::new(Check::SiteDown, m, format!("manifest: {e}"), false))?);
        }
        if let Some(r) = &site.fresh_reference {
            let path = r.replace("{nonce}", &chrono::Utc::now().timestamp_millis().to_string());
            let text = ctx.get_ok(&path).map_err(|e| Finding::new(Check::SiteDown, r, e, false))?;
            if !site.layout_fingerprint.is_empty() {
                let fp = Page::parse(&text).fingerprint(&site.layout_fingerprint);
                if fp.is_empty() {
                    return Err(Finding::new(Check::SiteDown, r, "fresh reference: layout_fingerprint matches nothing", false));
                }
                ctx.reference_fp = Some(fp);
            }
        }
        Ok(ctx)
    }

    fn get_ok(&self, path: &str) -> Result<String, String> {
        let url = self.site.url(path).map_err(|e| e.to_string())?;
        let r = self.fetcher.get(url.as_str()).map_err(|e| e.to_string())?;
        if r.status != 200 {
            return Err(format!("HTTP {}", r.status));
        }
        Ok(r.text())
    }

    /// All findings for one page, plus its size when it rendered completely (size baseline).
    pub fn check_page(&self, path: &str) -> (Vec<Finding>, Option<usize>) {
        let mut out = vec![];
        let url = match self.site.url(path) {
            Ok(u) => u,
            Err(e) => return (vec![Finding::new(Check::PageStatus, path, e.to_string(), false)], None),
        };
        let resp = match self.fetcher.get(url.as_str()) {
            Ok(r) => r,
            Err(e) => return (vec![Finding::new(Check::PageStatus, path, e.to_string(), false)], None),
        };
        if resp.status != 200 {
            let to = resp.location.as_deref().map(|l| format!(" → {l}")).unwrap_or_default();
            return (vec![Finding::new(Check::PageStatus, path, format!("HTTP {}{to}", resp.status), false)], None);
        }
        if resp.elapsed > self.slow {
            out.push(Finding::new(Check::SiteSlow, path, format!("{} ms", resp.elapsed.as_millis()), false));
        }
        if !resp.content_type.starts_with("text/html") {
            out.push(Finding::new(Check::PageIncomplete, path, format!("content type {:?}", resp.content_type), false));
            return (out, None);
        }
        let page = Page::parse(&resp.text());
        let before = out.len();
        if !page.complete() {
            out.push(Finding::new(Check::PageIncomplete, path, "cut off (no </html>)", false));
        }
        let body_text = page.body_text();
        for t in self.site.types_for(path) {
            let missing: Vec<&str> = t.require.iter().filter(|(_, s)| !page.has(s)).map(|(n, _)| n.as_str()).collect();
            if !missing.is_empty() {
                out.push(Finding::new(Check::PageIncomplete, path, format!("{}: missing {}", t.name, missing.join(", ")), false));
            }
            for f in &t.forbid_text {
                if body_text.contains(f.as_str()) {
                    out.push(Finding::new(Check::PageIncomplete, path, format!("{}: contains {f:?}", t.name), false));
                }
            }
            if let Some(min) = t.min_text {
                let n = page.main_text().chars().count();
                if n < min {
                    out.push(Finding::new(Check::PageIncomplete, path, format!("{}: {n} chars of text, expected ≥ {min}", t.name), false));
                }
            }
        }
        let complete = out.len() == before;

        if let (Some(reference), false) = (&self.reference_fp, self.site.layout_fingerprint.is_empty()) {
            let fp = page.fingerprint(&self.site.layout_fingerprint);
            if !fp.is_empty() && fp != *reference {
                out.push(Finding::new(Check::StaleLayout, path, layout_diff(reference, &fp), true));
            }
        }

        let mut images = 0;
        for (asset, kind) in page.assets(&url, self.images) {
            if asset.host_str() != self.site.base.host_str() {
                continue;
            }
            if kind == AssetKind::Image {
                images += 1;
                if images > MAX_IMAGES_PER_PAGE {
                    continue;
                }
            }
            let stale = self.manifest.as_ref().is_some_and(|m| m.is_stale(asset.path()));
            if stale {
                out.push(Finding::new(Check::StaleBuild, path, asset.path().to_string(), true));
            }
            if let Some(err) = self.asset_error(&asset, kind) {
                out.push(Finding::new(Check::AssetMissing, path, format!("{} → {err}", asset.path()), stale));
            }
        }
        (out, complete.then_some(page.byte_len()))
    }

    /// Cached per run: `None` = loads fine.
    fn asset_error(&self, url: &Url, kind: AssetKind) -> Option<String> {
        if let Some(r) = self.assets.borrow().get(url) {
            return r.clone();
        }
        let r = match self.fetcher.get(url.as_str()) {
            Err(e) => Some(e.to_string()),
            Ok(r) if r.status != 200 => Some(format!("HTTP {}", r.status)),
            Ok(r) => {
                let ok = match kind {
                    AssetKind::Style => r.content_type.starts_with("text/css"),
                    AssetKind::Script => r.content_type.contains("javascript"),
                    AssetKind::Image => r.content_type.starts_with("image/"),
                };
                (!ok).then(|| format!("content type {:?}", r.content_type))
            }
        };
        self.assets.borrow_mut().insert(url.clone(), r.clone());
        r
    }
}

/// Short human summary of how a cached menu differs from the fresh one.
fn layout_diff(reference: &str, cached: &str) -> String {
    let r: HashSet<&str> = reference.lines().collect();
    let c: HashSet<&str> = cached.lines().collect();
    let only = |a: &HashSet<&str>, b: &HashSet<&str>| {
        let mut v: Vec<String> = a.difference(b).map(|l| l.split('\t').next().unwrap_or_default().to_string()).collect();
        v.sort();
        v.truncate(3);
        v.join(", ")
    };
    let (new, old) = (only(&r, &c), only(&c, &r));
    match (new.is_empty(), old.is_empty()) {
        (true, true) => "menu order differs".into(),
        (false, true) => format!("missing in cache: {new}"),
        (true, false) => format!("removed since: {old}"),
        (false, false) => format!("missing in cache: {new}; removed since: {old}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_paths_and_staleness() {
        let m = Manifest::parse(
            r#"{"resources/css/site.css":{"file":"assets/site-A.css","src":"x","isEntry":true},
                "resources/js/site.js":{"file":"assets/site-B.js","css":["assets/extra-C.css"]}}"#,
            "/build/manifest.json",
        )
        .unwrap();
        assert!(!m.is_stale("/build/assets/site-A.css"));
        assert!(!m.is_stale("/build/assets/extra-C.css"));
        assert!(m.is_stale("/build/assets/site-OLD.css"));
        assert!(!m.is_stale("/img/asset/x.jpg"));
    }

    #[test]
    fn layout_diff_names_links() {
        assert_eq!(layout_diff("/a\tA\n/b\tB", "/a\tA\n/c\tC"), "missing in cache: /b; removed since: /c");
    }
}
