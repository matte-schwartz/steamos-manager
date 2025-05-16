all: build

.PHONY: build clean test

target/release/steamos-manager: build

target/release/steamosctl: build

build:
	@cargo build -r

clean:
	@cargo clean

test:
	@cargo test

install: target/release/steamos-manager target/release/steamosctl
	install -d -m0755 "$(DESTDIR)/usr/share/dbus-1/services/"
	install -d -m0755 "$(DESTDIR)/usr/share/dbus-1/system-services/"
	install -d -m0755 "$(DESTDIR)/usr/share/dbus-1/system.d/"
	install -d -m0755 "$(DESTDIR)/usr/lib/systemd/system/"
	install -d -m0755 "$(DESTDIR)/usr/lib/systemd/user/gamescope-session.service.wants/"

	install -Ds -m755 "target/release/steamos-manager" "$(DESTDIR)/usr/lib/steamos-manager"
	install -D -m755 "target/release/steamosctl" "$(DESTDIR)/usr/bin/steamosctl"
	install -D -m644 -t "$(DESTDIR)/usr/share/steamos-manager/platforms" "data/platforms/"*
	install -D -m644 LICENSE "$(DESTDIR)/usr/share/licenses/steamos-manager/LICENSE"

	install -m644 "data/system/com.steampowered.SteamOSManager1.service" "$(DESTDIR)/usr/share/dbus-1/system-services/"
	install -m644 "data/system/com.steampowered.SteamOSManager1.conf" "$(DESTDIR)/usr/share/dbus-1/system.d/"
	install -m644 "data/system/steamos-manager.service" "$(DESTDIR)/usr/lib/systemd/system/"

	install -m644 "data/user/com.steampowered.SteamOSManager1.service" "$(DESTDIR)/usr/share/dbus-1/services/"
	install -m644 "data/user/steamos-manager.service" "$(DESTDIR)/usr/lib/systemd/user/"
