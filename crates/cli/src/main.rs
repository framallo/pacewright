mod auth_cmd;
mod client;
mod oauth_cmd;
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
    /// Start or resume a pipeline run. Idempotent: succeeded steps are never redone.
    Run {
        /// Pipeline name, e.g. `podcast/episode`
        pipeline: String,
        /// Stable id for this run, e.g. `ep172`. Re-use it to resume.
        #[arg(long)]
        run_id: String,
        /// JSON object of pipeline vars
        #[arg(long, default_value = "{}")]
        params: String,
        /// Re-queue this run's FAILED steps before starting
        #[arg(long)]
        retry_failed: bool,
    },
    /// List runs with a rollup of their step statuses.
    Runs,
    /// Show one run's steps, in order.
    Show {
        run_id: String,
    },
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
    /// OAuth token auth for first-party APIs (LinkedIn, YouTube) — login/status/logout.
    Oauth {
        #[command(subcommand)]
        cmd: Option<OauthCmd>,
    },
}

#[derive(Subcommand)]
enum OauthCmd {
    /// Show each provider: app configured? · token valid/expired/absent · scope · author URN.
    Status,
    /// Run the consent flow: opens the browser, catches the loopback redirect, stores the token.
    Login {
        /// Provider to authorize (currently `linkedin`).
        provider: String,
        /// App client id. Falls back to the stored value, then an interactive prompt.
        #[arg(long)]
        client_id: Option<String>,
        /// App client secret (kept 0600, never logged). Falls back to stored, then a hidden prompt.
        #[arg(long)]
        client_secret: Option<String>,
        /// Loopback base for the redirect URI (default `http://localhost:8765`); the URI is
        /// `{base}/{provider}/callback`. Must match what's registered on the app.
        #[arg(long)]
        callback_base: Option<String>,
    },
    /// Drop a provider's tokens (keeps its app credentials for a quick re-login).
    Logout { provider: String },
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
        Cmd::Run { pipeline, run_id, params, retry_failed } => Request::RunStart {
            pipeline,
            run_id,
            params: serde_json::from_str(&params)
                .map_err(|e| anyhow::anyhow!("--params must be a JSON object: {e}"))?,
            retry_failed,
        },
        Cmd::Runs => Request::RunList,
        Cmd::Show { run_id } => Request::RunShow { run_id },
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
        // OAuth is a local flow (browser + secret store) — it never touches the daemon socket.
        Cmd::Oauth { cmd } => {
            let home = pw_dir();
            return match cmd.unwrap_or(OauthCmd::Status) {
                OauthCmd::Status => oauth_cmd::status(home).await,
                OauthCmd::Login {
                    provider,
                    client_id,
                    client_secret,
                    callback_base,
                } => oauth_cmd::login(home, provider, client_id, client_secret, callback_base).await,
                OauthCmd::Logout { provider } => oauth_cmd::logout(home, provider).await,
            };
        }
    };
    let resp = client::call(&sock, req).await?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}
