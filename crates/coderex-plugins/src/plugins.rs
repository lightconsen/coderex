//! Plugin manager — loading, handles, marketplace reconciliation.

use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, Ordering};

/// A handle to a loaded plugin.
#[derive(Debug)]
#[allow(dead_code)]
pub struct PluginHandle {
    /// The plugin name.
    pub name: String,
}

/// A handle to a marketplace connection.
#[derive(Debug)]
#[allow(dead_code)]
pub struct MarketplaceHandle {
    /// The marketplace name.
    pub name: String,
}

/// Manages all loaded plugins and marketplace state.
#[derive(Debug)]
#[allow(dead_code)]
pub struct PluginManager {
    plugins: DashMap<String, PluginHandle>,
    marketplaces: DashMap<String, MarketplaceHandle>,
    needs_refresh: AtomicBool,
}

impl Default for PluginManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginManager {
    /// Create a new plugin manager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            plugins: DashMap::new(),
            marketplaces: DashMap::new(),
            needs_refresh: AtomicBool::new(false),
        }
    }

    /// Check if a refresh is needed.
    #[must_use]
    pub fn needs_refresh(&self) -> bool {
        self.needs_refresh.load(Ordering::Relaxed)
    }
}
