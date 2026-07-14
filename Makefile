# Installs hark into the desktop: the binary, its icon and a launcher GNOME
# can find. Defaults to a per-user install, so no root is needed:
#
#   make install                      -> ~/.local
#   make install PREFIX=/usr/local    -> system-wide (needs sudo)
#   make uninstall
#
# DESTDIR is honoured for packaging; the desktop and icon caches are then left
# alone, since the package manager refreshes them on the real root.

PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share

APPID := dev.milan.hark
BIN := hark

DESKTOP_DIR := $(DATADIR)/applications
ICON_DIR := $(DATADIR)/icons/hicolor/scalable/apps

BUILT_DESKTOP := target/$(APPID).desktop

.PHONY: all build install uninstall clean

all: build

build:
	cargo build --release

# GNOME launches the entry with the session's PATH, which need not contain
# $(BINDIR) — so the launcher gets the absolute path baked in. It is rebuilt on
# every install rather than kept as a file target: its contents depend on
# $(BINDIR), which make cannot see, so a cached copy from an install into a
# different prefix would be silently reused and point at the wrong binary.
install: build
	@mkdir -p $(dir $(BUILT_DESKTOP))
	sed 's|@BIN@|$(BINDIR)/$(BIN)|g' desktop/$(APPID).desktop.in > $(BUILT_DESKTOP)
	@command -v desktop-file-validate >/dev/null && desktop-file-validate $(BUILT_DESKTOP) || true
	install -Dm755 target/release/$(BIN) $(DESTDIR)$(BINDIR)/$(BIN)
	install -Dm644 $(BUILT_DESKTOP) $(DESTDIR)$(DESKTOP_DIR)/$(APPID).desktop
	install -Dm644 desktop/$(APPID).svg $(DESTDIR)$(ICON_DIR)/$(APPID).svg
ifeq ($(DESTDIR),)
	@$(MAKE) --no-print-directory refresh
endif
	@echo "hark installed to $(BINDIR)/$(BIN)"

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/$(BIN)
	rm -f $(DESTDIR)$(DESKTOP_DIR)/$(APPID).desktop
	rm -f $(DESTDIR)$(ICON_DIR)/$(APPID).svg
ifeq ($(DESTDIR),)
	@$(MAKE) --no-print-directory refresh
endif
	@echo "hark uninstalled"

# Both caches are optional: GNOME still finds the launcher and the icon without
# them, so a missing tool is not a failure.
.PHONY: refresh
refresh:
	-@command -v update-desktop-database >/dev/null && \
		update-desktop-database -q $(DESKTOP_DIR) 2>/dev/null
	-@command -v gtk-update-icon-cache >/dev/null && \
		gtk-update-icon-cache -qtf $(DATADIR)/icons/hicolor 2>/dev/null

clean:
	cargo clean
