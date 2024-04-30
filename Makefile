export AW_WEBUI_DIR=$(PWD)/aw-webui/dist

prebuild:
	if [ -e "aw-webui/.git" ]; then \
		echo "Submodule seems to already be initialized, continuing..."; \
	else \
		git submodule update --init --recursive; \
	fi

	if [ -e "aw-webui/dist"]; then \
		echo "Aw-webui seems to already be built, continuing..."; \
	else \
		cd aw-webui && make; \
	fi

	npm run tauri icon "./aw-webui/media/logo/logo.png"

build:
	npm run tauri build

dev:
	npm run tauri dev
