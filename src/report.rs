//! Markdown report: written to `<state_dir>/reports/latest.md`, printed with `--print`, mailed.

use std::fmt::Write;

use crate::run::{Event, Issue, Mode, Outcome, State};

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Quick => "quick",
        Mode::Full => "full",
    }
}

fn issue_line(i: &Issue) -> String {
    format!(
        "**{}** `{}`: {} ({} of {} pages, since {})",
        i.severity,
        i.check.id(),
        i.check.describe(),
        i.count,
        i.checked,
        i.first_seen.format("%Y-%m-%d %H:%M UTC")
    )
}

pub fn render(out: &Outcome, state: &State) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# sitewatch: {} run, {}\n", mode_name(out.mode), out.started.format("%Y-%m-%d %H:%M UTC"));
    for site in &out.sites {
        let _ = writeln!(s, "## {}", site.site);
        if site.down {
            let _ = writeln!(s, "\nSite unreachable: pages were not checked.\n");
        } else {
            let _ = writeln!(s, "\n{} pages checked.\n", site.checked);
        }
        let mut open: Vec<&Issue> = state.issues.values().filter(|i| i.site == site.site).collect();
        open.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.check.cmp(&b.check)));
        if open.is_empty() {
            let _ = writeln!(s, "No open issues.\n");
        } else {
            let _ = writeln!(s, "### Open issues\n");
            for i in open {
                let _ = writeln!(s, "- {}", issue_line(i));
                for e in &i.examples {
                    let _ = writeln!(s, "  - {e}");
                }
            }
            s.push('\n');
        }
        let changes: Vec<String> = out.events.iter().filter_map(|e| event_line(e, &site.site)).collect();
        if !changes.is_empty() {
            let _ = writeln!(s, "### Changes in this run\n");
            for c in changes {
                let _ = writeln!(s, "- {c}");
            }
            s.push('\n');
        }
    }
    s
}

fn event_line(e: &Event, site: &str) -> Option<String> {
    match e {
        Event::New(i) if i.site == site => Some(format!("new: `{}` ({} pages)", i.check.id(), i.count)),
        Event::Escalated(i) if i.site == site => Some(format!("escalated to {}: `{}`", i.severity, i.check.id())),
        Event::Resolved(i) if i.site == site => Some(format!("resolved: `{}`", i.check.id())),
        Event::HealRequested { site: s, issues } if s == site => Some(format!("cache clear requested ({})", issues.join(", "))),
        Event::HealFailed { site: s, issues } if s == site => Some(format!("cache clear did not help ({})", issues.join(", "))),
        _ => None,
    }
}
