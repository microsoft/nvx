PYTHON ?= python3
PYRIGHT ?= $(PYTHON) -m pyright
RUFF ?= $(PYTHON) -m ruff
SHELLCHECK ?= shellcheck
SHFMT ?= shfmt

PYTHON_SOURCES := scripts benchmarks
SHELL_SOURCES := alpine/init alpine/nvx-exit alpine/nvx-hostmount alpine/nvx-snapshot
SHFMT_FLAGS := -ln posix -i 4 -ci

.PHONY: all init guest guest-native openvmm run benchmark sources verify package \
	check typecheck lint lint-python lint-shell format format-python format-shell \
	format-check format-check-python format-check-shell clean

all: guest openvmm

init:
	$(PYTHON) scripts/nvx.py init

guest:
	$(PYTHON) scripts/nvx.py build-guest

guest-native:
	$(PYTHON) scripts/nvx.py build-guest --native

openvmm:
	$(PYTHON) scripts/nvx.py build-openvmm

run:
	$(PYTHON) scripts/nvx.py run

benchmark:
	$(PYTHON) scripts/nvx.py benchmark --suite boot

sources:
	$(PYTHON) scripts/nvx.py collect-sources

verify:
	$(PYTHON) scripts/nvx.py verify

package: sources
	$(PYTHON) scripts/nvx.py package --include-source

check: lint typecheck format-check

typecheck:
	$(PYRIGHT) --pythonplatform Linux
	$(PYRIGHT) --pythonplatform Windows

lint: lint-python lint-shell

lint-python:
	$(RUFF) check $(PYTHON_SOURCES)

lint-shell:
	$(SHELLCHECK) --shell=sh $(SHELL_SOURCES)

format: format-python format-shell

format-python:
	$(RUFF) format $(PYTHON_SOURCES)

format-shell:
	$(SHFMT) -w $(SHFMT_FLAGS) $(SHELL_SOURCES)

format-check: format-check-python format-check-shell

format-check-python:
	$(RUFF) format --check $(PYTHON_SOURCES)

format-check-shell:
	$(SHFMT) -d $(SHFMT_FLAGS) $(SHELL_SOURCES)

clean:
	@echo "Remove build/ or dist/ explicitly; this target is intentionally non-destructive."
