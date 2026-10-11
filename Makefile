# The two things everyone does with this checkout. `cargo install --path` builds in a private
# temp target dir by default — a full rebuild every time — so `install` points it at the
# workspace's own `target/` (same release profile, incremental).
CARGO ?= cargo
BINS  ?= crates/cv crates/cv-mcp crates/cvd   # the last two are one-release exec shims

.PHONY: build install version test

build:
	$(CARGO) build --release

## Install cv (and the cv-mcp/cvd shims) into ~/.cargo/bin from this checkout, reusing target/release.
install:
	@for b in $(BINS); do $(CARGO) install --path $$b --force --target-dir target || exit 1; done
	@echo; cv --version; echo "checkout: $$(git rev-parse --short=12 HEAD)$$(git status --porcelain | grep -q . && echo -dirty)"

## Does the installed binary match this checkout?
version:
	@echo "installed: $$(cv --version)"
	@echo "checkout:  $$(git rev-parse --short=12 HEAD)$$(git status --porcelain | grep -q . && echo -dirty)"

## CI's gate, exactly.
test:
	$(CARGO) test --workspace
