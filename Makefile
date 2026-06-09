# rightkeys Makefile
#
# Install system-wide (build as your user first; cargo is not on root's PATH):
#     make build
#     sudo make install
#
# Install for the current user only (no root; ~/.local must be on PATH):
#     make build
#     make install PREFIX=$(HOME)/.local
#
# Other targets: icons, install-config, uninstall, clean

PREFIX     ?= /usr/local
DESTDIR    ?=
CARGO      ?= cargo
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

.PHONY: all build install uninstall icons install-config clean help

all: build

help:
	@echo "Targets: build, install, uninstall, icons, install-config, clean"
	@echo "Vars:    PREFIX (default /usr/local), DESTDIR"

build:
	$(CARGO) build --release

# Regenerate the PNG icon set and the Windows .ico from the SVG.
icons:
	for s in $(ICON_SIZES); do \
		$(INKSCAPE) -w $$s -h $$s $(SVG) -o assets/icons/rightkeys-$$s.png; \
	done
	$(CONVERT) assets/icons/rightkeys-16.png assets/icons/rightkeys-32.png \
		assets/icons/rightkeys-48.png assets/icons/rightkeys-256.png \
		assets/icons/rightkeys.ico

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

# Copy the example config to the per-user location if none exists yet.
install-config:
	@mkdir -p $(HOME)/.config/rightkeys
	@if [ -f $(HOME)/.config/rightkeys/config.kdl ]; then \
		echo "config exists; not overwriting ~/.config/rightkeys/config.kdl"; \
	else \
		cp config.example.kdl $(HOME)/.config/rightkeys/config.kdl; \
		echo "installed config to ~/.config/rightkeys/config.kdl"; \
	fi

clean:
	$(CARGO) clean
