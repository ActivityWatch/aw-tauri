build:
	if [ -e "media/.git" ]; then \
		echo "Submodule seems to already be initialized, continuing..."; \
	else \
		git submodule update --init --recursive; \
	fi
	
	npm run tauri build

dev:
	if [ -e "media/.git" ]; then \
		echo "Submodule seems to already be initialized, continuing..."; \
	else \
		git submodule update --init --recursive; \
	fi

	npm run tauri dev
