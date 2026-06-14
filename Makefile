# rightkeys Makefile (cross-platform)
#
# Build (both platforms):
#     make build
#
# Linux install (system-wide; build as your user first):
#     make build
#     sudo make install
# Linux install (current user only; ~/.local must be on PATH):
#     make build
#     make install PREFIX=$(HOME)/.local
#
# Windows install (per-user, no admin; needs PowerShell):
#     make build
#     make install
# This copies rightkeys.exe to %LOCALAPPDATA%\Programs\rightkeys, seeds the
# config, adds the folder to your user PATH, and creates a Startup shortcut so
# the tray app launches at login. Override the location with WINPREFIX=...
#
# Other targets: icons, install-config, uninstall, clean

CARGO ?= cargo

.PHONY: all build install uninstall icons install-config clean help

all: build

build:
	$(CARGO) build --release

clean:
	$(CARGO) clean

help:
	@echo Targets: build install install-config uninstall icons clean
	@echo Run make build then make install

ifeq ($(OS),Windows_NT)
# ============================ Windows =================================
# All install logic lives in scripts/windows-setup.ps1 so a single recipe
# survives whichever shell mingw32-make picks (cmd.exe or sh).

WINPREFIX ?= $(LOCALAPPDATA)\Programs\rightkeys
PS        := powershell -NoProfile -ExecutionPolicy Bypass

install:
	$(PS) -File scripts/windows-setup.ps1 -Action install -Prefix "$(WINPREFIX)"

install-config:
	$(PS) -File scripts/windows-setup.ps1 -Action config

uninstall:
	$(PS) -File scripts/windows-setup.ps1 -Action uninstall -Prefix "$(WINPREFIX)"

icons:
	$(PS) -Command "Write-Host 'icon regeneration is Linux-only; the tray icon is embedded into the exe at build time.'"

else
# ============================= Linux =================================

PREFIX     ?= /usr/local
DESTDIR    ?=
INKSCAPE   ?= inkscape
CONVERT    ?= convert

BINDIR     := $(DESTDIR)$(PREFIX)/bin
DATADIR    := $(DESTDIR)$(PREFIX)/share
APPDIR     := $(DATADIR)/applications
ICONDIR    := $(DATADIR)/icons/hicolor

ICON_SIZES := 16 24 32 48 64 128 256 512
SVG        := assets/icons/rightkeys.svg
DESKTOP    := assets/rightkeys.desktop
BIN        := target/release/rightkeys

install:
	@test -x $(BIN) || { \
		echo "error: $(BIN) not found. Run 'make build' first (as your user, not root)."; \
		exit 1; \
	}
	install -Dm755 $(BIN) $(BINDIR)/rightkeys
	for s in $(ICON_SIZES); do \
		install -Dm644 assets/icons/rightkeys-$$s.png \
			$(ICONDIR)/$${s}x$${s}/apps/rightkeys.png; \
	done
	install -Dm644 $(SVG) $(ICONDIR)/scalable/apps/rightkeys.svg
	install -Dm644 $(DESKTOP) $(APPDIR)/rightkeys.desktop
	-update-desktop-database $(APPDIR) 2>/dev/null || true
	-gtk-update-icon-cache -f -t $(ICONDIR) 2>/dev/null || true
	@echo "installed rightkeys to $(PREFIX)"

uninstall:
	rm -f $(BINDIR)/rightkeys
	for s in $(ICON_SIZES); do \
		rm -f $(ICONDIR)/$${s}x$${s}/apps/rightkeys.png; \
	done
	rm -f $(ICONDIR)/scalable/apps/rightkeys.svg
	rm -f $(APPDIR)/rightkeys.desktop
	-update-desktop-database $(APPDIR) 2>/dev/null || true
	-gtk-update-icon-cache -f -t $(ICONDIR) 2>/dev/null || true
	@echo "uninstalled rightkeys"

# Regenerate the PNG icon set and the Windows .ico from the SVG.
icons:
	for s in $(ICON_SIZES); do \
		$(INKSCAPE) -w $$s -h $$s $(SVG) -o assets/icons/rightkeys-$$s.png; \
	done
	$(CONVERT) assets/icons/rightkeys-16.png assets/icons/rightkeys-32.png \
		assets/icons/rightkeys-48.png assets/icons/rightkeys-256.png \
		assets/icons/rightkeys.ico

# Copy the example config to the per-user location if none exists yet.
install-config:
	@mkdir -p $(HOME)/.config/rightkeys
	@if [ -f $(HOME)/.config/rightkeys/config.kdl ]; then \
		echo "config exists; not overwriting ~/.config/rightkeys/config.kdl"; \
	else \
		cp config.example.kdl $(HOME)/.config/rightkeys/config.kdl; \
		echo "installed config to ~/.config/rightkeys/config.kdl"; \
	fi

endif
