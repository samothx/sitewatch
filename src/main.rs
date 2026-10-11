use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use sitewatch::checks::SiteCtx;
use sitewatch::config::Config;
use sitewatch::fetch::Fetcher;
use sitewatch::run::{self, Mode, Options, State};
use sitewatch::{notify, report};

#[derive(Parser)]
#[command(version, about = "Website watchdog: stale cached pages, missing assets, incomplete renders")]
struct Cli {
    #[arg(long, default_value = "/etc/sitewatch/config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check the configured sites, update the state, push/mail on changes.
    Run {
        #[arg(long, value_enum, default_value = "quick")]
        mode: Mode,
        /// Write nothing (state, report, sitemap cache, heal request), push and mail nothing.
        #[arg(long)]
        dry_run: bool,
        /// Print the report to stdout.
        #[arg(long)]
        print: bool,
        /// Overrides [general] state_dir.
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Check one page of a configured site and print every finding.
    Check { url: String },
    /// Validate the config file.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    Check,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match real_main(&cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sitewatch: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn real_main(cli: &Cli) -> Result<ExitCode> {
    let cfg = Config::load(&cli.config)?;
    match &cli.cmd {
        Cmd::Config { cmd: ConfigCmd::Check } => {
            for s in &cfg.sites {
                println!("{}: {} ({} page types, {} quick pages)", s.name, s.base, s.page_types.len(), s.quick_pages.len());
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Check { url } => check(&cfg, url),
        Cmd::Run { mode, dry_run, print, state_dir } => {
            let opts = Options {
                mode: *mode,
                state_dir: state_dir.clone().unwrap_or_else(|| cfg.general.state_dir.clone()),
                dry_run: *dry_run,
            };
            let result = do_run(&cfg, &opts, *print);
            if let Err(e) = &result
                && cfg.notify.enabled && !opts.dry_run {
                    let m = opskit::notify::Message {
                        event: "sitewatch.error",
                        title: "sitewatch run failed".into(),
                        body: format!("{e:#}"),
                        priority: None,
                    };
                    let _ = opskit::notify::emit(&cfg.notify.command, &m);
                }
            result.map(|_| ExitCode::SUCCESS)
        }
    }
}

fn do_run(cfg: &Config, opts: &Options, print: bool) -> Result<()> {
    let state_file = opts.state_dir.join("state.json");
    let mut state: State = opskit::state::load(&state_file)?;
    let out = run::run(cfg, opts, &mut state)?;
    let msgs = notify::messages(cfg, &out, &mut state);
    let text = report::render(&out, &state);
    if print {
        println!("{text}");
        if opts.dry_run {
            for m in &msgs {
                println!("would push [{}] {}: {}", m.event, m.title, m.body.replace('\n', " / "));
            }
        }
    }
    if opts.dry_run {
        return Ok(());
    }
    let reports = opts.state_dir.join("reports");
    std::fs::create_dir_all(&reports)?;
    std::fs::write(reports.join("latest.md"), &text)?;
    if out.mode == Mode::Full {
        std::fs::write(reports.join("latest-full.md"), &text)?;
    }
    let mut errors = vec![];
    if cfg.notify.enabled
        && let Err(e) = notify::push(cfg, &msgs) {
            errors.push(format!("push: {e:#}"));
        }
    if notify::should_mail(cfg, &out, &state)
        && let Err(e) = notify::mail(cfg, &out, &state, &text) {
            errors.push(format!("mail: {e:#}"));
        }
    // State after delivery attempts, so a failed push is not lost silently: errors fail the unit.
    opskit::state::save(&state_file, &state)?;
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    // Last, so only a clean run pings. A failed ping only logs: the missing ping alerts anyway.
    if cfg.heartbeat.enabled {
        match notify::heartbeat_blocker(cfg, &state) {
            Some(i) => println!("heartbeat skipped: {} {} open ({})", i.severity, i.check.id(), i.site),
            None => match notify::heartbeat(cfg) {
                Ok(()) => println!("heartbeat sent"),
                Err(e) => eprintln!("heartbeat failed: {e:#}"),
            },
        }
    }
    Ok(())
}

fn check(cfg: &Config, url: &str) -> Result<ExitCode> {
    let u = url::Url::parse(url).context("url")?;
    let Some(site) = cfg.sites.iter().find(|s| s.base.host_str() == u.host_str()) else {
        bail!("no configured site for host {:?}", u.host_str().unwrap_or_default());
    };
    let fetcher = Fetcher::new(Duration::from_secs(cfg.general.timeout_secs), site.quick_rate, site.connect_to);
    let ctx = match SiteCtx::prepare(site, fetcher, true, Duration::from_millis(cfg.general.slow_ms)) {
        Ok(c) => c,
        Err(f) => bail!("{}: {} — {}", f.check.id(), f.path, f.detail),
    };
    let mut path = u.path().to_string();
    if let Some(q) = u.query() {
        path = format!("{path}?{q}");
    }
    let types: Vec<&str> = site.types_for(&path).iter().map(|t| t.name.as_str()).collect();
    println!("{path}: page types {}", if types.is_empty() { "-".into() } else { types.join(", ") });
    let (findings, size) = ctx.check_page(&path);
    if let Some(s) = size {
        println!("complete render, {s} bytes");
    }
    if findings.is_empty() {
        println!("no findings");
        return Ok(ExitCode::SUCCESS);
    }
    for f in &findings {
        println!("{:8} {:20} {}{}", f.check.severity().to_string(), f.check.id(), f.detail, if f.healable { " (healable)" } else { "" });
    }
    Ok(ExitCode::from(1))
}
