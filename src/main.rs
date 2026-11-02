//! Axis Agents — a desktop app that runs a team of autonomous AI agents
//! on Discord, built on the Axis library.
//!
//! Each agent is its own Discord bot with its own persona and model. They
//! run simultaneously, share what they learn about each server, and
//! coordinate in a private HQ channel.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod llm;
mod memory;
mod paths;
mod runtime;
mod tools;
mod ui;

fn main() -> eframe::Result<()> {
    let log_dir = paths::data_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("axis_agents=info,axis=warn"));
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("axis-agents.log"))
    {
        Ok(file) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .try_init();
        }
        Err(_) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .try_init();
        }
    }

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Axis Agents")
            .with_inner_size([1320.0, 860.0])
            .with_min_inner_size([980.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Axis Agents",
        native,
        Box::new(|cc| Ok(Box::new(ui::App::new(cc)))),
    )
}
