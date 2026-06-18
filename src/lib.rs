//! Library interface for claude-watch.
//!
//! Exposes core modules for integration testing.

pub mod active_agents;
pub mod agent;
pub mod cadence;
pub mod cmd;
pub mod config;
pub mod event_bus;
pub mod obligations;
pub mod pr_parse;
pub mod proc_util;
pub mod reminders;
pub mod respawn;
pub mod session_event;
pub mod status;
pub mod task_watch;
pub mod tmux;
pub mod watcher;
pub mod workload;
