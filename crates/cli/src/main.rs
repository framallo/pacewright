mod client;
mod recipe_install;
mod recipe_job;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pacewright_adapter_recipe::registry::RecipeRegistry;
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
    /// Manage recipes installed from GitHub repos.
    #[command(subcommand)]
    Recipe(RecipeCmd),
}

#[derive(Subcommand)]
enum RecipeCmd {
    /// Install recipes from a GitHub repo: owner/repo[@ref][#subdir].
    Add { spec: String },
    /// List installed recipes and their source provenance.
    List,
    /// Enqueue a paced run of the recipe named in a job note's YAML frontmatter.
    Job { note: PathBuf, #[arg(long)] vault: Option<PathBuf> },
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
        Cmd::Recipe(rc) => match rc {
            // Install/list are local filesystem ops — no daemon round-trip.
            RecipeCmd::Add { spec } => return recipe_install::add(&spec),
            RecipeCmd::List => return recipe_install::list(),
            // A job resolves the note -> recipe locally, then enqueues a *paced* task.
            RecipeCmd::Job { note, vault } => {
                let registry = RecipeRegistry::load_dir(&recipe_install::recipes_root()?);
                let job = recipe_job::build_job(&note, &registry, vault.as_deref())?;
                eprintln!("enqueuing recipe `{}` from {}", job.recipe_name, note.display());
                job.into_add_request()
            }
        },
    };
    let resp = client::call(&sock, req).await?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}
