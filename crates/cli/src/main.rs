mod client;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pacewright_proto::{AddTaskReq, Request};
use std::path::PathBuf;

fn sock_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap()).join(".pacewright").join("pw.sock")
}

#[derive(Parser)]
#[command(name = "pacewright", bin_name = "pacewright")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Add {
        adapter: String,
        action: String,
        #[arg(long, default_value = "{}")] params: String,
        #[arg(long)] at: Option<i64>,
        #[arg(long)] every: Option<String>,
        #[arg(long)] depends_on: Option<String>,
        #[arg(long)] priority: Option<i64>,
        #[arg(long)] dedup: Option<String>,
    },
    List { #[arg(long)] status: Option<String> },
    Get { id: String },
    Cancel { id: String },
    RunNow { id: String, #[arg(long)] force: bool },
    Pause { scope: String },
    Resume { scope: String },
    Limits,
    Adapters,
    Status,
    Tui,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let sock = sock_path();
    let req = match cli.cmd {
        Cmd::Add { adapter, action, params, at, every, depends_on, priority, dedup } => {
            Request::Add(AddTaskReq {
                adapter, action, params: serde_json::from_str(&params)?,
                scheduled_for: at, recurrence: every, depends_on, priority, dedup_key: dedup, max_attempts: None,
            })
        }
        Cmd::List { status } => Request::List { status, adapter: None, limit: Some(100) },
        Cmd::Get { id } => Request::Get { id },
        Cmd::Cancel { id } => Request::Cancel { id },
        Cmd::RunNow { id, force } => Request::RunNow { id, force },
        Cmd::Pause { scope } => Request::Pause { scope },
        Cmd::Resume { scope } => Request::Resume { scope },
        Cmd::Limits => Request::Limits,
        Cmd::Adapters => Request::Adapters,
        Cmd::Status => Request::Status,
        Cmd::Tui => { return tui::run(&sock).await; }
    };
    let resp = client::call(&sock, req).await?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}
