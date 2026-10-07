//! One run: check every site, fold findings into one issue per (site, check), compare with the
//! state of the previous run and collect the transitions worth telling someone about.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use opskit::Severity;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::checks::{Check, Finding, SiteCtx};
use crate::config::{Config, ImageCheck, Site};
use crate::fetch::Fetcher;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Fixed pages, re-checks of open issues and a random sitemap sample.
    Quick,
    /// Every sitemap URL.
    Full,
}

/// Paths of an open issue that every run checks again, so it can resolve.
const RECHECK_PER_ISSUE: usize = 20;
const EXAMPLES: usize = 5;
const SIZE_SAMPLES: usize = 7;
/// Sitemap cache age for quick runs (full runs always fetch it).
const SITEMAP_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub issues: BTreeMap<String, Issue>,
    /// Recent complete-page sizes of the quick pages, `<site> <path>` → bytes.
    #[serde(default)]
    pub sizes: BTreeMap<String, Vec<usize>>,
    #[serde(default)]
    pub heal: BTreeMap<String, HealState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub site: String,
    pub check: Check,
    pub severity: Severity,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub notified: bool,
    pub count: usize,
    pub checked: usize,
    pub paths: Vec<String>,
    pub examples: Vec<String>,
    pub healable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealState {
    pub requested: DateTime<Utc>,
    #[serde(default)]
    pub failure_reported: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    New(Issue),
    Escalated(Issue),
    Resolved(Issue),
    HealRequested { site: String, issues: Vec<String> },
    HealFailed { site: String, issues: Vec<String> },
}

pub struct SiteResult {
    pub site: String,
    pub checked: usize,
    pub down: bool,
}

pub struct Outcome {
    pub mode: Mode,
    pub started: DateTime<Utc>,
    pub sites: Vec<SiteResult>,
    pub events: Vec<Event>,
}

pub struct Options {
    pub mode: Mode,
    pub state_dir: PathBuf,
    /// No state, sitemap cache or heal request is written.
    pub dry_run: bool,
}

pub fn issue_key(site: &str, check: Check) -> String {
    format!("{site}/{}", check.id())
}

pub fn run(cfg: &Config, opts: &Options, state: &mut State) -> Result<Outcome> {
    let started = Utc::now();
    let mut out = Outcome { mode: opts.mode, started, sites: vec![], events: vec![] };
    for site in &cfg.sites {
        let rate = match opts.mode {
            Mode::Quick => site.quick_rate,
            Mode::Full => site.full_rate,
        };
        let fetcher = Fetcher::new(Duration::from_secs(cfg.general.timeout_secs), rate, site.connect_to);
        let images = match site.image_check {
            ImageCheck::None => false,
            ImageCheck::Quick => opts.mode == Mode::Quick,
            ImageCheck::All => true,
        };
        let slow = Duration::from_millis(cfg.general.slow_ms);
        let mut findings = vec![];
        let mut checked = 0;
        let down = match SiteCtx::prepare(site, fetcher, images, slow) {
            Err(f) => {
                findings.push(f);
                true
            }
            Ok(ctx) => {
                for path in select_paths(site, opts, state, &ctx, &mut findings) {
                    let (f, size) = ctx.check_page(&path);
                    checked += 1;
                    findings.extend(f);
                    if let Some(size) = size
                        && site.quick_pages.contains(&path) {
                            findings.extend(size_check(site, &path, size, state));
                        }
                }
                false
            }
        };
        fold(site, checked, down, findings, started, state, &mut out.events);
        if cfg.heal.enabled && !down {
            heal_decision(site, cfg.heal.cooldown_hours, started, state, opts, &mut out.events)?;
        }
        out.sites.push(SiteResult { site: site.name.clone(), checked, down });
    }
    Ok(out)
}

fn select_paths(site: &Site, opts: &Options, state: &State, ctx: &SiteCtx, findings: &mut Vec<Finding>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut paths = vec![];
    let mut add = |p: String, paths: &mut Vec<String>| {
        if seen.insert(p.clone()) {
            paths.push(p);
        }
    };
    for p in &site.quick_pages {
        add(p.clone(), &mut paths);
    }
    for issue in state.issues.values().filter(|i| i.site == site.name && i.check != Check::SiteDown) {
        for p in issue.paths.iter().take(RECHECK_PER_ISSUE) {
            add(p.clone(), &mut paths);
        }
    }
    let Some(sitemap) = &site.sitemap else { return paths };
    let urls = match sitemap_paths(site, sitemap, opts, ctx) {
        Ok(u) => u,
        Err(e) => {
            findings.push(Finding { check: Check::PageStatus, path: sitemap.clone(), detail: format!("sitemap: {e:#}"), healable: false });
            return paths;
        }
    };
    match opts.mode {
        Mode::Full => urls.into_iter().for_each(|p| add(p, &mut paths)),
        Mode::Quick => {
            let mut pool: Vec<String> = urls.into_iter().filter(|p| !paths.contains(p)).collect();
            fastrand::shuffle(&mut pool);
            pool.into_iter().take(site.quick_sample).for_each(|p| add(p, &mut paths));
        }
    }
    paths
}

