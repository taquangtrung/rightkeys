# rightkeys Makefile (cross-platform)
#
# `install` builds first, so a single command suffices. Under sudo it builds as
# the invoking user, so `target/` is never left root-owned.
#
# Build only (both platforms):
#     make build
#
# Linux install (system-wide):
#     make install          # sudo is added automatically if PREFIX requires it
# Linux install (current user only; ~/.local must be on PATH):
#     make install PREFIX=$(HOME)/.local
#
# Windows install (per-user, no admin; needs PowerShell):
#     make install
# This copies rightkeys.exe to %LOCALAPPDATA%\Programs\rightkeys, seeds the
# config, adds the folder to your user PATH, and creates a Startup shortcut so
# the tray app launches at login. Override the location with WINPREFIX=...
#
# Other targets: icons, install-config, uninstall, clean

CARGO ?= cargo

.PHONY: all build install stage deb uninstall icons install-config clean help

all: build

# Build the release binary. When invoked under sudo (e.g. `sudo make install`),
# build as the original user via a login shell so cargo never writes root-owned
# artifacts into target/ (or root's cargo cache) and still finds the user's
# toolchain on PATH.
build:
ifneq ($(SUDO_USER),)
	sudo -u $(SUDO_USER) bash -lc 'cd "$(CURDIR)" && $(CARGO) build --release'
else
	$(CARGO) build --release
endif

clean:
	$(CARGO) clean
	rm -rf target/deb

help:
	@echo Targets: build install install-config uninstall icons clean
	@echo Run make install (it builds first)

ifeq ($(OS),Windows_NT)
# ============================ Windows =================================
# All install logic lives in scripts/windows-setup.ps1 so a single recipe
# survives whichever shell mingw32-make picks (cmd.exe or sh).

WINPREFIX ?= $(LOCALAPPDATA)\Programs\rightkeys
PS        := powershell -NoProfile -ExecutionPolicy Bypass

install: build
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
ICON_PNGS  := $(patsubst %,assets/icons/rightkeys-%.png,$(ICON_SIZES))
ICO        := assets/icons/rightkeys.ico
SVG        := assets/icons/rightkeys.svg
DESKTOP    := assets/rightkeys.desktop
BIN        := target/release/rightkeys

# Prepend sudo unless running as root. Used by `dpkg -i` / `dpkg -r`.
# Override with SUDO= to disable.
UID_ := $(shell id -u)
SUDO := $(if $(filter 0,$(UID_)),,sudo)

# --- Debian package (.deb) ---------------------------------------------------
# `make install` builds a .deb and installs it with `dpkg -i`, so the package
# manager owns the files and `dpkg -r rightkeys` removes them cleanly. The deb
# follows FHS layout under /usr (not /usr/local). For a non-package,
# prefix-overridable file copy, use `make install-files`.

