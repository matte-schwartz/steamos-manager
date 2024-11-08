//! # D-Bus interface proxy for: `com.steampowered.SteamOSManager1.FanControl1`
//!
//! This code was generated by `zbus-xmlgen` `5.0.1` from D-Bus introspection data.
//! Source: `com.steampowered.SteamOSManager1.xml`.
//!
//! You may prefer to adapt it, instead of using it verbatim.
//!
//! More information can be found in the [Writing a client proxy] section of the zbus
//! documentation.
//!
//!
//! [Writing a client proxy]: https://dbus2.github.io/zbus/client.html
//! [D-Bus standard interfaces]: https://dbus.freedesktop.org/doc/dbus-specification.html#standard-interfaces,
use zbus::proxy;
#[proxy(
    interface = "com.steampowered.SteamOSManager1.FanControl1",
    default_service = "com.steampowered.SteamOSManager1",
    default_path = "/com/steampowered/SteamOSManager1",
    assume_defaults = true
)]
pub trait FanControl1 {
    /// FanControlState property
    #[zbus(property)]
    fn fan_control_state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn set_fan_control_state(&self, value: u32) -> zbus::Result<()>;
}