fn sitemap_cache(opts: &Options, site: &Site) -> PathBuf {
    opts.state_dir.join(format!("sitemap-{}.txt", site.name))
}

/// Same-host paths (+ query) of the sitemap; follows one level of sitemap index.
fn sitemap_paths(site: &Site, sitemap: &str, opts: &Options, ctx: &SiteCtx) -> Result<Vec<String>> {
    let cache = sitemap_cache(opts, site);
    if opts.mode == Mode::Quick {
        let fresh = std::fs::metadata(&cache).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age < SITEMAP_MAX_AGE);
        if fresh {
            let text = std::fs::read_to_string(&cache)?;
            return Ok(text.lines().map(str::to_string).collect());
        }
    }
    let loc = Regex::new(r"<loc>\s*([^<\s]+)\s*</loc>").expect("static regex");
    let fetch = |path: &str| -> Result<String> {
        let url = site.url(path)?;
        let r = ctx.fetcher.get(url.as_str())?;
        anyhow::ensure!(r.status == 200, "HTTP {} for {path}", r.status);
        Ok(r.text())
    };
    let mut paths = vec![];
    let mut nested = vec![];
    for cap in loc.captures_iter(&fetch(sitemap)?) {
        nested.push(cap[1].to_string());
    }
    let mut locs = vec![];
    for u in nested {
        if u.ends_with(".xml") && u.contains("sitemap") {
            let text = fetch(&u)?;
            locs.extend(loc.captures_iter(&text).map(|c| c[1].to_string()));
        } else {
            locs.push(u);
        }
    }
    for l in locs {
        let Ok(u) = site.base.join(&l.replace("&amp;", "&")) else { continue };
        if u.host_str() == site.base.host_str() {
            let mut p = u.path().to_string();
            if let Some(q) = u.query() {
                p = format!("{p}?{q}");
            }
            paths.push(p);
        }
    }
    anyhow::ensure!(!paths.is_empty(), "sitemap lists no pages of this host");
    if !opts.dry_run {
        if let Some(dir) = cache.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&cache, paths.join("\n")).with_context(|| format!("writing {}", cache.display()))?;
    }
    Ok(paths)
}

/// `page.shrunk` against the median of the last complete renders (needs 3 samples).
fn size_check(site: &Site, path: &str, size: usize, state: &mut State) -> Option<Finding> {
    let samples = state.sizes.entry(format!("{} {path}", site.name)).or_default();
    if samples.len() >= 3 {
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        let median = sorted[sorted.len() / 2];
        if (size as f64) < site.shrink_ratio * median as f64 {
            // Not recorded: a broken render must not become the new normal.
            return Some(Finding {
                check: Check::PageShrunk,
                path: path.to_string(),
                detail: format!("{size} bytes, usually ~{median}"),
                healable: false,
            });
        }
    }
    samples.push(size);
    if samples.len() > SIZE_SAMPLES {
        samples.remove(0);
    }
    None
}

/// Findings → one issue per check; transitions against the stored issues of this site.
fn fold(site: &Site, checked: usize, down: bool, findings: Vec<Finding>, now: DateTime<Utc>, state: &mut State, events: &mut Vec<Event>) {
    let mut current: BTreeMap<Check, Issue> = BTreeMap::new();
    for f in findings {
        let issue = current.entry(f.check).or_insert_with(|| Issue {
            site: site.name.clone(),
            check: f.check,
            severity: f.check.severity(),
            first_seen: now,
            last_seen: now,
            notified: false,
            count: 0,
            checked,
            paths: vec![],
            examples: vec![],
            healable: true,
        });
        issue.healable &= f.healable;
        if !issue.paths.contains(&f.path) {
            issue.count += 1;
            if issue.paths.len() < RECHECK_PER_ISSUE {
                issue.paths.push(f.path.clone());
            }
        }
        if issue.examples.len() < EXAMPLES {
            issue.examples.push(format!("{} — {}", f.path, f.detail));
        }
    }
    for (check, mut issue) in current.clone() {
        let key = issue_key(&site.name, check);
        match state.issues.get(&key) {
            Some(old) => {
                issue.first_seen = old.first_seen;
                issue.notified = old.notified;
                let escalated = issue.severity > old.severity;
                state.issues.insert(key, issue.clone());
                if escalated {
                    events.push(Event::Escalated(issue));
                }
            }
            None => {
                state.issues.insert(key, issue.clone());
                events.push(Event::New(issue));
            }
        }
    }
    // A site that was down this run could not be checked: keep its other issues open.
    let stale: Vec<String> = state
        .issues
        .iter()
        .filter(|(_, i)| i.site == site.name && !current.contains_key(&i.check) && (!down || i.check == Check::SiteDown))
        .map(|(k, _)| k.clone())
        .collect();
    for key in stale {
        if let Some(issue) = state.issues.remove(&key) {
            events.push(Event::Resolved(issue));
        }
    }
}

