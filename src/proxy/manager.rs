//! # D-Bus interface proxy for: `com.steampowered.SteamOSManager1.Manager`
//!
//! This code was generated by `zbus-xmlgen` `4.1.0` from D-Bus introspection data.
//! Source: `com.steampowered.SteamOSManager1.Manager.xml`.
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
    interface = "com.steampowered.SteamOSManager1.Manager",
    default_service = "com.steampowered.SteamOSManager1",
    default_path = "/com/steampowered/SteamOSManager1",
    assume_defaults = true
)]
trait Manager {
    /// ReloadConfig method
    fn reload_config(&self) -> zbus::Result<()>;

    /// SetWifiDebugMode method
    fn set_wifi_debug_mode(&self, mode: u32, buffer_size: u32) -> zbus::Result<()>;

    /// GpuPerformanceLevel property
    #[zbus(property)]
    fn gpu_performance_level(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn set_gpu_performance_level(&self, value: u32) -> zbus::Result<()>;

    /// GpuPowerProfile property
    #[zbus(property)]
    fn gpu_power_profile(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn set_gpu_power_profile(&self, value: u32) -> zbus::Result<()>;

    /// GpuPowerProfiles property
    #[zbus(property)]
    fn gpu_power_profiles(&self) -> zbus::Result<std::collections::HashMap<u32, String>>;

    /// HardwareCurrentlySupported property
    #[zbus(property)]
    fn hardware_currently_supported(&self) -> zbus::Result<u32>;

    /// ManualGpuClock property
    #[zbus(property)]
    fn manual_gpu_clock(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn set_manual_gpu_clock(&self, value: u32) -> zbus::Result<()>;

    /// ManualGpuClockMax property
    #[zbus(property)]
    fn manual_gpu_clock_max(&self) -> zbus::Result<u32>;

    /// ManualGpuClockMin property
    #[zbus(property)]
    fn manual_gpu_clock_min(&self) -> zbus::Result<u32>;

    /// TdpLimit property
    #[zbus(property)]
    fn tdp_limit(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn set_tdp_limit(&self, value: u32) -> zbus::Result<()>;

    /// TdpLimitMax property
    #[zbus(property)]
    fn tdp_limit_max(&self) -> zbus::Result<u32>;

    /// TdpLimitMin property
    #[zbus(property)]
    fn tdp_limit_min(&self) -> zbus::Result<u32>;

    /// Version property
    #[zbus(property)]
    fn version(&self) -> zbus::Result<u32>;

    /// WifiBackend property
    #[zbus(property)]
    fn wifi_backend(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn set_wifi_backend(&self, value: u32) -> zbus::Result<()>;

    /// WifiDebugModeState property
    #[zbus(property)]
    fn wifi_debug_mode_state(&self) -> zbus::Result<u32>;
}
