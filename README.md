# SteamOS Manager

SteamOS Manager is a system daemon that aims to abstract Steam's interactions
with the operating system. The goal is to have a standardized interface so that
SteamOS specific features in the Steam client, e.g. TDP management, can be
exposed in any linux distro that provides an implementation of this DBus API.

The interface may be fully or partially implemented. The Steam client will
check which which features are available at startup and restrict the settings
it presents to the user based on feature availability.

Some of the features that SteamOS Manager enables include:
- GPU clock management
- TDP management
- BIOS/Dock updates
- Storage device maintenance tasks
- External storage device formatting
  - Steam geneally performs device enumeration via UDisks2, but formatting
	happens via SteamOSManager

For a full list of features please refer to the [interface specification](https://gitlab.steamos.cloud/holo/steamos-manager/-/blob/master/com.steampowered.SteamOSManager1.xml).

Other notable dbus interfaces used by the Steam Client include:
- org.freedesktop.UDisks2
- org.freedesktop.portal.desktop
- org.freedesktop.login1
- org.bluez

# Building

This project is written in Rust, so you will need an implementation for the
device you're using for building. The [Arch wiki
article](https://wiki.archlinux.org/title/rust) has some good guidelines for
this, but this mostly consists of installing `rustup` from the
`rustup` package (if you're on Arch) and running `rustup default stable` to get
an initial toolchain, or just installing the regular `rust` package for a
system-managed installation.

Once you have that and `cargo` is in your path, to build the project you can
use `cargo build`.

# Developing

As far as IDEs go, Visual Studio Code works pretty well for giving errors about
things you are changing and has plugins for vim mode, etc. if you are used to
those keybindings. Most/all IDEs that work with language servers should do that
fine though.

For VS Code, these extensions help: `rust` and `rust-analyzer`.

Before committing code, please run `cargo fmt` to make sure that your code
matches the preferred code style, and `cargo clippy` can help with common
mistakes and idiomatic code.

To perform tests, run `cargo test`. This will compile a test configuration of
the project and run the built-in test suite.

## Interface compatibility notes

SteamOS Manager and the Steam client are normally updated independently of each
other, thus the interface must remain binary compatible across releases.

In general, when making changes to the interface please consider the following:
- Method signatures must not be altered
  - Instead, prefer exposing a new symbol and add a compatibility adapter to
	the previous interface
- Changes in behaviour should be avoided
  - Consider how a change would affect the beta and stable release of the Steam
	client
- Features must have a mechanism to discover if they are available or not
  - E.g. for a feature exposed as a property if the property is not present on
	the bus it means the feature is unsupported.

Note that while SteamOS Manager must never break binary compatibility of
the interface, the Steam client makes no guarantees that older versions of
an interface will be used if available. As a rule of thumb, the client will
always provide full support for the SteamOS Manager interface version available
in the Stable release of SteamOS.

Another pitfall is that while most of the properties do signal when they are
changed by SteamOS Manager itself, several of these properties can also change
out from under SteamOS Manager if something on the system bypasses it. While
this should never be the case if the user doesn't prod at the underlying system
manually, it's something that interface users should be aware of.

## Implementation details

SteamOS Manager is compromised of two daemons: one runs as the logged in user
and exposes a public DBus API on the session bus, and the second daemon runs as
the `root` user. The root daemon exposes a limited DBus API on the system bus
for tasks that require elevated permissions to execute.

The DBus API exposed on the system bus is considered a private implementation
detail of SteamOS Manager and it may be changed at any moment and without
warning. For this reason, we don't provide an XML schema for the system
daemon's interface and clients shouldn't use it directly.

## Extending the API

To extend the API with a new method or property update the XML schema and
extend the user daemon's DBus API, which is implemented in
`src/manager/user.rs`. Make sure to also update the proxy implementation in
`src/proxy.rs` and expose it in steamosctl, in `src/bin/steamosctl.rs`.

If the new functionality requires elevated privileges, instead extend the
system daemon's DBus API in `src/manager/root.rs` with the necessary helpers to
complete the task. However, you should keep as much logic as possible in the
user daemon.
