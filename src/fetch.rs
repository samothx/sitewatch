//! HTTP client: no cookies, no redirects followed (a redirect is a finding), HTTP status codes
//! returned instead of raised, a pause between requests, and an optional fixed connect address.

use std::cell::Cell;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use ureq::Agent;
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

/// Biggest body read; pages are ~110 KB, images a few MB.
const MAX_BODY: u64 = 32 * 1024 * 1024;

pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub location: Option<String>,
    pub body: Vec<u8>,
    pub elapsed: Duration,
}

impl Response {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub struct Fetcher {
    agent: Agent,
    pause: Duration,
    last: Cell<Option<Instant>>,
}

/// Resolves every host to one address, keeping the URL's port (TLS still uses the URL's host).
#[derive(Debug)]
struct FixedResolver(IpAddr);

impl Resolver for FixedResolver {
    fn resolve(&self, uri: &Uri, _config: &ureq::config::Config, _timeout: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("http") { 80 } else { 443 });
        let mut v = self.empty();
        v.push(SocketAddr::new(self.0, port));
        Ok(v)
    }
}

impl Fetcher {
    /// `rate` = requests per second.
    pub fn new(timeout: Duration, rate: f64, connect_to: Option<IpAddr>) -> Self {
        let config: Config = Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(timeout))
            .user_agent(concat!("sitewatch/", env!("CARGO_PKG_VERSION")))
            .build();
        let agent = match connect_to {
            Some(ip) => Agent::with_parts(config, DefaultConnector::new(), FixedResolver(ip)),
            None => Agent::new_with_config(config),
        };
        Self { agent, pause: Duration::from_secs_f64(1.0 / rate), last: Cell::new(None) }
    }

    pub fn get(&self, url: &str) -> Result<Response> {
        if let Some(last) = self.last.get() {
            let since = last.elapsed();
            if since < self.pause {
                std::thread::sleep(self.pause - since);
            }
        }
        self.last.set(Some(Instant::now()));
        let start = Instant::now();
        let mut resp = self.agent.get(url).call().map_err(|e| anyhow!("{e}"))?;
        let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        let content_type = header("content-type").unwrap_or_default();
        let location = header("location");
        let status = resp.status().as_u16();
        let body = resp.body_mut().with_config().limit(MAX_BODY).read_to_vec().map_err(|e| anyhow!("reading body: {e}"))?;
        Ok(Response { status, content_type, location, body, elapsed: start.elapsed() })
    }
}
