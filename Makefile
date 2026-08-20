PYTHON ?= python3

.PHONY: all init guest guest-native openvmm run benchmark sources verify package clean

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
	$(PYTHON) scripts/nvx.py benchmark -- --suite boot

sources:
	$(PYTHON) scripts/nvx.py collect-sources

verify:
	$(PYTHON) scripts/nvx.py verify

package: sources
	$(PYTHON) scripts/nvx.py package --include-source

clean:
	@echo "Remove build/ or dist/ explicitly; this target is intentionally non-destructive."
