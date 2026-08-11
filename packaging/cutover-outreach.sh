#!/bin/bash
# cutover-outreach.sh — flip the outreach fleet from the launchd bash scripts to pacewright.
#
# This is a LIVE cutover: it stops the bash jobs and hands their cadence to the pacewright daemon,
# which then drives real outreach (pitches, comments) on the real accounts at the scheduled slots.
# Run it deliberately, once, when you're ready. It is REVERSIBLE (see --undo).
#
# What it does:
#   1. Verifies the pacewright daemon is running the NEW binary (has `pipeline/start` + `escalations`).
#   2. Reconciles ~/.pacewright/schedules/*.toml into the queue (`schedule apply`).
#   3. Unloads + archives the 6 launchd jobs whose work is now a pacewright schedule.
# It deliberately KEEPS: pacewright-chrome (the attached Chrome), caddy, paperclipai.server,
# linkedin-post-daily (queue reader — not migrated), and x-harvest (CSV crawl — not migrated).
#
# Usage:  packaging/cutover-outreach.sh          # do the cutover
#         packaging/cutover-outreach.sh --undo    # restore the launchd jobs (reverse step 3)
set -euo pipefail

LA="$HOME/Library/LaunchAgents"
ARCHIVE="$LA/retired-by-pacewright"
PW="${PW:-$(command -v pacewright || echo ./target/release/pacewright)}"

# The launchd jobs fully migrated to pacewright schedules/outreach.toml.
RETIRE=(
  com.densitylabs.linkedin-comments-morning
  com.densitylabs.linkedin-comments-afternoon
  com.densitylabs.matchmaker-guesting
  com.densitylabs.x-engage-morning
  com.densitylabs.x-engage-afternoon
  com.ramallo.book-promo-weekly
)

if [ "${1:-}" = "--undo" ]; then
  echo "Restoring retired launchd jobs from $ARCHIVE ..."
  for job in "${RETIRE[@]}"; do
    if [ -f "$ARCHIVE/$job.plist" ]; then
      mv "$ARCHIVE/$job.plist" "$LA/$job.plist"
      launchctl load "$LA/$job.plist" && echo "  reloaded $job"
    fi
  done
  echo "Undo complete. Disable the pacewright schedules if you don't want double execution:"
  echo "  for id in matchmaker-guesting linkedin-comment-morning linkedin-comment-afternoon x-engage-morning x-engage-afternoon book-promo-weekly; do $PW schedule disable \$id; done"
  exit 0
fi

echo "== 1. Verify the pacewright daemon is up and new =="
if ! $PW escalations >/dev/null 2>&1; then
  echo "ERROR: pacewright daemon not reachable, or too old (no 'escalations' RPC)." >&2
  echo "Start the new daemon first:  ./target/release/pacewrightd &" >&2
  exit 1
fi
echo "  ok — daemon reachable with the new RPCs"

echo "== 1b. Verify the Claude Max/Pro subscription is signed in =="
# The claude_cli rounds (LinkedIn/X/book) use the `claude` CLI login; the agent/* steps use this.
# Without it, agent-backed steps fall back to API-key quota — the thing we're avoiding.
if $PW anthropic status 2>/dev/null | grep -q "signed in"; then
  echo "  ok — $($PW anthropic status)"
else
  echo "  Claude subscription NOT signed in. Log in so Claude calls bill your Max/Pro plan, not API quota:"
  echo "    $PW anthropic login"
  read -r -p "  Run 'pcw anthropic login' now? [y/N] " ans
  if [ "$ans" = "y" ] || [ "$ans" = "Y" ]; then
    $PW anthropic login
  else
    echo "  Continuing without a subscription login (agent/* steps may use API quota)."
  fi
fi

echo "== 2. Reconcile the declarative schedules into the queue =="
$PW schedule check
$PW schedule apply

echo "== 3. Unload + archive the migrated launchd jobs =="
mkdir -p "$ARCHIVE"
for job in "${RETIRE[@]}"; do
  plist="$LA/$job.plist"
  if [ -f "$plist" ]; then
    launchctl unload "$plist" 2>/dev/null || true
    mv "$plist" "$ARCHIVE/$job.plist"
    echo "  retired $job"
  else
    echo "  (skip $job — not installed)"
  fi
done

echo
echo "Cutover complete. The outreach fleet now runs on pacewright."
echo "Watch it with:  $PW schedule list ; $PW digest ; $PW escalations"
echo "Reverse with:   packaging/cutover-outreach.sh --undo"
