//! App-wide settings: web search, MCP servers, HQ, limits, appearance.

use egui::{RichText, TextEdit, Ui};

use super::App;
use super::theme::{self, FAINT, TEXT, WEAK};
use super::widgets::{self, card, form_row, pill, section, toggle_row};
use crate::config::{AccessLevel, McpServerConfig, McpTransport, SearchProvider};

pub fn show(app: &mut App, ui: &mut Ui) {
    widgets::page_header(ui, "Settings", "Shared by every agent.");
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let mut settings = app.config.settings.clone();
        card(ui, |ui| {
            section(ui, "🌐 Web", "How agents search and read the web.");
            form_row(ui, "Search provider", "", |ui| {
                egui::ComboBox::from_id_salt("search_provider")
                    .selected_text(settings.search.provider.label())
                    .width(260.0)
                    .show_ui(ui, |ui| {
                        for provider in SearchProvider::ALL {
                            ui.selectable_value(&mut settings.search.provider, provider, provider.label());
                        }
                    });
            });
            match settings.search.provider {
                SearchProvider::Brave | SearchProvider::Tavily => {
                    form_row(ui, "API key", "", |ui| {
                        ui.add(TextEdit::singleline(&mut settings.search.api_key).password(true).desired_width(360.0));
                    });
                }
                SearchProvider::SearXng => {
                    form_row(ui, "SearXNG URL", "Enable the json format in its settings.yml.", |ui| {
                        ui.add(TextEdit::singleline(&mut settings.search.searxng_url).desired_width(360.0));
                    });
                }
                SearchProvider::DuckDuckGo => {}
            }
            form_row(ui, "Results per search", "", |ui| {
                ui.add(egui::DragValue::new(&mut settings.search.max_results).range(1..=10));
            });
            form_row(
                ui,
                "GitHub token",
                "Optional, for watching repos: raises GitHub's limit from 60 to 5000 checks an hour and allows private repos. A fine-grained token with read-only access is enough.",
                |ui| {
                    ui.add(TextEdit::singleline(&mut settings.github_token).password(true).hint_text("not needed for a few public repos").desired_width(360.0));
                },
            );
            toggle_row(
                ui,
                &mut settings.allow_private_fetch,
                "Let admins' requests read private network pages",
                "Off: agents refuse localhost/LAN URLs (protects your router, NAS, LM Studio…). On: only for server admins.",
            );
        });
        ui.add_space(8.0);


        card(ui, |ui| {
            section(ui, "🤝 Team", "");
            form_row(ui, "HQ category", "", |ui| {
                ui.add(TextEdit::singleline(&mut settings.hq_category).desired_width(240.0));
            });
            form_row(ui, "HQ channel", "The private channel agents look for (or ask admins to create).", |ui| {
                ui.add(TextEdit::singleline(&mut settings.hq_channel).desired_width(240.0));
            });
            form_row(ui, "Agent loop guard", "Agents stop replying to each other in public after this many messages in a row with no human.", |ui| {
                ui.add(egui::DragValue::new(&mut settings.max_agent_chain).range(1..=20).suffix(" messages"));
            });
            form_row(ui, "Parallel requests per model server", "Shared by agents on the same server — local LM Studio handles 1-2 at a time.", |ui| {
                ui.add(egui::DragValue::new(&mut settings.endpoint_parallelism).range(1..=32));
            });
        });
        ui.add_space(8.0);

        card(ui, |ui| {
            section(ui, "🔌 MCP servers", "Give agents extra tools via the Model Context Protocol (stdio programs like `npx -y @modelcontextprotocol/server-filesystem C:\\notes`, or remote HTTP servers). MCP tools can touch your computer, so choose who may trigger them.");
            let statuses = app.sup.live.mcp.lock().expect("live poisoned").clone();
            let mut remove = None;
            for (index, server) in settings.mcp_servers.iter_mut().enumerate() {
                let status = statuses.get(&server.id).cloned().unwrap_or_default();
                let open = app.selected_mcp.as_deref() == Some(server.id.as_str());
                ui.horizontal(|ui| {
                    widgets::switch(ui, &mut server.enabled);
                    ui.label(RichText::new(&server.name).strong().color(TEXT));
                    if !server.enabled {
                        pill(ui, "disabled", FAINT);
                    } else if status.connected {
                        pill(ui, &format!("connected · {} tools", status.tools.len()), theme::GREEN);
                    } else if let Some(error) = &status.error {
                        pill(ui, &crate::llm::truncate(error, 60), theme::RED);
                    } else {
                        pill(ui, "connecting…", theme::YELLOW);
                    }
                    pill(ui, server.access.label(), theme::BLUE);
                    if ui.small_button(if open { "close" } else { "edit" }).clicked() {
                        app.selected_mcp = if open { None } else { Some(server.id.clone()) };
                    }
                    if ui.small_button("remove").clicked() {
                        remove = Some(index);
                    }
                });
                if open {
                    egui::Frame::new().inner_margin(egui::Margin::symmetric(24, 4)).show(ui, |ui| {
                        form_row(ui, "Name", "", |ui| {
                            ui.add(TextEdit::singleline(&mut server.name).desired_width(240.0));
                        });
                        form_row(ui, "Transport", "", |ui| {
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut server.transport, McpTransport::Stdio, "Local program (stdio)");
                                ui.selectable_value(&mut server.transport, McpTransport::Http, "Remote (HTTP)");
                            });
                        });
                        match server.transport {
                            McpTransport::Stdio => {
                                form_row(ui, "Command", "e.g. npx, uvx, python, or a path to an .exe", |ui| {
                                    ui.add(TextEdit::singleline(&mut server.command).desired_width(360.0));
                                });
                                form_row(ui, "Arguments", "One per line.", |ui| {
                                    let mut args = server.args.join("\n");
                                    if ui.add(TextEdit::multiline(&mut args).desired_rows(3).desired_width(360.0)).changed() {
                                        server.args = args.lines().map(str::to_owned).filter(|a| !a.trim().is_empty()).collect();
                                    }
                                });
                                form_row(ui, "Environment", "KEY=value, one per line.", |ui| {
                                    let mut env = server.env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n");
                                    if ui.add(TextEdit::multiline(&mut env).desired_rows(2).desired_width(360.0)).changed() {
                                        server.env = env
                                            .lines()
                                            .filter_map(|line| line.split_once('=').map(|(k, v)| (k.trim().to_owned(), v.to_owned())))
                                            .collect();
                                    }
                                });
                            }
                            McpTransport::Http => {
                                form_row(ui, "URL", "", |ui| {
                                    ui.add(TextEdit::singleline(&mut server.url).desired_width(360.0));
                                });
                                form_row(ui, "Headers", "Name: value, one per line (e.g. Authorization).", |ui| {
                                    let mut headers = server.headers.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n");
                                    if ui.add(TextEdit::multiline(&mut headers).desired_rows(2).desired_width(360.0)).changed() {
                                        server.headers = headers
                                            .lines()
                                            .filter_map(|line| line.split_once(':').map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())))
                                            .collect();
                                    }
                                });
                            }
                        }
                        form_row(ui, "Who may use it", "", |ui| {
                            egui::ComboBox::from_id_salt(("mcp_access", &server.id))
                                .selected_text(server.access.label())
                                .show_ui(ui, |ui| {
                                    for level in AccessLevel::ALL {
                                        ui.selectable_value(&mut server.access, level, level.label());
                                    }
                                });
                        });
                        if !status.tools.is_empty() {
                            ui.label(RichText::new(format!("Tools: {}", status.tools.join(", "))).size(12.0).color(WEAK));
                        }
                    });
                }
                ui.separator();
            }
            if let Some(index) = remove {
                settings.mcp_servers.remove(index);
            }
            ui.horizontal(|ui| {
                if ui.button("➕ Add MCP server").clicked() {
                    let server = McpServerConfig::default();
                    app.selected_mcp = Some(server.id.clone());
                    settings.mcp_servers.push(server);
                }
                if ui.button("🔄 Reconnect all").clicked() {
                    app.sup.sync_mcp(settings.clone());
                }
            });
        });
        ui.add_space(8.0);

        card(ui, |ui| {
            section(ui, "Appearance & data", "");
            form_row(ui, "Interface scale", "", |ui| {
                if ui.add(egui::Slider::new(&mut settings.ui_scale, 0.75..=1.75)).drag_stopped() {
                    ui.ctx().set_zoom_factor(settings.ui_scale);
                }
            });
            let dir = crate::paths::data_dir();
            form_row(ui, "Data folder", "Config (with tokens and keys), memories, knowledge.", |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(dir.display().to_string()).monospace().color(WEAK));
                    if ui.small_button("open").clicked() {
                        let _ = std::fs::create_dir_all(&dir);
                        widgets::open_external(&dir.display().to_string());
                    }
                });
            });
            egui::CollapsingHeader::new("⚙ Advanced — behavior knobs")
                .default_open(false)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(
                            "Every operational number lives here — nothing is buried in code. Defaults match Discord's limits and the behavior verified against live small models; change only what you mean to."
                        )
                        .size(12.0)
                        .color(WEAK),
                    );
                    let tun = &mut settings.tunables;
                    let mb = |bytes: usize| format!("{} MB", bytes / 1024 / 1024);
                    form_row(ui, "Upload limit", "One file upload (Discord's bot cap is 25 MB).", |ui| {
                        ui.label(RichText::new(mb(tun.upload_limit_bytes)).strong());
                        if ui.small_button("reset 25 MB").clicked() {
                            tun.upload_limit_bytes = 25 * 1024 * 1024;
                        }
                    });
                    form_row(ui, "Attach headroom", "Kept under the upload limit because Discord caps the WHOLE message (all files + text). A '25 MB' ask fills to the largest total that actually sends.", |ui| {
                        ui.add(
                            egui::Slider::new(&mut tun.attach_headroom_bytes, 16 * 1024..=2048 * 1024usize)
                                .custom_formatter(|v, _| format!("{} KB", (v as usize) / 1024)),
                        );
                    });
                    form_row(ui, "Sandbox per channel", "Total file storage per channel before pruning.", |ui| {
                        ui.add(egui::Slider::new(&mut tun.sandbox_limit_bytes, 16..=1024u64 * 1024 * 1024).custom_formatter(|v, _| format!("{} MB", (v as u64) / 1024 / 1024)));
                    });
                    form_row(ui, "Split parts", "Most parts an oversized duplicate becomes.", |ui| {
                        ui.add(egui::Slider::new(&mut tun.max_duplicate_parts, 1..=10));
                    });
                    form_row(ui, "Auto-fill after rewrites", "Rewrites of a still-short file before the app fills it itself.", |ui| {
                        ui.add(egui::Slider::new(&mut tun.file_rewrite_autofill, 1..=5));
                    });
                    form_row(ui, "Size nudges", "'Not finished' pushes before the app fills the file.", |ui| {
                        ui.add(egui::Slider::new(&mut tun.size_nudge_limit, 1..=6));
                    });
                    form_row(ui, "Edit interval", "Minimum ms between status-message edits.", |ui| {
                        ui.add(egui::Slider::new(&mut tun.edit_min_interval_ms, 500..=5000));
                    });
                    form_row(ui, "Temp file lifetime", "Hours before _tmp scratch files are removed.", |ui| {
                        ui.add(egui::Slider::new(&mut tun.temp_file_lifetime_hours, 1..=168));
                    });
                    form_row(ui, "Script timeout", "run_script execution cap, seconds.", |ui| {
                        ui.add(egui::Slider::new(&mut tun.script_timeout_secs, 5..=120));
                    });
                    if ui.small_button("restore all defaults").clicked() {
                        *tun = crate::config::Tunables::default();
                    }
                });
            egui::CollapsingHeader::new("🧰 Tool emojis")
                .default_open(false)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(
                            "One emoji per tool — shown as a line at the top of the status message and the final answer (once per tool). Paste custom emotes like :thonk: or a Discord emote string; anything you type is sent verbatim."
                        )
                        .size(12.0)
                        .color(WEAK),
                    );
                    let names: Vec<String> = settings.tool_emojis.keys().cloned().collect();
                    for name in names {
                        let emoji = settings.tool_emojis.get_mut(&name).unwrap();
                        form_row(ui, &name, "", |ui| {
                            ui.add(egui::TextEdit::singleline(emoji).desired_width(120.0));
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.label("add tool:");
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut app.emoji_new_tool).desired_width(160.0),
                        );
                        if resp.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            && !app.emoji_new_tool.trim().is_empty()
                        {
                            settings
                                .tool_emojis
                                .insert(app.emoji_new_tool.trim().to_owned(), "🔧".to_owned());
                            app.emoji_new_tool.clear();
                        }
                    });
                    if ui.small_button("restore default emojis").clicked() {
                        settings.tool_emojis = crate::config::default_tool_emojis();
                    }
                });
        });

        if settings != app.config.settings {
            app.config.settings = settings;
            app.mark_settings_dirty();
        }
    });
}
