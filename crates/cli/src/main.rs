mod auth_cmd;
mod client;
mod recipe_install;
mod recipe_job;
mod schedule_cmd;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pacewright_adapter_recipe::registry::RecipeRegistry;
use pacewright_proto::{AddTaskReq, Request};
use std::path::PathBuf;

fn pw_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap()).join(".pacewright")
}
fn sock_path() -> PathBuf {
    pw_dir().join("pw.sock")
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
        #[arg(long, default_value = "{}")]
        params: String,
        #[arg(long)]
        at: Option<i64>,
        #[arg(long)]
        every: Option<String>,
        #[arg(long)]
        depends_on: Option<String>,
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        dedup: Option<String>,
    },
    List {
        #[arg(long)]
        status: Option<String>,
    },
    Get {
        id: String,
    },
    Cancel {
        id: String,
    },
    RunNow {
        id: String,
        #[arg(long)]
        force: bool,
    },
    Pause {
        scope: String,
    },
    Resume {
        scope: String,
    },
    Limits,
    Adapters,
    Status,
    Tui,
    /// Manage recipes installed from GitHub repos.
    #[command(subcommand)]
    Recipe(RecipeCmd),
    /// Manage the declarative schedule (recurrent tasks you enable/disable).
    #[command(subcommand)]
    Schedule(ScheduleCmd),
    /// Establish & inspect the logged-in sessions account recipes need.
    Auth {
        #[command(subcommand)]
        cmd: Option<AuthCmd>,
    },
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Show each account: signed-in/out/unknown · recipes using it · last checked.
    Status,
    /// Open a headed login window (the daemon pops Chrome; you sign in by hand).
    Login {
        /// The account to log into. Omit (or pass `--all`) to open every signed-out one.
        account: Option<String>,
        /// Open a login window for every account not already signed in.
        #[arg(long)]
        all: bool,
    },
    /// Force a status refresh (runs each account's check headless). Omit `account` for all.
    Recheck { account: Option<String> },
}

#[derive(Subcommand)]
enum ScheduleCmd {
    /// Validate the schedule files offline (no daemon): ids, recipes, params, cron.
    Check,
    /// Show the schedule catalog: id · recipe · when · next-fire · on/off · live status.
    List,
    /// Reconcile the schedule files into the queue.
    Apply {
        /// Also cancel live tasks whose entries were removed from the files.
        #[arg(long)]
        prune: bool,
    },
    /// Enable a recurrent task (overrides its file default) and reconcile.
    Enable { id: String },
    /// Disable a recurrent task and reconcile (cancels its live task).
    Disable { id: String },
}

#[derive(Subcommand)]
enum RecipeCmd {
    /// Install recipes from a GitHub repo: owner/repo[@ref][#subdir].
    Add { spec: String },
    /// List installed recipes and their source provenance.
    List,
    /// Tell a running daemon to reload recipes from disk (no restart needed).
    Reload,
    /// Enqueue a paced run of the recipe named in a job note's YAML frontmatter.
    Job {
        note: PathBuf,
        #[arg(long)]
        vault: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let sock = sock_path();
    let req = match cli.cmd {
        Cmd::Add {
            adapter,
            action,
            params,
            at,
            every,
            depends_on,
            priority,
            dedup,
        } => Request::Add(AddTaskReq {
            adapter,
            action,
            params: serde_json::from_str(&params)?,
            scheduled_for: at,
            recurrence: every,
            depends_on,
            priority,
            dedup_key: dedup,
            max_attempts: None,
        }),
        Cmd::List { status } => Request::List {
            status,
            adapter: None,
            limit: Some(100),
        },
        Cmd::Get { id } => Request::Get { id },
        Cmd::Cancel { id } => Request::Cancel { id },
        Cmd::RunNow { id, force } => Request::RunNow { id, force },
        Cmd::Pause { scope } => Request::Pause { scope },
        Cmd::Resume { scope } => Request::Resume { scope },
        Cmd::Limits => Request::Limits,
        Cmd::Adapters => Request::Adapters,
        Cmd::Status => Request::Status,
        Cmd::Tui => {
            return tui::run(&sock).await;
        }
        Cmd::Recipe(rc) => match rc {
            // Install is a local filesystem op, but then ping a running daemon to hot-reload so the
            // new recipes are runnable immediately. If no daemon is up, that's fine — it loads them
            // at next boot.
            RecipeCmd::Add { spec } => {
                recipe_install::add(&spec)?;
                match client::call(&sock, Request::RecipeReload).await {
                    Ok(_) => eprintln!("daemon reloaded recipes"),
                    Err(_) => eprintln!(
                        "(no running daemon to reload — it will load these on next start)"
                    ),
                }
                return Ok(());
            }
            RecipeCmd::List => return recipe_install::list(),
            RecipeCmd::Reload => Request::RecipeReload,
            // A job resolves the note -> recipe locally, then enqueues a *paced* task.
            RecipeCmd::Job { note, vault } => {
                let registry = RecipeRegistry::load_dir(&recipe_install::recipes_root()?);
                let job = recipe_job::build_job(&note, &registry, vault.as_deref())?;
                eprintln!(
                    "enqueuing recipe `{}` from {}",
                    job.recipe_name,
                    note.display()
                );
                job.into_add_request()
            }
        },
        Cmd::Schedule(sc) => match sc {
            // `check` is offline (parse + validate locally); the rest are daemon RPCs.
            ScheduleCmd::Check => {
                return schedule_cmd::check(
                    &pw_dir().join("schedules"),
                    &recipe_install::recipes_root()?,
                );
            }
            ScheduleCmd::List => Request::ScheduleList,
            ScheduleCmd::Apply { prune } => Request::ScheduleApply { prune },
            ScheduleCmd::Enable { id } => Request::ScheduleEnable { id },
            ScheduleCmd::Disable { id } => Request::ScheduleDisable { id },
        },
        // Auth commands format their own (table / friendly message) rather than dumping JSON.
        Cmd::Auth { cmd } => {
            return match cmd.unwrap_or(AuthCmd::Status) {
                AuthCmd::Status => auth_cmd::status(&sock).await,
                AuthCmd::Login { account, all } => auth_cmd::login(&sock, account, all).await,
                AuthCmd::Recheck { account } => auth_cmd::recheck(&sock, account).await,
            };
        }
    };
    let resp = client::call(&sock, req).await?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}
