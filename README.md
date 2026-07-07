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