STAGE      := target/deb/root
DEBDIR     := target/deb
VERSION    := $(shell grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
ARCH       := $(shell dpkg --print-architecture 2>/dev/null || echo amd64)
DESC       := $(shell grep -m1 '^description' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
HOMEPAGE   := $(shell grep -m1 '^homepage' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
M_NAME     := $(shell git config user.name 2>/dev/null || echo RightKeys)
M_MAIL     := $(shell git config user.email 2>/dev/null || echo rightkeys@localhost)
DEB        := $(DEBDIR)/rightkeys_$(VERSION)_$(ARCH).deb

# Stage the install tree into $(STAGE) under prefix /usr (FHS for packages).
# No sudo: everything lands under the user-owned target/ dir.
stage: build
	rm -rf $(STAGE)
	install -Dm755 $(BIN) $(STAGE)/usr/bin/rightkeys
	$(foreach s,$(ICON_SIZES),install -Dm644 assets/icons/rightkeys-$(s).png $(STAGE)/usr/share/icons/hicolor/$(s)x$(s)/apps/rightkeys.png;)
	install -Dm644 $(SVG) $(STAGE)/usr/share/icons/hicolor/scalable/apps/rightkeys.svg
	install -Dm644 $(DESKTOP) $(STAGE)/usr/share/applications/rightkeys.desktop
	@mkdir -p $(STAGE)/DEBIAN
	@printf 'Package: rightkeys\nVersion: %s\nSection: utils\nPriority: optional\nArchitecture: %s\nDepends: libgtk-3-0, xdotool\nMaintainer: %s <%s>\nHomepage: %s\nDescription: %s\n Modmaps, tap-hold keys, multi-step bindings, selection-aware remaps,\n window management, app launching, and a hint-based element/window picker.\n' \
		"$(VERSION)" "$(ARCH)" "$(M_NAME)" "$(M_MAIL)" "$(HOMEPAGE)" "$(DESC)" \
		> $(STAGE)/DEBIAN/control
	@printf '#!/bin/sh\nset -e\nif command -v update-desktop-database >/dev/null 2>&1; then update-desktop-database -q /usr/share/applications || true; fi\nif command -v gtk-update-icon-cache >/dev/null 2>&1; then gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true; fi\n' > $(STAGE)/DEBIAN/postinst
	@printf '#!/bin/sh\nset -e\nif command -v update-desktop-database >/dev/null 2>&1; then update-desktop-database -q /usr/share/applications || true; fi\nif command -v gtk-update-icon-cache >/dev/null 2>&1; then gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true; fi\n' > $(STAGE)/DEBIAN/postrm
	@chmod 755 $(STAGE)/DEBIAN/postinst $(STAGE)/DEBIAN/postrm

# Build the .deb from the staged tree. --root-owner-group sets root:root on
# every file so the package installs with correct ownership.
deb: stage
	@mkdir -p $(DEBDIR)
	dpkg-deb --build --root-owner-group $(STAGE) $(DEB)
	@echo "built $(DEB)"

# Install by building the .deb and handing it to dpkg.
install: deb
	$(SUDO) dpkg -i $(DEB)
	@echo "installed rightkeys $(VERSION) via $(DEB)"

# Exact reverse of `make install`: hand removal to dpkg, which owns every
# file the deb laid down and runs the deb's postrm to refresh the menu/icon
# caches. `apt remove rightkeys` / `apt purge rightkeys` do the same thing.
# The manual sweep is a fallback for the legacy `install-files` layout (which
# dpkg doesn't track) and a no-op when the package was just removed.
# User config (~/.config/rightkeys) is intentionally preserved, as apt does;
# use `make uninstall-config` to remove it.
uninstall:
	@if dpkg -s rightkeys >/dev/null 2>&1; then \
		$(SUDO) dpkg -r rightkeys; \
	else \
		echo "rightkeys not installed as a package; removing files manually"; \
		$(foreach d,/usr /usr/local,\
			$(SUDO) rm -f $(d)/bin/rightkeys $(d)/share/applications/rightkeys.desktop $(d)/share/icons/hicolor/scalable/apps/rightkeys.svg;) \
		$(foreach s,$(ICON_SIZES),\
			$(SUDO) rm -f /usr/share/icons/hicolor/$(s)x$(s)/apps/rightkeys.png /usr/local/share/icons/hicolor/$(s)x$(s)/apps/rightkeys.png;) \
		-$(SUDO) update-desktop-database -q /usr/share/applications /usr/local/share/applications 2>/dev/null || true; \
		-$(SUDO) gtk-update-icon-cache -q -t /usr/share/icons/hicolor 2>/dev/null || true; \
		-$(SUDO) gtk-update-icon-cache -q -t /usr/local/share/icons/hicolor 2>/dev/null || true; \
	fi
	@echo "uninstalled rightkeys"

# Remove the per-user config (the seed copied by `make install-config`).
# Separate so `make uninstall` keeps your config by default, as apt does.
uninstall-config:
	@rm -rf $(HOME)/.config/rightkeys
	@echo "removed ~/.config/rightkeys"

# --- Non-package install (legacy file copy, honours PREFIX/DESTDIR) ----------

install-files: build
	$(SUDO) install -Dm755 $(BIN) $(BINDIR)/rightkeys
	$(foreach s,$(ICON_SIZES),$(SUDO) install -Dm644 assets/icons/rightkeys-$(s).png $(ICONDIR)/$(s)x$(s)/apps/rightkeys.png;)
	$(SUDO) install -Dm644 $(SVG) $(ICONDIR)/scalable/apps/rightkeys.svg
	$(SUDO) install -Dm644 $(DESKTOP) $(APPDIR)/rightkeys.desktop
	-$(SUDO) update-desktop-database $(APPDIR) 2>/dev/null || true
	-$(SUDO) gtk-update-icon-cache -f -t $(ICONDIR) 2>/dev/null || true
	@echo "installed rightkeys to $(PREFIX)"

# Each PNG is rebuilt only when the SVG changes; $* expands to the size stem.
assets/icons/rightkeys-%.png: $(SVG)
	$(INKSCAPE) -w $* -h $* $(SVG) -o $@

$(ICO): assets/icons/rightkeys-16.png assets/icons/rightkeys-32.png \
        assets/icons/rightkeys-48.png assets/icons/rightkeys-256.png
	$(CONVERT) $^ $@

# Regenerate every committed bitmap (desktop set, Windows .ico) from the source
# SVG. The only target that needs inkscape; a plain build does not.
icons: $(ICON_PNGS) $(ICO)

# Copy the example config to the per-user location if none exists yet.
install-config:
	@mkdir -p $(HOME)/.config/rightkeys
	@if [ -f $(HOME)/.config/rightkeys/settings.kdl ]; then \
		echo "config exists; not overwriting ~/.config/rightkeys/settings.kdl"; \
	else \
		cp config.example.kdl $(HOME)/.config/rightkeys/settings.kdl; \
		echo "installed config to ~/.config/rightkeys/settings.kdl"; \
	fi

endif
