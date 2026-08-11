//! The MCP tool surface: the `tools/list` catalog and the pure `build_request` mapping from a
//! tool call to a `pacewright_proto::Request`. Kept free of I/O so it's unit-testable without a
//! running daemon.

use pacewright_proto::{AddTaskReq, LimitSpec, Request};
use serde_json::{json, Value};

/// A required string argument, or a descriptive error naming the tool and field.
fn req_str(args: &Value, tool: &str, field: &str) -> Result<String, String> {
    args.get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("{tool}: missing required string argument `{field}`"))
}

fn opt_str(args: &Value, field: &str) -> Option<String> {
    args.get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn opt_i64(args: &Value, field: &str) -> Option<i64> {
    args.get(field).and_then(|v| v.as_i64())
}

fn opt_f64(args: &Value, field: &str) -> Option<f64> {
    args.get(field).and_then(|v| v.as_f64())
}

fn opt_bool(args: &Value, field: &str) -> bool {
    args.get(field).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Map a `tools/call` (name + arguments object) to the daemon request it stands for.
/// Returns `Err` with a human message on an unknown tool or a missing required argument.
pub fn build_request(name: &str, args: &Value) -> Result<Request, String> {
    match name {
        "add_task" => Ok(Request::Add(AddTaskReq {
            adapter: req_str(args, name, "adapter")?,
            action: req_str(args, name, "action")?,
            params: args.get("params").cloned().unwrap_or_else(|| json!({})),
            scheduled_for: opt_i64(args, "at"),
            recurrence: opt_str(args, "every"),
            depends_on: opt_str(args, "depends_on"),
            priority: opt_i64(args, "priority"),
            dedup_key: opt_str(args, "dedup"),
            max_attempts: opt_i64(args, "max_attempts"),
        })),
        "get_task" => Ok(Request::Get {
            id: req_str(args, name, "id")?,
        }),
        "list_tasks" => Ok(Request::List {
            status: opt_str(args, "status"),
            adapter: opt_str(args, "adapter"),
            limit: opt_i64(args, "limit"),
        }),
        "cancel_task" => Ok(Request::Cancel {
            id: req_str(args, name, "id")?,
        }),
        "run_now" => Ok(Request::RunNow {
            id: req_str(args, name, "id")?,
            force: opt_bool(args, "force"),
        }),
        "status" => Ok(Request::Status),
        "list_adapters" => Ok(Request::Adapters),
        "limits" => Ok(Request::Limits),
        "pause" => Ok(Request::Pause {
            scope: req_str(args, name, "scope")?,
        }),
        "resume" => Ok(Request::Resume {
            scope: req_str(args, name, "scope")?,
        }),
        "set_limit" => Ok(Request::SetLimit {
            key: req_str(args, name, "key")?,
            config: LimitSpec {
                daily_cap: opt_i64(args, "daily_cap"),
                min_gap: opt_str(args, "min_gap"),
                jitter: opt_f64(args, "jitter"),
                active: opt_str(args, "active"),
            },
        }),
        "schedule_list" => Ok(Request::ScheduleList),
        "schedule_apply" => Ok(Request::ScheduleApply {
            prune: opt_bool(args, "prune"),
        }),
        "schedule_enable" => Ok(Request::ScheduleEnable {
            id: req_str(args, name, "id")?,
        }),
        "schedule_disable" => Ok(Request::ScheduleDisable {
            id: req_str(args, name, "id")?,
        }),
        "auth_list" => Ok(Request::AuthList),
        "auth_login" => Ok(Request::AuthLogin {
            account: req_str(args, name, "account")?,
        }),
        "auth_login_all" => Ok(Request::AuthLoginAll),
        "auth_recheck" => Ok(Request::AuthRecheck {
            account: opt_str(args, "account"),
        }),
        "recipe_reload" => Ok(Request::RecipeReload),
        "digest" => Ok(Request::Digest),
        "escalations" => Ok(Request::Escalations {
            drain: args.get("drain").and_then(Value::as_bool).unwrap_or(false),
        }),
        "data_list" => Ok(Request::DataList),
        "data_show" => Ok(Request::DataShow {
            name: req_str(args, "data_show", "name")?,
            limit: opt_i64(args, "limit"),
        }),
        "ledger_stats" => Ok(Request::LedgerStats),
        "anthropic_status" => Ok(Request::AnthropicStatus),
        other => Err(format!("unknown tool `{other}`")),
    }
}

/// A minimal JSON-Schema object with the given properties and required list.
fn schema(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
    })
}

