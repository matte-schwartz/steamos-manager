//! # D-Bus interface proxy for: `com.steampowered.SteamOSManager1.BatteryChargeLimit1`
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
    interface = "com.steampowered.SteamOSManager1.BatteryChargeLimit1",
    default_service = "com.steampowered.SteamOSManager1",
    default_path = "/com/steampowered/SteamOSManager1",
    assume_defaults = true
)]
pub trait BatteryChargeLimit1 {
    /// MaxChargeLevel property
    #[zbus(property)]
    fn max_charge_level(&self) -> zbus::Result<i32>;
    #[zbus(property)]
    fn set_max_charge_level(&self, value: i32) -> zbus::Result<()>;

    /// SuggestedMinimumLimit property
    #[zbus(property)]
    fn suggested_minimum_limit(&self) -> zbus::Result<i32>;
}