fn heal_request_path(state_dir: &Path, site: &str) -> PathBuf {
    state_dir.join("heal").join(format!("{site}.request"))
}

fn heal_decision(site: &Site, cooldown_hours: i64, now: DateTime<Utc>, state: &mut State, opts: &Options, events: &mut Vec<Event>) -> Result<()> {
    let open: Vec<&Issue> = state.issues.values().filter(|i| i.site == site.name).collect();
    if open.is_empty() || !open.iter().all(|i| i.healable) {
        return Ok(());
    }
    let names: Vec<String> = open.iter().map(|i| format!("{} ({} pages)", i.check.id(), i.count)).collect();
    let cooldown = chrono::Duration::hours(cooldown_hours);
    match state.heal.get_mut(&site.name) {
        Some(h) if now - h.requested < cooldown => {
            // The heal unit had time to clear the cache: still stale means it did not help.
            if now - h.requested > chrono::Duration::minutes(15) && !h.failure_reported {
                h.failure_reported = true;
                events.push(Event::HealFailed { site: site.name.clone(), issues: names });
            }
        }
        _ => {
            if !opts.dry_run {
                let p = heal_request_path(&opts.state_dir, &site.name);
                std::fs::create_dir_all(p.parent().expect("has parent"))?;
                std::fs::write(&p, format!("{now}\n{}\n", names.join("\n"))).with_context(|| format!("writing {}", p.display()))?;
            }
            state.heal.insert(site.name.clone(), HealState { requested: now, failure_reported: false });
            events.push(Event::HealRequested { site: site.name.clone(), issues: names });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site() -> Site {
        crate::config::Config::parse("[[sites]]\nname='s'\nbase_url='https://x.test'\nquick_pages=['/']\n").unwrap().sites.remove(0)
    }

    fn f(check: Check, path: &str) -> Finding {
        Finding { check, path: path.into(), detail: "d".into(), healable: check == Check::StaleBuild }
    }

    #[test]
    fn transitions_new_same_resolved() {
        let s = site();
        let mut st = State::default();
        let mut ev = vec![];
        fold(&s, 10, false, vec![f(Check::StaleBuild, "/"), f(Check::StaleBuild, "/a")], Utc::now(), &mut st, &mut ev);
        assert!(matches!(&ev[..], [Event::New(i)] if i.count == 2 && i.healable));
        ev.clear();
        fold(&s, 10, false, vec![f(Check::StaleBuild, "/")], Utc::now(), &mut st, &mut ev);
        assert!(ev.is_empty(), "same issue again → no event");
        fold(&s, 10, false, vec![], Utc::now(), &mut st, &mut ev);
        assert!(matches!(&ev[..], [Event::Resolved(_)]));
        assert!(st.issues.is_empty());
    }

    #[test]
    fn down_site_keeps_other_issues() {
        let s = site();
        let mut st = State::default();
        let mut ev = vec![];
        fold(&s, 10, false, vec![f(Check::PageIncomplete, "/")], Utc::now(), &mut st, &mut ev);
        ev.clear();
        fold(&s, 0, true, vec![f(Check::SiteDown, "/build/manifest.json")], Utc::now(), &mut st, &mut ev);
        assert!(matches!(&ev[..], [Event::New(i)] if i.check == Check::SiteDown));
        assert_eq!(st.issues.len(), 2);
    }

    #[test]
    fn shrink_needs_baseline_and_is_not_recorded() {
        let s = site();
        let mut st = State::default();
        for _ in 0..3 {
            assert!(size_check(&s, "/", 100_000, &mut st).is_none());
        }
        assert!(size_check(&s, "/", 30_000, &mut st).is_some());
        assert_eq!(st.sizes["s /"].len(), 3);
        assert!(size_check(&s, "/", 95_000, &mut st).is_none());
    }
}