fn str_prop(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}
fn int_prop(desc: &str) -> Value {
    json!({ "type": "integer", "description": desc })
}
fn bool_prop(desc: &str) -> Value {
    json!({ "type": "boolean", "description": desc })
}

/// The `tools/list` catalog: every tool's name, human description, and input JSON Schema.
pub fn tool_catalog() -> Value {
    json!([
        {
            "name": "add_task",
            "description": "Enqueue a task for an adapter. `params` is the adapter/recipe's JSON input. Optional: `at` (epoch ms to run at), `every` (cron-like recurrence), `depends_on` (task id gate), `priority`, `dedup` (dedup key), `max_attempts`.",
            "inputSchema": schema(json!({
                "adapter": str_prop("Adapter name, e.g. `globex` or `dummy`."),
                "action": str_prop("Action/recipe name within the adapter, e.g. `list_projects`."),
                "params": json!({ "type": "object", "description": "Adapter-specific JSON params." }),
                "at": int_prop("Epoch ms to first run at (default: now)."),
                "every": str_prop("Recurrence spec (cron-like) for a recurring task."),
                "depends_on": str_prop("Task id that must succeed before this runs."),
                "priority": int_prop("Higher runs first among ready tasks."),
                "dedup": str_prop("Dedup key; a matching active task is returned instead of a new one."),
                "max_attempts": int_prop("Max retry attempts before failing.")
            }), &["adapter", "action"])
        },
        { "name": "get_task", "description": "Fetch one task plus its event history by id.",
          "inputSchema": schema(json!({ "id": str_prop("Task id.") }), &["id"]) },
        { "name": "list_tasks", "description": "List tasks, newest first. Optional filters: `status` (pending|blocked|deferred|running|succeeded|failed|canceled), `adapter`, `limit` (default 100).",
          "inputSchema": schema(json!({ "status": str_prop("Status filter."), "adapter": str_prop("Adapter filter."), "limit": int_prop("Max rows (default 100).") }), &[]) },
        { "name": "cancel_task", "description": "Cancel a non-terminal task by id.",
          "inputSchema": schema(json!({ "id": str_prop("Task id.") }), &["id"]) },
        { "name": "run_now", "description": "Reschedule a task to run immediately (clears its defer). `force` is reserved for future limit-bypass.",
          "inputSchema": schema(json!({ "id": str_prop("Task id."), "force": bool_prop("Reserved.") }), &["id"]) },
        { "name": "status", "description": "Daemon summary: pending/running counts and paused scopes.",
          "inputSchema": schema(json!({}), &[]) },
        { "name": "escalations", "description": "The escalation outbox: issues the notifier raised (terminal failures / auto-paused scopes) for triage. `drain:true` deletes each after reading. Use `get_task`/`resume` to act on them.",
          "inputSchema": schema(json!({ "drain": bool_prop("Delete each escalation after reading.") }), &[]) },
        { "name": "data_list", "description": "List saved JSON datasets (task output) with row counts.",
          "inputSchema": schema(json!({}), &[]) },
        { "name": "data_show", "description": "Print a saved dataset's rows. `limit` caps the count.",
          "inputSchema": schema(json!({ "name": str_prop("Dataset name, e.g. `x/pool-ai`."), "limit": int_prop("Max rows to return.") }), &["name"]) },
        { "name": "ledger_stats", "description": "All-time dedup ledger: touched-target counts per scope (never-act-twice).",
          "inputSchema": schema(json!({}), &[]) },
        { "name": "anthropic_status", "description": "Whether a Claude Max/Pro subscription is signed in and its token freshness.",
          "inputSchema": schema(json!({}), &[]) },
        { "name": "digest", "description": "Today's structured summary: what ran, what's queued, what failed (with errors), and what is waiting on a human (paused scopes + failure count).",
          "inputSchema": schema(json!({}), &[]) },
        { "name": "list_adapters", "description": "List registered adapters and their actions.",
          "inputSchema": schema(json!({}), &[]) },
        { "name": "limits", "description": "Show today's rate-limit counters.",
          "inputSchema": schema(json!({}), &[]) },
        { "name": "pause", "description": "Pause a scope: `all` (or `daemon`) pauses everything; any other value pauses that adapter.",
          "inputSchema": schema(json!({ "scope": str_prop("`all`, `daemon`, or an adapter name.") }), &["scope"]) },
        { "name": "resume", "description": "Resume a previously paused scope.",
          "inputSchema": schema(json!({ "scope": str_prop("`all`, `daemon`, or an adapter name.") }), &["scope"]) },
        { "name": "set_limit", "description": "Set a runtime pacing override for a limit key (persisted). `min_gap`/`active` are human strings like `8m` / `09:00-17:00`.",
          "inputSchema": schema(json!({ "key": str_prop("Limit key, e.g. `globex.publish`."), "daily_cap": int_prop("Max runs per local day."), "min_gap": str_prop("Min gap between runs, e.g. `8m`."), "jitter": json!({ "type": "number", "description": "Fractional jitter 0..1." }), "active": str_prop("Active window, e.g. `09:00-17:00`.") }), &["key"]) },
        { "name": "schedule_list", "description": "Show the declarative schedule catalog (id, recipe, timing, next-fire, enabled, live status).",
          "inputSchema": schema(json!({}), &[]) },
        { "name": "schedule_apply", "description": "Reconcile the schedule files into the queue. `prune` also cancels live tasks for removed entries.",
          "inputSchema": schema(json!({ "prune": bool_prop("Cancel tasks for removed entries.") }), &[]) },
        { "name": "schedule_enable", "description": "Enable a scheduled entry by id and reconcile.",
          "inputSchema": schema(json!({ "id": str_prop("Schedule entry id.") }), &["id"]) },
        { "name": "schedule_disable", "description": "Disable a scheduled entry by id (cancels its live task).",
          "inputSchema": schema(json!({ "id": str_prop("Schedule entry id.") }), &["id"]) },
        { "name": "auth_list", "description": "List account recipes with signed-in/out/unknown status and which recipes use each.",
          "inputSchema": schema(json!({}), &[]) },
        { "name": "auth_login", "description": "Open a headed login window for an account (the daemon pops Chrome; a human signs in).",
          "inputSchema": schema(json!({ "account": str_prop("Account name.") }), &["account"]) },
        { "name": "auth_login_all", "description": "Open a login window, one at a time, for every account not known to be signed in.",
          "inputSchema": schema(json!({}), &[]) },
        { "name": "auth_recheck", "description": "Re-run an account's signed-in check headless and refresh its cached status. Omit `account` to recheck all.",
          "inputSchema": schema(json!({ "account": str_prop("Account name, or omit for all.") }), &[]) },
        { "name": "recipe_reload", "description": "Reload recipes from disk without restarting the daemon; newly added task recipes become runnable.",
          "inputSchema": schema(json!({}), &[]) }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_add_task_defaults_params_and_reads_options() {
        let args = json!({ "adapter": "globex", "action": "list_projects", "at": 123 });
        let req = build_request("add_task", &args).unwrap();
        match req {
            Request::Add(a) => {
                assert_eq!(a.adapter, "globex");
                assert_eq!(a.action, "list_projects");
                assert_eq!(a.params, json!({}));
                assert_eq!(a.scheduled_for, Some(123));
            }
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn test_missing_required_arg_is_an_error() {
        let err = build_request("get_task", &json!({})).unwrap_err();
        assert!(err.contains("id"), "got: {err}");
    }

    #[test]
    fn test_unknown_tool_errors() {
        assert!(build_request("nope", &json!({}))
            .unwrap_err()
            .contains("unknown tool"));
    }

    #[test]
    fn test_set_limit_maps_spec_fields() {
        let args =
            json!({ "key": "globex.publish", "daily_cap": 5, "min_gap": "8m", "jitter": 0.5 });
        match build_request("set_limit", &args).unwrap() {
            Request::SetLimit { key, config } => {
                assert_eq!(key, "globex.publish");
                assert_eq!(config.daily_cap, Some(5));
                assert_eq!(config.min_gap.as_deref(), Some("8m"));
                assert_eq!(config.jitter, Some(0.5));
            }
            _ => panic!("expected SetLimit"),
        }
    }

    #[test]
    fn test_catalog_lists_every_buildable_tool() {
        // Every catalog entry must map to a real request (with a minimal args stub).
        let catalog = tool_catalog();
        for tool in catalog.as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            // Provide the union of possibly-required args; extras are ignored by build_request.
            let stub = json!({
                "adapter": "a", "action": "b", "id": "x", "scope": "all", "key": "k",
                "account": "acct", "name": "d", "pipeline": "p", "run_id": "r"
            });
            assert!(
                build_request(name, &stub).is_ok(),
                "catalog tool `{name}` failed to build"
            );
        }
    }
}
