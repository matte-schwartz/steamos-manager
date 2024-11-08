//! # D-Bus interface proxy for: `com.steampowered.SteamOSManager1.GpuPowerProfile1`
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
    interface = "com.steampowered.SteamOSManager1.GpuPowerProfile1",
    default_service = "com.steampowered.SteamOSManager1",
    default_path = "/com/steampowered/SteamOSManager1",
    assume_defaults = true
)]
pub trait GpuPowerProfile1 {
    /// AvailableGpuPowerProfiles property
    #[zbus(property)]
    fn available_gpu_power_profiles(&self) -> zbus::Result<Vec<String>>;

    /// GpuPowerProfile property
    #[zbus(property)]
    fn gpu_power_profile(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_gpu_power_profile(&self, value: &str) -> zbus::Result<()>;
}
