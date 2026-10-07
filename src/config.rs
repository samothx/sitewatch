//! `/etc/sitewatch/config.toml`: global settings plus one `[[sites]]` table per watched site.
//! Selectors and patterns are compiled once at load time, so `config check` catches typos.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use opskit::Severity;
use regex::Regex;
use scraper::Selector;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub notify: Notify,
    #[serde(default)]
    pub mail: Mail,
    #[serde(default)]
    pub heal: Heal,
    #[serde(rename = "sites")]
    pub raw_sites: Vec<RawSite>,
    #[serde(skip)]
    pub sites: Vec<Site>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct General {
    pub state_dir: PathBuf,
    pub timeout_secs: u64,
    /// A page slower than this is reported as `site.slow`.
    pub slow_ms: u64,
}

impl Default for General {
    fn default() -> Self {
        Self { state_dir: "/var/lib/sitewatch".into(), timeout_secs: 20, slow_ms: 3000 }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Notify {
    pub enabled: bool,
    pub command: PathBuf,
    /// New/escalated issues at or above this severity are pushed.
    pub min_severity: Severity,
    /// One summary push after every full run.
    pub digest: bool,
}

impl Default for Notify {
    fn default() -> Self {
        Self {
            enabled: true,
            command: "/usr/local/bin/ntfy-dispatcher".into(),
            min_severity: Severity::Medium,
            digest: true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Mail {
    pub enabled: bool,
    pub from: String,
    pub to: Vec<String>,
    pub sendmail: PathBuf,
    pub html: bool,
    /// Mail the report when a new issue at or above this severity appears, and after a full
    /// run that still has open issues at or above it.
    pub min_severity: Severity,
}

impl Default for Mail {
    fn default() -> Self {
        Self {
            enabled: false,
            from: String::new(),
            to: vec![],
            sendmail: "/usr/sbin/sendmail".into(),
            html: true,
            min_severity: Severity::Medium,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Heal {
    /// Write `<state_dir>/heal/<site>.request` when every open issue of a site is a stale cache
    /// reference; `sitewatch-heal@<site>.path` then clears and re-warms that site's cache.
    pub enabled: bool,
    pub cooldown_hours: i64,
}

impl Default for Heal {
    fn default() -> Self {
        Self { enabled: false, cooldown_hours: 6 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageCheck {
    None,
    Quick,
    All,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSite {
    pub name: String,
    pub base_url: String,
    /// Vite manifest, e.g. `/build/manifest.json`: the CSS/JS names a fresh render uses.
    pub manifest: Option<String>,
    pub sitemap: Option<String>,
    /// A URL that always bypasses the static cache; `{nonce}` is replaced per run.
    pub fresh_reference: Option<String>,
    #[serde(default)]
    pub layout_fingerprint: Vec<String>,
    #[serde(default)]
    pub quick_pages: Vec<String>,
    #[serde(default = "d_quick_sample")]
    pub quick_sample: usize,
    #[serde(default = "d_quick_rate")]
    pub quick_rate: f64,
    #[serde(default = "d_full_rate")]
    pub full_rate: f64,
    /// Connect to this address instead of resolving the host (TLS/SNI still use the host name).
    pub connect_to: Option<IpAddr>,
    #[serde(default = "d_shrink_ratio")]
    pub shrink_ratio: f64,
    #[serde(default = "d_image_check")]
    pub image_check: ImageCheck,
    #[serde(default)]
    pub page_types: Vec<RawPageType>,
}

fn d_quick_sample() -> usize {
    10
}
fn d_quick_rate() -> f64 {
    2.0
}
fn d_full_rate() -> f64 {
    4.0
}
fn d_shrink_ratio() -> f64 {
    0.6
}
fn d_image_check() -> ImageCheck {
    ImageCheck::Quick
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPageType {
    pub name: String,
    #[serde(rename = "match")]
    pub pattern: String,
    #[serde(default)]
    pub require: Vec<String>,
    #[serde(default)]
    pub forbid_text: Vec<String>,
    pub min_text: Option<usize>,
}

#[derive(Debug)]
pub struct Site {
    pub name: String,
    pub base: url::Url,
    pub manifest: Option<String>,
    pub sitemap: Option<String>,
    pub fresh_reference: Option<String>,
    pub layout_fingerprint: Vec<Selector>,
    pub quick_pages: Vec<String>,
    pub quick_sample: usize,
    pub quick_rate: f64,
    pub full_rate: f64,
    pub connect_to: Option<IpAddr>,
    pub shrink_ratio: f64,
    pub image_check: ImageCheck,
    pub page_types: Vec<PageType>,
}

#[derive(Debug)]
pub struct PageType {
    pub name: String,
    pub pattern: Regex,
    /// Original selector text next to the compiled selector, for messages.
    pub require: Vec<(String, Selector)>,
    pub forbid_text: Vec<String>,
    pub min_text: Option<usize>,
}

impl Site {
    /// Every page type whose pattern matches the path (+ query) applies.
    pub fn types_for<'a>(&'a self, path: &str) -> Vec<&'a PageType> {
        self.page_types.iter().filter(|t| t.pattern.is_match(path)).collect()
    }

    pub fn url(&self, path: &str) -> Result<url::Url> {
        self.base.join(path).with_context(|| format!("joining {path} to {}", self.base))
    }
}

fn selector(s: &str) -> Result<Selector> {
    Selector::parse(s).map_err(|e| anyhow::anyhow!("invalid CSS selector {s:?}: {e}"))
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut cfg: Config = toml::from_str(text)?;
        if cfg.raw_sites.is_empty() {
            bail!("no [[sites]] configured");
        }
        for raw in &cfg.raw_sites {
            cfg.sites.push(compile_site(raw).with_context(|| format!("site {:?}", raw.name))?);
        }
        if cfg.mail.enabled && (cfg.mail.from.is_empty() || cfg.mail.to.is_empty()) {
            bail!("[mail] enabled needs from and to");
        }
        Ok(cfg)
    }
}

fn compile_site(raw: &RawSite) -> Result<Site> {
    let base = url::Url::parse(&raw.base_url).context("base_url")?;
    if base.host_str().is_none() {
        bail!("base_url needs a host");
    }
    if raw.full_rate <= 0.0 || raw.quick_rate <= 0.0 {
        bail!("quick_rate/full_rate must be > 0");
    }
    if !(0.0..1.0).contains(&raw.shrink_ratio) {
        bail!("shrink_ratio must be in [0, 1)");
    }
    let mut page_types = vec![];
    for t in &raw.page_types {
        page_types.push(PageType {
            name: t.name.clone(),
            pattern: Regex::new(&t.pattern).with_context(|| format!("page type {:?}: match", t.name))?,
            require: t
                .require
                .iter()
                .map(|s| Ok((s.clone(), selector(s)?)))
                .collect::<Result<_>>()
                .with_context(|| format!("page type {:?}", t.name))?,
            forbid_text: t.forbid_text.clone(),
            min_text: t.min_text,
        });
    }
    Ok(Site {
        name: raw.name.clone(),
        base,
        manifest: raw.manifest.clone(),
        sitemap: raw.sitemap.clone(),
        fresh_reference: raw.fresh_reference.clone(),
        layout_fingerprint: raw.layout_fingerprint.iter().map(|s| selector(s)).collect::<Result<_>>()?,
        quick_pages: raw.quick_pages.clone(),
        quick_sample: raw.quick_sample,
        quick_rate: raw.quick_rate,
        full_rate: raw.full_rate,
        connect_to: raw.connect_to,
        shrink_ratio: raw.shrink_ratio,
        image_check: raw.image_check,
        page_types,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_selector_and_pattern() {
        let bad_sel = "[[sites]]\nname='a'\nbase_url='https://x.test'\n[[sites.page_types]]\nname='t'\nmatch='.*'\nrequire=['main >']\n";
        assert!(Config::parse(bad_sel).is_err());
        let bad_re = "[[sites]]\nname='a'\nbase_url='https://x.test'\n[[sites.page_types]]\nname='t'\nmatch='('\n";
        assert!(Config::parse(bad_re).is_err());
    }

    #[test]
    fn every_matching_type_applies() {
        let cfg = Config::parse(
            "[[sites]]\nname='a'\nbase_url='https://x.test'\n\
             [[sites.page_types]]\nname='any'\nmatch='.*'\n\
             [[sites.page_types]]\nname='recipe'\nmatch='^/rezepte/[^/?]+$'\n",
        )
        .unwrap();
        let names = |p| cfg.sites[0].types_for(p).iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        assert_eq!(names("/rezepte/x"), ["any", "recipe"]);
        assert_eq!(names("/rezepte"), ["any"]);
    }
}
