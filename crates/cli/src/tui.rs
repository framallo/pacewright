use anyhow::Result;
use chrono::TimeZone;
use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use pacewright_proto::{Request, Response};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use std::io::stdout;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::client::call;

/// The views the dashboard cycles through with Tab.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Feed,
    Schedule,
    Limits,
    Accounts,
}

impl Pane {
    fn next(self) -> Self {
        match self {
            Pane::Feed => Pane::Schedule,
            Pane::Schedule => Pane::Limits,
            Pane::Limits => Pane::Accounts,
            Pane::Accounts => Pane::Feed,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Pane::Feed => "Feed",
            Pane::Schedule => "Schedule",
            Pane::Limits => "Limits",
            Pane::Accounts => "Accounts",
        }
    }
    /// Whether this pane has a selectable row list (drives ↑/↓ and per-row actions).
    fn is_selectable(self) -> bool {
        matches!(self, Pane::Schedule | Pane::Accounts)
    }
}

/// Format an epoch-ms value in local time, or `-` when absent/unparseable.
fn fmt_ms(ms: Option<i64>) -> String {
    match ms.and_then(|ms| chrono::Local.timestamp_millis_opt(ms).single()) {
        Some(dt) => dt.format("%m-%d %H:%M:%S").to_string(),
        None => "-".to_string(),
    }
}

/// A task's effective run time: a deferred task's `next_eligible_at` wins over `scheduled_for`.
fn fmt_run_at(t: &serde_json::Value) -> String {
    fmt_ms(
        t["next_eligible_at"]
            .as_i64()
            .or_else(|| t["scheduled_for"].as_i64()),
    )
}

pub async fn run(sock: &Path) -> Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let result = run_loop(sock, &mut terminal).await;

    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen)?;

    result
}

