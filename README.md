# pacewright

Queues, schedules (daily caps + human pacing), runs, and tracks browser-automation
tasks. M1 is the engine + a DummyAdapter (no browser). See `docs/specs/` and `docs/plans/`.

## Build
```
source ~/.cargo/env
cargo build --release
```

## Run the daemon
```
./target/release/pacewrightd          # creates ~/.pacewright/{pw.sock,pacewright.db}
```

## Use the CLI
```
./target/release/pacewright add dummy echo --params '{"hi":1}'
./target/release/pacewright list --status succeeded
./target/release/pacewright tui
```

## Install as a launchd service
```
cp packaging/config.example.toml ~/.pacewright/config.toml   # edit limits
sed "s#__HOME__#$HOME#g" packaging/com.paperclip.pacewrightd.plist > ~/Library/LaunchAgents/com.paperclip.pacewrightd.plist
launchctl load ~/Library/LaunchAgents/com.paperclip.pacewrightd.plist
```

## Known gaps (M1)

The following are intentionally deferred to later milestones, not oversights:

- **`set_limit` RPC is not implemented.** Limits are configured via `config.toml`
  only; there is no runtime RPC to change them yet.
- **`run_now --force` is accepted but does not yet bypass pacing.** It resets
  `scheduled_for` to now and clears `next_eligible_at` (which effectively
  re-queues the task immediately), but it does not override an active limit
  defer computed on the next tick.
- **`subscribe` push is not implemented.** The TUI polls `list` + `status`
  on an interval rather than receiving a pushed event stream.
- **`pause`/`resume` are acknowledged no-ops.** They return a success response
  but do not yet stop or resume tick processing.
- **Adapter-panic isolation at the runner boundary is deferred to M2.** A
  panic inside an adapter's `run` implementation is not yet caught at the
  per-task level. (The daemon's tick loop itself now survives a panic in
  `tick()` regardless — see `crates/daemon/src/server.rs` — so a single bad
  tick no longer freezes the scheduler, but a panicking adapter call within a
  tick is still a gap to close in M2.)
- **The single-instance guard is best-effort.** `pacewrightd` refuses to bind
  a socket that's already in `AddrInUse`, but there is no OS-level lock file;
  in practice launchd enforces running a single instance.
