//! sitewatch: sees a site as a first-time visitor does and reports stale cached pages, missing
//! assets and incomplete renders on state change. See README.md.

pub mod checks;
pub mod config;
pub mod fetch;
pub mod notify;
pub mod page;
pub mod report;
pub mod run;