async fn run_loop<B: Backend>(sock: &Path, terminal: &mut Terminal<B>) -> Result<()> {
    let mut last = Instant::now() - Duration::from_secs(2);
    let mut tasks_json = serde_json::json!({ "tasks": [] });
    let mut status_json = serde_json::json!({ "pending": 0, "running": 0 });
    let mut sched_json = serde_json::json!({ "schedules": [] });
    let mut limits_json = serde_json::json!({ "counters": [] });
    let mut accounts_json = serde_json::json!({ "accounts": [] });
    let mut pane = Pane::Feed;
    let mut sel: usize = 0;

    loop {
        if last.elapsed() >= Duration::from_secs(1) {
            if let Ok(Response::Ok(v)) = call(
                sock,
                Request::List {
                    status: None,
                    adapter: None,
                    limit: Some(50),
                },
            )
            .await
            {
                tasks_json = v;
            }
            if let Ok(Response::Ok(v)) = call(sock, Request::Status).await {
                status_json = v;
            }
            if let Ok(Response::Ok(v)) = call(sock, Request::ScheduleList).await {
                sched_json = v;
            }
            if let Ok(Response::Ok(v)) = call(sock, Request::Limits).await {
                limits_json = v;
            }
            if let Ok(Response::Ok(v)) = call(sock, Request::AuthList).await {
                accounts_json = v;
            }
            last = Instant::now();
        }

        let sched_len = sched_json["schedules"].as_array().map_or(0, Vec::len);
        let accts_len = accounts_json["accounts"].as_array().map_or(0, Vec::len);
        // Clamp the shared cursor to whichever pane is currently selectable.
        let sel_len = match pane {
            Pane::Accounts => accts_len,
            _ => sched_len,
        };
        if sel >= sel_len.max(1) {
            sel = sel_len.saturating_sub(1);
        }

        terminal.draw(|f| {
            let chunks =
                Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(f.area());
            let header = format!(
                " pacewright — pending {} · running {} · paused {}   [{}] ",
                status_json["pending"],
                status_json["running"],
                status_json["paused"].as_array().map_or(0, Vec::len),
                pane.title(),
            );
            match pane {
                Pane::Feed => draw_feed(f, chunks[0], &tasks_json, &header),
                Pane::Schedule => draw_schedule(f, chunks[0], &sched_json, sel, &header),
                Pane::Limits => draw_limits(f, chunks[0], &limits_json, &header),
                Pane::Accounts => draw_accounts(f, chunks[0], &accounts_json, sel, &header),
            }
            let hint = match pane {
                Pane::Schedule => {
                    " tab: view · ↑/↓: select · space: enable/disable · a: apply · q: quit "
                }
                Pane::Accounts => " tab: view · ↑/↓: select · l: log in · r: recheck · q: quit ",
                _ => " tab: view · q: quit ",
            };
            f.render_widget(Paragraph::new(hint).style(Style::new().dim()), chunks[1]);
        })?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Tab => {
                        pane = pane.next();
                        sel = 0;
                    }
                    KeyCode::Up if pane.is_selectable() => sel = sel.saturating_sub(1),
                    KeyCode::Down if pane.is_selectable() => {
                        if sel + 1 < sel_len {
                            sel += 1;
                        }
                    }
                    KeyCode::Char(' ') if pane == Pane::Schedule => {
                        if let Some(entry) =
                            sched_json["schedules"].as_array().and_then(|a| a.get(sel))
                        {
                            if let Some(id) = entry["id"].as_str() {
                                let enabled = entry["enabled"].as_bool().unwrap_or(true);
                                let req = if enabled {
                                    Request::ScheduleDisable { id: id.to_string() }
                                } else {
                                    Request::ScheduleEnable { id: id.to_string() }
                                };
                                let _ = call(sock, req).await;
                                last = Instant::now() - Duration::from_secs(2); // force refresh
                            }
                        }
                    }
                    KeyCode::Char('a') if pane == Pane::Schedule => {
                        let _ = call(sock, Request::ScheduleApply { prune: false }).await;
                        last = Instant::now() - Duration::from_secs(2);
                    }
                    KeyCode::Char('l') if pane == Pane::Accounts => {
                        if let Some(acct) = account_at(&accounts_json, sel) {
                            let _ = call(sock, Request::AuthLogin { account: acct }).await;
                            last = Instant::now() - Duration::from_secs(2);
                        }
                    }
                    KeyCode::Char('r') if pane == Pane::Accounts => {
                        let acct = account_at(&accounts_json, sel);
                        let _ = call(sock, Request::AuthRecheck { account: acct }).await;
                        last = Instant::now() - Duration::from_secs(2);
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn draw_feed(f: &mut Frame, area: Rect, tasks_json: &serde_json::Value, header: &str) {
    let rows: Vec<Row> = tasks_json["tasks"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|t| {
            Row::new(vec![
                Cell::from(
                    t["id"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(8)
                        .collect::<String>(),
                ),
                Cell::from(t["adapter"].as_str().unwrap_or("").to_string()),
                Cell::from(t["action"].as_str().unwrap_or("").to_string()),
                Cell::from(t["status"].as_str().unwrap_or("").to_string()),
                Cell::from(t["attempts"].to_string()),
                Cell::from(fmt_run_at(t)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(10),
        Constraint::Length(14),
        Constraint::Length(18),
        Constraint::Length(11),
        Constraint::Length(4),
        Constraint::Length(17),
    ];
    let table = Table::new(rows, widths)
        .header(
            Row::new(vec!["id", "adapter", "action", "status", "try", "run at"])
                .style(Style::new().bold()),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(header.to_string()),
        );
    f.render_widget(table, area);
}

fn draw_schedule(
    f: &mut Frame,
    area: Rect,
    sched_json: &serde_json::Value,
    sel: usize,
    header: &str,
) {
    let entries = sched_json["schedules"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let rows: Vec<Row> = entries
        .iter()
        .map(|s| {
            let enabled = s["enabled"].as_bool().unwrap_or(true);
            let when = if let Some(c) = s["every"].as_str() {
                format!("every {c}")
            } else if let Some(at) = s["at"].as_i64() {
                format!("at {}", fmt_ms(Some(at)))
            } else {
                "on apply".to_string()
            };
            let live = s["live_status"].as_str().unwrap_or("—").to_string();
            Row::new(vec![
                Cell::from(if enabled { "◉" } else { "○" }).style(if enabled {
                    Style::new().green()
                } else {
                    Style::new().dim()
                }),
                Cell::from(s["id"].as_str().unwrap_or("").to_string()),
                Cell::from(s["recipe"].as_str().unwrap_or("").to_string()),
                Cell::from(when),
                Cell::from(fmt_ms(s["next_fire"].as_i64())),
                Cell::from(live),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(3),
        Constraint::Length(16),
        Constraint::Length(22),
        Constraint::Length(20),
        Constraint::Length(17),
        Constraint::Length(11),
    ];
    let table = Table::new(rows, widths)
        .header(
            Row::new(vec!["on", "id", "recipe", "when", "next fire", "status"])
                .style(Style::new().bold()),
        )
        .highlight_style(Style::new().reversed())
        .highlight_symbol("▌")
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(header.to_string()),
        );
    let mut state = TableState::default();
    if !entries.is_empty() {
        state.select(Some(sel.min(entries.len() - 1)));
    }
    f.render_stateful_widget(table, area, &mut state);
}

/// The account name of the selected Accounts row, if any (for the `l`/`r` per-row actions).
fn account_at(accounts_json: &serde_json::Value, sel: usize) -> Option<String> {
    accounts_json["accounts"]
        .as_array()
        .and_then(|a| a.get(sel))
        .and_then(|a| a["account"].as_str())
        .map(str::to_string)
}

fn draw_accounts(
    f: &mut Frame,
    area: Rect,
    accounts_json: &serde_json::Value,
    sel: usize,
    header: &str,
) {
    let entries = accounts_json["accounts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let rows: Vec<Row> = entries
        .iter()
        .map(|a| {
            // signed-in/out/unknown/logging-in → a colored dot + label.
            let (dot, label, style) = if a["logging_in"].as_bool() == Some(true) {
                ("◍", "logging in", Style::new().yellow())
            } else {
                match a["signed_in"].as_bool() {
                    Some(true) => ("◉", "signed in", Style::new().green()),
                    Some(false) => ("○", "signed out", Style::new().red()),
                    None => ("?", "unknown", Style::new().dim()),
                }
            };
            let recipes = a["recipes"]
                .as_array()
                .map(|r| {
                    r.iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            Row::new(vec![
                Cell::from(dot).style(style),
                Cell::from(a["account"].as_str().unwrap_or("").to_string()),
                Cell::from(label).style(style),
                Cell::from(fmt_ms(a["last_checked"].as_i64())),
                Cell::from(recipes),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(3),
        Constraint::Length(24),
        Constraint::Length(12),
        Constraint::Length(17),
        Constraint::Min(20),
    ];
    let table = Table::new(rows, widths)
        .header(
            Row::new(vec!["", "account", "session", "last checked", "recipes"])
                .style(Style::new().bold()),
        )
        .highlight_style(Style::new().reversed())
        .highlight_symbol("▌")
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(header.to_string()),
        );
    let mut state = TableState::default();
    if !entries.is_empty() {
        state.select(Some(sel.min(entries.len() - 1)));
    }
    f.render_stateful_widget(table, area, &mut state);
}

fn draw_limits(f: &mut Frame, area: Rect, limits_json: &serde_json::Value, header: &str) {
    let rows: Vec<Row> = limits_json["counters"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|c| {
            Row::new(vec![
                Cell::from(c["key"].as_str().unwrap_or("").to_string()),
                Cell::from(c["count"].to_string()),
                Cell::from(fmt_ms(c["last_spent_at"].as_i64())),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(28),
        Constraint::Length(8),
        Constraint::Length(17),
    ];
    let title = format!("{header}  (spent today — edit caps via `pcw schedule`/config)");
    let table = Table::new(rows, widths)
        .header(Row::new(vec!["limit key", "count", "last spent"]).style(Style::new().bold()))
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(table, area);
}
