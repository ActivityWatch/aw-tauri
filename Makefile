build: prebuild
	npm run tauri build

dev: prebuild
	npm run tauri dev

*/.git:
	git submodule update --init --recursive

src-tauri/icons/icon.png: aw-webui/.git
	mkdir -p src-tauri/icons
	npm run tauri icon "./aw-webui/media/logo/logo.png"

aw-webui/dist: aw-webui/.git
	cd aw-webui && make

prebuild: aw-webui/dist src-tauri/icons/icon.png
