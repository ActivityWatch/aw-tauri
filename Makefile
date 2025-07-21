# Build in release mode by default, unless RELEASE=false
ifeq ($(RELEASE), false)
		cargoflag :=
		targetdir := debug
else
		cargoflag := --release
		targetdir := release
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
	cp src-tauri/target/$(targetdir)/aw-tauri target/package/aw-tauri
	# Copy everything into `dist/aw-tauri`
	mkdir -p dist
	rm -rf dist/aw-tauri
	cp -rf target/package dist/aw-tauri

node_modules: package-lock.json
	npm ci