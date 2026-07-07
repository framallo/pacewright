# pacewright — dev tasks
# Rust isn't on the default PATH on this Mac; prepend the cargo bin dir so
# every recipe (which runs in its own shell) finds cargo/rustc/pacewright.
export PATH := $(HOME)/.cargo/bin:$(PATH)

CARGO   := cargo
PW_DIR  := $(HOME)/.pacewright
SOCK    := $(PW_DIR)/pw.sock
DAEMON  := ./target/release/pacewrightd
CLI     := ./target/release/pacewright
PLIST   := com.paperclip.pacewrightd.plist
LAUNCHD := $(HOME)/Library/LaunchAgents/$(PLIST)

.DEFAULT_GOAL := help

## ---- Build ----------------------------------------------------------------

.PHONY: build
build: ## Debug build of the whole workspace
	$(CARGO) build

.PHONY: release
release: ## Optimized release build (produces target/release/{pacewrightd,pacewright})
	$(CARGO) build --release

## ---- Test / quality -------------------------------------------------------

.PHONY: test
test: ## Run all workspace tests
	$(CARGO) test

.PHONY: test-core
test-core: ## Run only the pacewright-core tests
	$(CARGO) test -p pacewright-core

.PHONY: e2e
e2e: ## Run the daemon end-to-end integration test
	$(CARGO) test -p pacewright-daemon --test e2e

.PHONY: fmt
fmt: ## Format all code (rustfmt)
	$(CARGO) fmt

.PHONY: lint
lint: ## Lint with clippy (warnings as errors)
	$(CARGO) clippy --all-targets -- -D warnings

.PHONY: check
check: fmt lint test ## Format, lint, then test — the pre-commit gate

## ---- Run ------------------------------------------------------------------

.PHONY: run
run: release ## Build release + run the daemon in the foreground (Ctrl-C to stop)
	$(DAEMON)

.PHONY: daemon
daemon: release ## Build release + start the daemon in the background
	@$(DAEMON) & echo "pacewrightd started (pid $$!), socket $(SOCK)"

.PHONY: tui
tui: release ## Open the live ratatui dashboard (q to quit)
	$(CLI) tui

.PHONY: demo
demo: ## Enqueue a sample dummy 'echo' task (daemon must be running)
	$(CLI) add dummy echo --params '{"hello":"world"}'
	@sleep 2 && $(CLI) list --status succeeded

.PHONY: adapters
adapters: ## List registered adapters and their actions
	$(CLI) adapters

.PHONY: status
status: ## Show daemon status (queue depth, running tasks)
	$(CLI) status

## ---- launchd install ------------------------------------------------------

.PHONY: install
install: release ## Install the launchd job so the daemon runs at login
	@mkdir -p $(PW_DIR)
	@[ -f $(PW_DIR)/config.toml ] || cp packaging/config.example.toml $(PW_DIR)/config.toml
	@sed "s#__HOME__#$(HOME)#g" packaging/$(PLIST) > $(LAUNCHD)
	@launchctl unload $(LAUNCHD) 2>/dev/null || true
	@launchctl load $(LAUNCHD)
	@echo "installed + loaded $(LAUNCHD)"

.PHONY: uninstall
uninstall: ## Stop and remove the launchd job
	@launchctl unload $(LAUNCHD) 2>/dev/null || true
	@rm -f $(LAUNCHD)
	@echo "uninstalled $(LAUNCHD)"

.PHONY: logs
logs: ## Tail the daemon's launchd logs
	@tail -f $(PW_DIR)/pacewrightd.out.log $(PW_DIR)/pacewrightd.err.log

## ---- Housekeeping ---------------------------------------------------------

.PHONY: clean
clean: ## Remove build artifacts (cargo clean)
	$(CARGO) clean

.PHONY: help
help: ## Show this help
	@echo "pacewright — make targets:"
	@echo ""
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "Typical flow:  make check  ·  make daemon  ·  make demo  ·  make tui"
