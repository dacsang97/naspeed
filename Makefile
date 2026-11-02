SHELL := /bin/sh

CARGO ?= cargo
DEV_BINDIR ?= $(HOME)/.local/bin
DEV_BINARY := target/debug/naspeed
DEV_MARKER := $(DEV_BINDIR)/.naspeed-dev-install
DEV_FORCE ?= 0

.PHONY: dev-build dev-install dev-uninstall dev-status

dev-build:
	$(CARGO) build --locked

dev-install: dev-build
	install -d "$(DEV_BINDIR)"
	@if { [ -e "$(DEV_BINDIR)/naspeed" ] || [ -L "$(DEV_BINDIR)/naspeed" ] || \
		[ -e "$(DEV_BINDIR)/ns" ] || [ -L "$(DEV_BINDIR)/ns" ]; } && \
		{ [ ! -f "$(DEV_MARKER)" ] || \
		[ "$$(cat "$(DEV_MARKER)")" != "$(CURDIR)" ]; } && \
		[ "$(DEV_FORCE)" != 1 ]; then \
		printf 'Refusing to replace unowned files in %s.\n' "$(DEV_BINDIR)" >&2; \
		printf 'Move them first, or rerun with DEV_FORCE=1 to take ownership.\n' >&2; \
		exit 1; \
	fi
	install -m 0755 "$(DEV_BINARY)" "$(DEV_BINDIR)/naspeed"
	ln -sfn naspeed "$(DEV_BINDIR)/ns"
	@printf '%s\n' "$(CURDIR)" > "$(DEV_MARKER)"
	@printf 'Installed development binaries:\n  %s\n  %s\n' \
		"$(DEV_BINDIR)/naspeed" "$(DEV_BINDIR)/ns"
	@printf 'Run `hash -r`, then `make dev-status` to verify PATH precedence.\n'

dev-uninstall:
	@if [ ! -f "$(DEV_MARKER)" ] || \
		[ "$$(cat "$(DEV_MARKER)")" != "$(CURDIR)" ]; then \
		printf 'Refusing to remove files not owned by this checkout in %s.\n' \
			"$(DEV_BINDIR)" >&2; \
		exit 1; \
	fi; \
	if [ -e "$(DEV_BINDIR)/ns" ] || [ -L "$(DEV_BINDIR)/ns" ]; then \
		unlink "$(DEV_BINDIR)/ns"; \
	fi; \
	if [ -e "$(DEV_BINDIR)/naspeed" ] || [ -L "$(DEV_BINDIR)/naspeed" ]; then \
		unlink "$(DEV_BINDIR)/naspeed"; \
	fi; \
	unlink "$(DEV_MARKER)"
	@printf 'Removed development binaries from %s. The Homebrew install was not changed.\n' \
		"$(DEV_BINDIR)"

dev-status:
	@printf 'Development binary: '
	@if [ -x "$(DEV_BINDIR)/naspeed" ]; then \
		"$(DEV_BINDIR)/naspeed" --version; \
	else \
		printf 'not installed\n'; \
	fi
	@printf 'Managed here:      '
	@if [ -f "$(DEV_MARKER)" ] && \
		[ "$$(cat "$(DEV_MARKER)")" = "$(CURDIR)" ]; then \
		printf 'yes\n'; \
	else \
		printf 'no\n'; \
	fi
	@printf 'Active naspeed: %s\n' "$$(command -v naspeed 2>/dev/null || printf 'not found')"
	@printf 'Active ns:      %s\n' "$$(command -v ns 2>/dev/null || printf 'not found')"
