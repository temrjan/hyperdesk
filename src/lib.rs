// Modified in hyperdesk from upstream bahmgit/hlcli (Apache-2.0): removed server/operator modules and enforced #![forbid(unsafe_code)].
#![forbid(unsafe_code)]

pub mod catalog;
pub mod command;
pub mod config;
pub mod core;
pub mod exchange;
pub mod execution;
pub mod feed;
pub mod info;
mod info_reads;
pub mod ipc;
pub mod managed;
pub mod metrics;
pub mod planner;
pub mod protocol;
pub mod runtime;
pub mod scope;
pub mod security;
pub mod state;

mod trading_ipc;
