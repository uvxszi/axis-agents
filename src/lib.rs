//! Axis Agents library surface: the runtime, tools, memory and config
//! behind the desktop app — exposed so the simulation harness and tests
//! exercise the real pipeline instead of a copy.

pub mod config;
pub mod llm;
pub mod memory;
pub mod paths;
pub mod runtime;
pub mod tools;
pub mod ui;
