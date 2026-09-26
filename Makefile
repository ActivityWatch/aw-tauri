ifeq ($(shell uname -m), arm64)
	ARCH := _arm64
else
	ARCH :=
endif
OS := $(shell uname -s)

# Updater artifacts (.tar.gz/.zip + .sig) require the signing key, so only
# enable them when TAURI_SIGNING_PRIVATE_KEY is set (release CI). Without it,
# plain bundles are built and local/fork-PR builds keep working.
TAURI_BUILD_ARGS :=
ifdef TAURI_SIGNING_PRIVATE_KEY
TAURI_BUILD_ARGS += --config '{"bundle":{"createUpdaterArtifacts":true}}'
endif

build: prebuild
	# When TAURI_SIGNING_PRIVATE_KEY is present, the Makefile override above sets
	# bundle.createUpdaterArtifacts=true. That flag is therefore absent from
	# tauri.conf.json even though signed builds produce .sig files.
	npm run tauri build -- $(TAURI_BUILD_ARGS)

dev: prebuild
	npm run tauri dev

%/.git:
	git submodule update --init --recursive

src-tauri/icons/icon.png: aw-webui/.git
	mkdir -p src-tauri/icons
	npm run tauri icon "./aw-webui/media/logo/logo.png"

aw-webui/dist: aw-webui/.git
	cd aw-webui && make build

prebuild: aw-webui/dist node_modules src-tauri/icons/icon.png

precommit: format check

format:
	cd src-tauri && cargo fmt

check:
	cd src-tauri && cargo check && cargo clippy

package:
ifeq ($(OS),Linux)
	rm -rf target/package/aw-tauri
	mkdir -p target/package/aw-tauri
	cp src-tauri/target/release/bundle/deb/*.deb target/package/aw-tauri/aw-tauri$(ARCH).deb
	cp src-tauri/target/release/bundle/rpm/*.rpm target/package/aw-tauri/aw-tauri$(ARCH).rpm
	cp src-tauri/target/release/bundle/appimage/*.AppImage target/package/aw-tauri/aw-tauri$(ARCH).AppImage

	mkdir -p dist/aw-tauri
	rm -rf dist/aw-tauri/*
	cp target/package/aw-tauri/* dist/aw-tauri/
else
	rm -rf target/package
	mkdir -p target/package
	cp src-tauri/target/release/aw-tauri target/package/aw-tauri

	mkdir -p dist
	find dist/ -maxdepth 1 -type f -delete 2>/dev/null || true
	cp target/package/* dist/
endif

node_modules: package-lock.json
	npm ci
