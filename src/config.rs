// SPDX-License-Identifier: GPL-3.0

use cosmic::cosmic_config::{self, cosmic_config_derive::CosmicConfigEntry, CosmicConfigEntry};

/// The sites monitored before the user has added or removed any of their own.
pub const DEFAULT_SITES: &[&str] = &["captainslounge.nl", "draityachts.nl", "dedrait.com"];

/// How often (in seconds) to re-check every site, before the user changes it.
pub const DEFAULT_CHECK_INTERVAL_SECS: u64 = 300;

#[derive(Debug, Clone, CosmicConfigEntry, Eq, PartialEq)]
#[version = 1]
pub struct Config {
    pub sites: Vec<String>,
    pub check_interval_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sites: DEFAULT_SITES.iter().map(|s| s.to_string()).collect(),
            check_interval_secs: DEFAULT_CHECK_INTERVAL_SECS,
        }
    }
}
