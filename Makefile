ifeq ($(shell uname -m), arm64)
	ARCH := _arm64
else
	ARCH :=
endif

build: prebuild
	npm run tauri build

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
	# Clean and prepare target/package folder
	rm -rf target/package
	mkdir -p target/package
	# Copy binary
ifeq ($(OS),linux)
	cp src-tauri/target/release/bundle/deb/*.deb target/package/aw-tauri$(ARCH).deb
	cp src-tauri/target/release/bundle/rpm/*.rpm target/package/aw-tauri$(ARCH).rpm
	cp src-tauri/target/release/bundle/appimage/*.AppImage target/package/aw-tauri$(ARCH).AppImage
else
	cp src-tauri/target/release/aw-tauri target/package/aw-tauri
endif
	# Copy everything into `dist/aw-tauri`
	mkdir -p dist
	find dist/ -maxdepth 1 -type f -delete 2>/dev/null || true
	cp target/package/* dist/

node_modules: package-lock.json
	npm ci
