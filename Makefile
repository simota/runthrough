CARGO ?= cargo
# e.g. `make check CARGO_FLAGS=--offline` when dependencies are already fetched
CARGO_FLAGS ?=
# e.g. `make ci CI_FLAGS="--engine container -P ubuntu-latest=rust:1"`
CI_FLAGS ?= --host

.DEFAULT_GOAL := help
.PHONY: help build install uninstall test lint fmt typecheck check ci clean

help: ## Show this help
	@grep -E '^[a-z]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-10s %s\n", $$1, $$2}'

build: ## Build the binary
	$(CARGO) build $(CARGO_FLAGS)

install: ## Install the binary into ~/.cargo/bin (cargo install --path .)
	$(CARGO) install $(CARGO_FLAGS) --path .

uninstall: ## Remove the installed binary
	$(CARGO) uninstall runthrough

test: ## Run unit and e2e tests (e2e needs bash and node)
	$(CARGO) test $(CARGO_FLAGS)

lint: ## Check formatting and run clippy with warnings as errors
	$(CARGO) fmt --check
	$(CARGO) clippy $(CARGO_FLAGS) --all-targets -- -D warnings

fmt: ## Format the code
	$(CARGO) fmt

typecheck: ## Type-check all targets without building
	$(CARGO) check $(CARGO_FLAGS) --all-targets

check: typecheck lint test ## Run typecheck, lint and test

ci: ## Run this repo's own .github/workflows with runthrough (CI_FLAGS, default --host)
	$(CARGO) run $(CARGO_FLAGS) -- $(CI_FLAGS)

clean: ## Remove build artifacts
	$(CARGO) clean
