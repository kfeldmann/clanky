//! Clanky — an AI coding agent for the terminal.
//!
//! Library crate so integration tests (golden transcripts, etc.) can drive
//! the same code the binary runs. The binary entry point lives in `main.rs`.

pub mod cli;
pub mod config;
pub mod context;
pub mod error;
pub mod export;
pub mod prompt;
pub mod prompts;
pub mod provider;
pub mod session;
pub mod settings;
pub mod skills;
pub mod tools;
pub mod tui;
pub mod turn;
