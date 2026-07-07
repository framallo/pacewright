use anyhow::Result;
use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use pacewright_proto::{Request, Response};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Row, Table};
use std::io::stdout;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::client::call;

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
    let mut tasks_json = serde_json::json!({"tasks": []});
    let mut status_json = serde_json::json!({"pending": 0, "running": 0});

    loop {
        if last.elapsed() >= Duration::from_secs(1) {
            if let Ok(Response::Ok(v)) = call(
                sock,
                Request::List { status: None, adapter: None, limit: Some(50) },
            )
            .await
            {
                tasks_json = v;
            }
            if let Ok(Response::Ok(v)) = call(sock, Request::Status).await {
                status_json = v;
            }
            last = Instant::now();
        }

        terminal.draw(|f| {
            let area = f.area();
            let header = format!(
                " pacewright — pending {} · running {}   (q to quit) ",
                status_json["pending"], status_json["running"]
            );
            let rows: Vec<Row> = tasks_json["tasks"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|t| {
                    let id = t["id"].as_str().unwrap_or("");
                    Row::new(vec![
                        Cell::from(id.chars().take(8).collect::<String>()),
                        Cell::from(t["adapter"].as_str().unwrap_or("").to_string()),
                        Cell::from(t["action"].as_str().unwrap_or("").to_string()),
                        Cell::from(t["status"].as_str().unwrap_or("").to_string()),
                        Cell::from(t["attempts"].to_string()),
                    ])
                })
                .collect();
            let widths = [
                Constraint::Length(10),
                Constraint::Length(14),
                Constraint::Length(16),
                Constraint::Length(12),
                Constraint::Length(6),
            ];
            let table = Table::new(rows, widths)
                .header(Row::new(vec!["id", "adapter", "action", "status", "try"]).style(Style::new().bold()))
                .block(Block::default().borders(Borders::ALL).title(header));
            f.render_widget(table, area);
        })?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                if k.code == KeyCode::Char('q') {
                    break;
                }
            }
        }
    }

    Ok(())
}
