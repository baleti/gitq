//! Scrollback: capturing a tmux pane's history, splitting it into
//! command+output entries, and browsing them.
//!
//! Unlike the pipeline language this needs no git repository — it reads the
//! terminal, not git.

pub mod ansi;
pub mod browse;
pub mod capture;
pub mod entry;
pub mod mark;
pub mod render;
