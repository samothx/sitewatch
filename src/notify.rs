//! Pushes through `ntfy-dispatcher` (events `sitewatch.alert|resolved|heal|digest|error`, mapped
//! to topics by `dist/dispatcher-rules.toml`) and mails the report through opskit's Mailer.

use anyhow::Result;
use opskit::Severity;
use opskit::mail::Mailer;
use opskit::notify::{Message, emit};

use crate::config::Config;
use crate::run::{Event, Issue, Mode, Outcome, State};

fn priority(s: Severity) -> u8 {
    match s {
        Severity::Critical => 5,
        Severity::High => 4,
        Severity::Medium => 3,
        _ => 2,
    }
}

fn issue_body(i: &Issue) -> String {
    let mut b = format!("{} ({} of {} pages)", i.check.describe(), i.count, i.checked);
    for e in i.examples.iter().take(3) {
        b.push_str(&format!("\n• {e}"));
    }
    b
}

/// Messages for this run's events; marks pushed issues as notified in `state`.
pub fn messages(cfg: &Config, out: &Outcome, state: &mut State) -> Vec<Message> {
    let min = cfg.notify.min_severity;
    let mut msgs = vec![];
    for e in &out.events {
        match e {
            Event::New(i) | Event::Escalated(i) if i.severity >= min => {
                let verb = if matches!(e, Event::New(_)) { "" } else { "escalated: " };
                msgs.push(Message {
                    event: "sitewatch.alert",
                    title: format!("{}: {verb}{} ({} pages)", i.site, i.check.id(), i.count),
                    body: issue_body(i),
                    priority: Some(priority(i.severity)),
                });
                if let Some(s) = state.issues.get_mut(&crate::run::issue_key(&i.site, i.check)) {
                    s.notified = true;
                }
            }
            Event::Resolved(i) if i.notified => msgs.push(Message {
                event: "sitewatch.resolved",
                title: format!("{}: resolved {}", i.site, i.check.id()),
                body: format!("{} — no longer seen.", i.check.describe()),
                priority: None,
            }),
            Event::HealRequested { site, issues } => msgs.push(Message {
                event: "sitewatch.heal",
                title: format!("{site}: clearing the static cache"),
                body: format!("Only stale cache references open: {}", issues.join(", ")),
                priority: None,
            }),
            Event::HealFailed { site, issues } => msgs.push(Message {
                event: "sitewatch.alert",
                title: format!("{site}: cache clear did not help"),
                body: format!("Still open after the cache clear: {}", issues.join(", ")),
                priority: Some(4),
            }),
            _ => {}
        }
    }
    if out.mode == Mode::Full && cfg.notify.digest {
        let open = state.issues.len();
        let checked: usize = out.sites.iter().map(|s| s.checked).sum();
        let body = if open == 0 {
            format!("{checked} pages checked, no open issues.")
        } else {
            let mut b = format!("{checked} pages checked, {open} open issue(s):");
            for i in state.issues.values() {
                b.push_str(&format!("\n• {}: {} {} ({} pages)", i.site, i.severity, i.check.id(), i.count));
            }
            b
        };
        msgs.push(Message { event: "sitewatch.digest", title: "nightly check".into(), body, priority: None });
    }
    msgs
}

pub fn push(cfg: &Config, msgs: &[Message]) -> Result<()> {
    for m in msgs {
        emit(&cfg.notify.command, m)?;
    }
    Ok(())
}

/// Mail when something new reached `mail.min_severity`, or after a full run with open issues
/// at that level.
pub fn should_mail(cfg: &Config, out: &Outcome, state: &State) -> bool {
    let min = cfg.mail.min_severity;
    let new = out.events.iter().any(|e| matches!(e, Event::New(i) | Event::Escalated(i) if i.severity >= min) || matches!(e, Event::HealFailed { .. }));
    let open = out.mode == Mode::Full && state.issues.values().any(|i| i.severity >= min);
    cfg.mail.enabled && (new || open)
}

pub fn mail(cfg: &Config, out: &Outcome, state: &State, report: &str) -> Result<()> {
    let m = Mailer {
        app: "sitewatch".into(),
        from: cfg.mail.from.clone(),
        to: cfg.mail.to.clone(),
        sendmail: cfg.mail.sendmail.clone(),
        html: cfg.mail.html,
    };
    let worst = state.issues.values().map(|i| i.severity).max();
    let subject = match worst {
        Some(w) => format!("sitewatch: {} open issue(s), worst {w}", state.issues.len()),
        None => "sitewatch: all clear".into(),
    };
    let sites: Vec<&str> = out.sites.iter().map(|s| s.site.as_str()).collect();
    let intro = format!("sitewatch on {} checked {}.", opskit::hostname(), sites.join(", "));
    m.send(&m.build_message(&subject, &intro, report))
}
