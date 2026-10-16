//! Terminal UI — ratatui rendering, input handling, overlays, themes, Vim mode.

#![warn(missing_docs)]

pub mod export;
pub mod input;
pub mod message_list;
pub mod notification_center;
pub mod overlay;
pub mod permission_bar;
pub mod search;
pub mod selector;
pub mod state;
pub mod statusline;
pub mod statusline_setup;
pub mod terminal;
pub mod themes;
pub mod vim;

#[cfg(feature = "voice")]
pub mod voice;

#[cfg(feature = "buddy")]
pub mod buddy;
