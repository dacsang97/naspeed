SHELL := /bin/sh

CARGO ?= cargo
DEV_BINDIR ?= $(HOME)/.local/bin
DEV_BINARY := target/debug/naspeed
DEV_MARKER := $(DEV_BINDIR)/.naspeed-dev-install
DEV_FORCE ?= 0
FUNCTION_TOOLCHAIN ?= stable
FUNCTION_DRIVER := target/function-tools/naspeed-rustc-functions
SPLIT_DRIVER := $(dir $(FUNCTION_DRIVER))naspeed-rustc-split
LAZY_LAUNCHER := $(dir $(FUNCTION_DRIVER))naspeed-lazy-launch
LAZY_DRIVER := $(dir $(FUNCTION_DRIVER))naspeed-rustc-lazy
DEMAND_DRIVER := $(dir $(FUNCTION_DRIVER))naspeed-rustc-demand
NASC := $(dir $(FUNCTION_DRIVER))nasc
LLVM_CONFIG ?= llvm-config
FUNCTION_RUSTC_REV := 58cf8f9c35b355199727627d07ae34a4e2846d67

.PHONY: dev-build dev-install dev-uninstall dev-status

dev-build:
	$(CARGO) build --locked

.PHONY: function-driver function-driver-test
function-driver:
	@set -eu; \
	rustc="$$(rustup which --toolchain "$(FUNCTION_TOOLCHAIN)" rustc)"; \
	"$$rustc" -vV | grep -Fqx 'commit-hash: $(FUNCTION_RUSTC_REV)' || { \
		printf 'Function codegen needs rustc 1.99.1 (%s).\n' "$(FUNCTION_RUSTC_REV)" >&2; exit 1; \
	}; \
	sysroot="$$("$$rustc" --print sysroot)"; \
	mkdir -p "$(dir $(FUNCTION_DRIVER))"; \
	NASPEED_DRIVER_RUSTC="$$rustc" NASPEED_DRIVER_SYSROOT="$$sysroot" \
		RUSTC_BOOTSTRAP=naspeed_rustc_functions "$$rustc" --edition=2024 \
		--crate-name naspeed_rustc_functions -C opt-level=2 \
		-L "native=$$sysroot/lib" -C "link-arg=-Wl,-rpath,$$sysroot/lib" \
		src/compiler/function_driver.rs -o "$(FUNCTION_DRIVER)"; \
	ln -sfn "$(notdir $(FUNCTION_DRIVER))" "$(SPLIT_DRIVER)"; \
	printf '%s\n' "$$rustc" > "$(FUNCTION_DRIVER).toolchain"

.PHONY: lazy-driver lazy-driver-test
lazy-driver: function-driver
	@set -eu; \
	test "$$(uname -ms)" = 'Darwin arm64' || { echo 'The lazy driver requires Apple Silicon macOS.' >&2; exit 1; }; \
	llvm="$(LLVM_CONFIG)"; \
	"$$llvm" --version | grep -q '^23\.' || { echo 'Set LLVM_CONFIG to LLVM 23 llvm-config.' >&2; exit 1; }; \
	bin="$$("$$llvm" --bindir)"; \
	builtins="$$("$$bin/clang" --print-resource-dir)/lib/darwin/libclang_rt.osx.a"; \
	test -f "$$builtins"; \
	"$$bin/clang++" $$("$$llvm" --cxxflags) -O2 "-DNASPEED_CLANG_BUILTINS=\"$$builtins\"" \
		src/compiler/lazy_launcher.cpp src/compiler/demand_ir.cpp src/compiler/dep_image.cpp src/compiler/app_patch.cpp \
		$$("$$llvm" --ldflags --system-libs --libs orcjit native object) \
		-o "$(LAZY_LAUNCHER)"; \
	ln -sfn "$(notdir $(FUNCTION_DRIVER))" "$(LAZY_DRIVER)"; \
	ln -sfn "$(notdir $(FUNCTION_DRIVER))" "$(DEMAND_DRIVER)"; \
	rustc --edition=2024 --crate-name nasc -C opt-level=2 \
		src/compiler/lang/main.rs -o "$(NASC)"

lazy-driver-test: lazy-driver dev-build
	python3 -B scripts/test_lazy_run.py "$(DEV_BINARY)" "$(LAZY_DRIVER)" "$(FUNCTION_TOOLCHAIN)"
	python3 -B scripts/test_demand_ir.py "$(DEV_BINARY)" "$(LAZY_DRIVER)"
	python3 -B scripts/test_cargo_demand_ir.py "$(DEV_BINARY)" "$(LAZY_DRIVER)" "$(FUNCTION_TOOLCHAIN)"
	python3 -B scripts/test_app_patch.py "$(DEV_BINARY)" "$(LAZY_DRIVER)" "$(FUNCTION_TOOLCHAIN)"
	python3 -B scripts/test_app_live.py "$(DEV_BINARY)" "$(LAZY_DRIVER)" "$(FUNCTION_TOOLCHAIN)"
	python3 -B scripts/test_app_cache_gc.py "$(DEV_BINARY)" "$(LAZY_DRIVER)" "$(FUNCTION_TOOLCHAIN)"

function-driver-test: function-driver dev-build
	python3 scripts/test_function_codegen.py "$(DEV_BINARY)" "$(FUNCTION_DRIVER)" "$(FUNCTION_TOOLCHAIN)"
	python3 -B scripts/test_split_link.py "$(DEV_BINARY)" "$(FUNCTION_DRIVER)" "$(FUNCTION_TOOLCHAIN)"

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
	@if [ -f "$(FUNCTION_DRIVER)" ]; then \
		install -m 0755 "$(FUNCTION_DRIVER)" "$(DEV_BINDIR)/naspeed-rustc-split"; \
		if [ -f "$(FUNCTION_DRIVER).toolchain" ]; then \
			install -m 0644 "$(FUNCTION_DRIVER).toolchain" "$(DEV_BINDIR)/naspeed-rustc-split.toolchain"; \
		fi; \
	fi
	@if [ -f "$(NASC)" ]; then \
		install -m 0755 "$(NASC)" "$(DEV_BINDIR)/nasc"; \
	fi
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
