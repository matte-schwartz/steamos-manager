/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 * Copyright © 2024 Igalia S.L.
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{Error, Result};
use std::collections::HashMap;
use tokio::sync::mpsc::{Sender, UnboundedSender};
use tokio::sync::oneshot;
use tracing::error;
use zbus::object_server::SignalEmitter;
use zbus::proxy::{Builder, CacheProperties};
use zbus::zvariant::Fd;
use zbus::{fdo, interface, zvariant, Connection, ObjectServer, Proxy};

use crate::cec::{HdmiCecControl, HdmiCecState};
use crate::daemon::user::Command;
use crate::daemon::DaemonCommand;
use crate::error::{to_zbus_error, to_zbus_fdo_error, zbus_to_zbus_fdo};
use crate::hardware::{
    device_type, device_variant, steam_deck_variant, DeviceType, SteamDeckVariant,
};
use crate::job::JobManagerCommand;
use crate::platform::{device_config, platform_config};
use crate::power::{
    get_available_cpu_scaling_governors, get_available_gpu_performance_levels,
    get_available_gpu_power_profiles, get_available_platform_profiles, get_cpu_scaling_governor,
    get_gpu_clocks, get_gpu_clocks_range, get_gpu_performance_level, get_gpu_power_profile,
    get_max_charge_level, get_platform_profile, TdpManagerCommand,
};
use crate::screenreader::{OrcaManager, ScreenReaderMode};
use crate::wifi::{
    get_wifi_backend, get_wifi_power_management_state, list_wifi_interfaces, WifiBackend,
};
use crate::API_VERSION;

pub(crate) const MANAGER_PATH: &str = "/com/steampowered/SteamOSManager1";

macro_rules! method {
    ($self:expr, $method:expr, $($args:expr),+) => {
        $self.proxy
            .call($method, &($($args,)*))
            .await
            .map_err(zbus_to_zbus_fdo)
    };
    ($self:expr, $method:expr) => {
        $self.proxy
            .call($method, &())
            .await
            .map_err(zbus_to_zbus_fdo)
    };
}

macro_rules! job_method {
    ($self:expr, $method:expr, $($args:expr),+) => {
        {
            let (tx, rx) = oneshot::channel();
            $self.job_manager.send(JobManagerCommand::MirrorJob {
                connection: $self.proxy.connection().clone(),
                path: method!($self, $method, $($args),+)?,
                reply: tx,
            }).map_err(to_zbus_fdo_error)?;
            rx.await.map_err(to_zbus_fdo_error)?
        }
    };
    ($self:expr, $method:expr) => {
        {
            let (tx, rx) = oneshot::channel();
            $self.job_manager.send(JobManagerCommand::MirrorJob {
                connection: $self.proxy.connection().clone(),
                path: method!($self, $method)?,
                reply: tx,
            }).map_err(to_zbus_fdo_error)?;
            rx.await.map_err(to_zbus_fdo_error)?
        }
    };
}

macro_rules! getter {
    ($self:expr, $prop:expr) => {
        $self
            .proxy
            .get_property($prop)
            .await
            .map_err(zbus_to_zbus_fdo)
    };
}

macro_rules! setter {
    ($self:expr, $prop:expr, $value:expr) => {
        $self
            .proxy
            .set_property($prop, $value)
            .await
            .map_err(|e| zbus::Error::FDO(Box::new(e)))
    };
}

struct SteamOSManager {
    proxy: Proxy<'static>,
    _job_manager: UnboundedSender<JobManagerCommand>,
}

struct AmbientLightSensor1 {
    proxy: Proxy<'static>,
}

struct BatteryChargeLimit1 {
    proxy: Proxy<'static>,
}

struct CpuScaling1 {
    proxy: Proxy<'static>,
}

struct FactoryReset1 {
    proxy: Proxy<'static>,
}

struct FanControl1 {
    proxy: Proxy<'static>,
}

struct GpuPerformanceLevel1 {
    proxy: Proxy<'static>,
}

struct GpuPowerProfile1 {
    proxy: Proxy<'static>,
}

pub(crate) struct TdpLimit1 {
    manager: UnboundedSender<TdpManagerCommand>,
}

struct HdmiCec1 {
    hdmi_cec: HdmiCecControl<'static>,
}

struct LowPowerMode1 {
    manager: UnboundedSender<TdpManagerCommand>,
}

struct Manager2 {
    proxy: Proxy<'static>,
    channel: Sender<Command>,
}

struct PerformanceProfile1 {
    proxy: Proxy<'static>,
    tdp_limit_manager: Option<UnboundedSender<TdpManagerCommand>>,
}

struct ScreenReader0 {
    screen_reader: OrcaManager<'static>,
}

struct Storage1 {
    proxy: Proxy<'static>,
    job_manager: UnboundedSender<JobManagerCommand>,
}

struct UpdateBios1 {
    proxy: Proxy<'static>,
    job_manager: UnboundedSender<JobManagerCommand>,
}

struct UpdateDock1 {
    proxy: Proxy<'static>,
    job_manager: UnboundedSender<JobManagerCommand>,
}

struct WifiDebug1 {
    proxy: Proxy<'static>,
}

struct WifiDebugDump1 {
    proxy: Proxy<'static>,
}

struct WifiPowerManagement1 {
    proxy: Proxy<'static>,
}

impl SteamOSManager {
    pub async fn new(
        system_conn: Connection,
        proxy: Proxy<'static>,
        job_manager: UnboundedSender<JobManagerCommand>,
    ) -> Result<Self> {
        job_manager.send(JobManagerCommand::MirrorConnection(system_conn))?;
        Ok(SteamOSManager {
            proxy,
            // Hold onto extra sender to make sure the channel isn't dropped
            // early on devices we don't have any interfaces that use job control.
            _job_manager: job_manager,
        })
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.Manager")]
impl SteamOSManager {
    #[zbus(property(emits_changed_signal = "const"))]
    async fn version(&self) -> u32 {
        API_VERSION
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn tdp_limit_min(&self) -> u32 {
        0
    }

    #[zbus(property)]
    async fn wifi_debug_mode_state(&self) -> fdo::Result<u32> {
        getter!(self, "WifiDebugModeState")
    }

    async fn set_wifi_debug_mode(
        &self,
        mode: u32,
        buffer_size: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let _: () = method!(self, "SetWifiDebugMode", mode, buffer_size)?;
        self.wifi_debug_mode_state_changed(&ctx)
            .await
            .map_err(zbus_to_zbus_fdo)?;
        Ok(())
    }

    #[zbus(property)]
    async fn wifi_backend(&self) -> fdo::Result<u32> {
        match get_wifi_backend().await {
            Ok(backend) => Ok(backend as u32),
            Err(e) => Err(to_zbus_fdo_error(e)),
        }
    }

    #[zbus(property)]
    async fn set_wifi_backend(
        &self,
        backend: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let _: () = self.proxy.call("SetWifiBackend", &(backend)).await?;
        self.wifi_backend_changed(&ctx).await
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.AmbientLightSensor1")]
impl AmbientLightSensor1 {
    #[zbus(property(emits_changed_signal = "false"))]
    async fn als_calibration_gain(&self) -> fdo::Result<Vec<f64>> {
        getter!(self, "AlsCalibrationGain")
    }
}

impl BatteryChargeLimit1 {
    const DEFAULT_SUGGESTED_MINIMUM_LIMIT: i32 = 10;
}

#[interface(name = "com.steampowered.SteamOSManager1.BatteryChargeLimit1")]
impl BatteryChargeLimit1 {
    #[zbus(property)]
    async fn max_charge_level(&self) -> fdo::Result<i32> {
        let level = get_max_charge_level().await.map_err(to_zbus_fdo_error)?;
        if level <= 0 {
            Ok(-1)
        } else {
            Ok(level)
        }
    }

    #[zbus(property)]
    async fn set_max_charge_level(
        &self,
        limit: i32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let _: () = self.proxy.call("SetMaxChargeLevel", &(limit)).await?;
        self.max_charge_level_changed(&ctx).await
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn suggested_minimum_limit(&self) -> i32 {
        let Ok(Some(ref config)) = device_config().await else {
            return BatteryChargeLimit1::DEFAULT_SUGGESTED_MINIMUM_LIMIT;
        };
        let Some(ref config) = config.battery_charge_limit else {
            return BatteryChargeLimit1::DEFAULT_SUGGESTED_MINIMUM_LIMIT;
        };
        config
            .suggested_minimum_limit
            .unwrap_or(BatteryChargeLimit1::DEFAULT_SUGGESTED_MINIMUM_LIMIT)
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.CpuScaling1")]
impl CpuScaling1 {
    #[zbus(property(emits_changed_signal = "const"))]
    async fn available_cpu_scaling_governors(&self) -> fdo::Result<Vec<String>> {
        let governors = get_available_cpu_scaling_governors()
            .await
            .map_err(to_zbus_fdo_error)?;
        let mut result = Vec::new();
        for g in governors {
            result.push(g.to_string());
        }
        Ok(result)
    }

    #[zbus(property)]
    async fn cpu_scaling_governor(&self) -> fdo::Result<String> {
        let governor = get_cpu_scaling_governor()
            .await
            .map_err(to_zbus_fdo_error)?;
        Ok(governor.to_string())
    }

    #[zbus(property)]
    async fn set_cpu_scaling_governor(
        &self,
        governor: String,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let _: () = self
            .proxy
            .call("SetCpuScalingGovernor", &(governor))
            .await?;
        self.cpu_scaling_governor_changed(&ctx).await
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.FactoryReset1")]
impl FactoryReset1 {
    async fn prepare_factory_reset(&self, flags: u32) -> fdo::Result<u32> {
        method!(self, "PrepareFactoryReset", flags)
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.FanControl1")]
impl FanControl1 {
    #[zbus(property)]
    async fn fan_control_state(&self) -> fdo::Result<u32> {
        getter!(self, "FanControlState")
    }

    #[zbus(property)]
    async fn set_fan_control_state(
        &self,
        state: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let _: () = setter!(self, "FanControlState", state)?;
        self.fan_control_state_changed(&ctx).await
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.GpuPerformanceLevel1")]
impl GpuPerformanceLevel1 {
    #[zbus(property(emits_changed_signal = "const"))]
    async fn available_gpu_performance_levels(&self) -> fdo::Result<Vec<String>> {
        get_available_gpu_performance_levels()
            .await
            .inspect_err(|message| error!("Error getting GPU performance levels: {message}"))
            .map(|levels| levels.into_iter().map(|level| level.to_string()).collect())
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn gpu_performance_level(&self) -> fdo::Result<String> {
        match get_gpu_performance_level().await {
            Ok(level) => Ok(level.to_string()),
            Err(e) => {
                error!("Error getting GPU performance level: {e}");
                Err(to_zbus_fdo_error(e))
            }
        }
    }

    #[zbus(property)]
    async fn set_gpu_performance_level(
        &self,
        level: &str,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let _: () = self.proxy.call("SetGpuPerformanceLevel", &(level)).await?;
        self.gpu_performance_level_changed(&ctx).await
    }

    #[zbus(property)]
    async fn manual_gpu_clock(&self) -> fdo::Result<u32> {
        get_gpu_clocks()
            .await
            .inspect_err(|message| error!("Error getting manual GPU clock: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn set_manual_gpu_clock(
        &self,
        clocks: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let _: () = self.proxy.call("SetManualGpuClock", &(clocks)).await?;
        self.manual_gpu_clock_changed(&ctx).await
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn manual_gpu_clock_min(&self) -> fdo::Result<u32> {
        Ok(*get_gpu_clocks_range()
            .await
            .map_err(to_zbus_fdo_error)?
            .start())
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn manual_gpu_clock_max(&self) -> fdo::Result<u32> {
        Ok(*get_gpu_clocks_range()
            .await
            .map_err(to_zbus_fdo_error)?
            .end())
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.GpuPowerProfile1")]
impl GpuPowerProfile1 {
    #[zbus(property(emits_changed_signal = "const"))]
    async fn available_gpu_power_profiles(&self) -> fdo::Result<Vec<String>> {
        let (_, names): (Vec<u32>, Vec<String>) = get_available_gpu_power_profiles()
            .await
            .map_err(to_zbus_fdo_error)?
            .into_iter()
            .unzip();
        Ok(names)
    }

    #[zbus(property)]
    async fn gpu_power_profile(&self) -> fdo::Result<String> {
        match get_gpu_power_profile().await {
            Ok(profile) => Ok(profile.to_string()),
            Err(e) => {
                error!("Error getting GPU power profile: {e}");
                Err(to_zbus_fdo_error(e))
            }
        }
    }

    #[zbus(property)]
    async fn set_gpu_power_profile(
        &self,
        profile: &str,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let _: () = self.proxy.call("SetGpuPowerProfile", &(profile)).await?;
        self.gpu_power_profile_changed(&ctx).await
    }
}

impl HdmiCec1 {
    async fn new(connection: &Connection) -> Result<HdmiCec1> {
        let hdmi_cec = HdmiCecControl::new(connection).await?;
        Ok(HdmiCec1 { hdmi_cec })
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.HdmiCec1")]
impl HdmiCec1 {
    #[zbus(property)]
    async fn hdmi_cec_state(&self) -> fdo::Result<u32> {
        match self.hdmi_cec.get_enabled_state().await {
            Ok(state) => Ok(state as u32),
            Err(e) => Err(to_zbus_fdo_error(e)),
        }
    }

    #[zbus(property)]
    async fn set_hdmi_cec_state(
        &self,
        state: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let state = match HdmiCecState::try_from(state) {
            Ok(state) => state,
            Err(err) => return Err(fdo::Error::InvalidArgs(err.to_string()).into()),
        };
        let _: () = self
            .hdmi_cec
            .set_enabled_state(state)
            .await
            .inspect_err(|message| error!("Error setting CEC state: {message}"))
            .map_err(to_zbus_error)?;
        self.hdmi_cec_state_changed(&ctx).await
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.LowPowerMode1")]
impl LowPowerMode1 {
    async fn enter_download_mode(&self, identifier: &str) -> fdo::Result<Fd> {
        let (tx, rx) = oneshot::channel();
        self.manager
            .send(TdpManagerCommand::EnterDownloadMode(
                identifier.to_string(),
                tx,
            ))
            .map_err(|_| {
                fdo::Error::Failed(String::from("Failed to obtain download mode handle"))
            })?;
        Ok(rx
            .await
            .map_err(to_zbus_fdo_error)?
            .map_err(to_zbus_fdo_error)?
            .ok_or(fdo::Error::Failed(String::from(
                "Download mode not configured",
            )))?
            .into())
    }

    async fn list_download_mode_handles(&self) -> fdo::Result<HashMap<String, u32>> {
        let (tx, rx) = oneshot::channel();
        self.manager
            .send(TdpManagerCommand::ListDownloadModeHandles(tx))
            .map_err(|_| {
                fdo::Error::Failed(String::from("Failed to obtain download mode handle list"))
            })?;
        rx.await.map_err(to_zbus_fdo_error)
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.Manager2")]
impl Manager2 {
    async fn reload_config(&self) -> fdo::Result<()> {
        self.channel
            .send(DaemonCommand::ReadConfig)
            .await
            .inspect_err(|message| error!("Error sending ReadConfig command: {message}"))
            .map_err(to_zbus_fdo_error)?;
        method!(self, "ReloadConfig")
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn device_model(&self) -> fdo::Result<(String, String)> {
        let (device, variant) = device_variant().await.map_err(to_zbus_fdo_error)?;
        Ok((device.to_string(), variant))
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.PerformanceProfile1")]
impl PerformanceProfile1 {
    #[zbus(property(emits_changed_signal = "const"))]
    async fn available_performance_profiles(&self) -> fdo::Result<Vec<String>> {
        let config = device_config().await.map_err(to_zbus_fdo_error)?;
        let config = config
            .as_ref()
            .and_then(|config| config.performance_profile.as_ref())
            .ok_or(fdo::Error::Failed(String::from(
                "No performance platform-profile configured",
            )))?;
        get_available_platform_profiles(&config.platform_profile_name)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn performance_profile(&self) -> fdo::Result<String> {
        let config = device_config().await.map_err(to_zbus_fdo_error)?;
        let config = config
            .as_ref()
            .and_then(|config| config.performance_profile.as_ref())
            .ok_or(fdo::Error::Failed(String::from(
                "No performance platform-profile configured",
            )))?;
        get_platform_profile(&config.platform_profile_name)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn set_performance_profile(
        &self,
        profile: &str,
        #[zbus(connection)] connection: &Connection,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let _: () = self.proxy.call("SetPerformanceProfile", &(profile)).await?;
        self.performance_profile_changed(&ctx).await?;
        let connection = connection.clone();
        if let Some(manager) = self.tdp_limit_manager.as_ref() {
            let manager = manager.clone();
            let _ = manager.send(TdpManagerCommand::UpdateDownloadMode);
            tokio::spawn(async move {
                let (tx, rx) = oneshot::channel();
                manager.send(TdpManagerCommand::IsActive(tx))?;
                if rx.await?? {
                    let tdp_limit = TdpLimit1 { manager };
                    connection
                        .object_server()
                        .at(MANAGER_PATH, tdp_limit)
                        .await?;
                } else {
                    connection
                        .object_server()
                        .remove::<TdpLimit1, _>(MANAGER_PATH)
                        .await?;
                }
                Ok::<(), Error>(())
            });
        }
        Ok(())
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn suggested_default_performance_profile(&self) -> fdo::Result<String> {
        let config = device_config().await.map_err(to_zbus_fdo_error)?;
        let config = config
            .as_ref()
            .and_then(|config| config.performance_profile.as_ref())
            .ok_or(fdo::Error::Failed(String::from(
                "No performance platform-profile configured",
            )))?;
        Ok(config.suggested_default.to_string())
    }
}

impl ScreenReader0 {
    async fn new(connection: &Connection) -> Result<ScreenReader0> {
        let screen_reader = OrcaManager::new(connection).await?;
        Ok(ScreenReader0 { screen_reader })
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.ScreenReader0")]
impl ScreenReader0 {
    #[zbus(property)]
    async fn enabled(&self) -> bool {
        self.screen_reader.enabled()
    }

    #[zbus(property)]
    async fn set_enabled(&mut self, enabled: bool) -> fdo::Result<()> {
        self.screen_reader
            .set_enabled(enabled)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn rate(&self) -> f64 {
        self.screen_reader.rate()
    }

    #[zbus(property)]
    async fn set_rate(&mut self, rate: f64) -> fdo::Result<()> {
        self.screen_reader
            .set_rate(rate)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn pitch(&self) -> f64 {
        self.screen_reader.pitch()
    }

    #[zbus(property)]
    async fn set_pitch(&mut self, pitch: f64) -> fdo::Result<()> {
        self.screen_reader
            .set_pitch(pitch)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn volume(&self) -> f64 {
        self.screen_reader.volume()
    }

    #[zbus(property)]
    async fn set_volume(&mut self, volume: f64) -> fdo::Result<()> {
        self.screen_reader
            .set_volume(volume)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn mode(&self) -> u32 {
        self.screen_reader.mode() as u32
    }

    #[zbus(property)]
    async fn set_mode(
        &mut self,
        m: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let mode = match ScreenReaderMode::try_from(m) {
            Ok(mode) => mode,
            Err(err) => return Err(fdo::Error::InvalidArgs(err.to_string())),
        };
        self.screen_reader
            .set_mode(mode)
            .await
            .map_err(to_zbus_fdo_error)?;
        self.mode_changed(&ctx).await.map_err(to_zbus_fdo_error)
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.Storage1")]
impl Storage1 {
    async fn format_device(
        &mut self,
        device: &str,
        label: &str,
        validate: bool,
    ) -> fdo::Result<zvariant::OwnedObjectPath> {
        job_method!(self, "FormatDevice", device, label, validate)
    }

    async fn trim_devices(&mut self) -> fdo::Result<zvariant::OwnedObjectPath> {
        job_method!(self, "TrimDevices")
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.TdpLimit1")]
impl TdpLimit1 {
    #[zbus(property)]
    async fn tdp_limit(&self) -> u32 {
        let (tx, rx) = oneshot::channel();
        if self
            .manager
            .send(TdpManagerCommand::GetTdpLimit(tx))
            .is_err()
        {
            return 0;
        }
        rx.await.unwrap_or(Ok(0)).unwrap_or(0)
    }

    #[zbus(property)]
    async fn set_tdp_limit(&self, limit: u32) -> zbus::Result<()> {
        self.manager
            .send(TdpManagerCommand::SetTdpLimit(limit))
            .map_err(|_| zbus::Error::Failure(String::from("Failed to set TDP limit")))
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn tdp_limit_min(&self) -> u32 {
        let (tx, rx) = oneshot::channel();
        if self
            .manager
            .send(TdpManagerCommand::GetTdpLimitRange(tx))
            .is_err()
        {
            return 0;
        }
        if let Ok(range) = rx.await {
            range.map(|r| *r.start()).unwrap_or(0)
        } else {
            0
        }
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn tdp_limit_max(&self) -> u32 {
        let (tx, rx) = oneshot::channel();
        if self
            .manager
            .send(TdpManagerCommand::GetTdpLimitRange(tx))
            .is_err()
        {
            return 0;
        }
        if let Ok(range) = rx.await {
            range.map(|r| *r.end()).unwrap_or(0)
        } else {
            0
        }
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.UpdateBios1")]
impl UpdateBios1 {
    async fn update_bios(&mut self) -> fdo::Result<zvariant::OwnedObjectPath> {
        job_method!(self, "UpdateBios")
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.UpdateDock1")]
impl UpdateDock1 {
    async fn update_dock(&mut self) -> fdo::Result<zvariant::OwnedObjectPath> {
        job_method!(self, "UpdateDock")
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.WifiDebug1")]
impl WifiDebug1 {
    #[zbus(property)]
    async fn wifi_debug_mode_state(&self) -> fdo::Result<u32> {
        getter!(self, "WifiDebugModeState")
    }

    async fn set_wifi_debug_mode(
        &self,
        mode: u32,
        options: HashMap<&str, zvariant::Value<'_>>,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let _: () = method!(self, "SetWifiDebugMode", mode, options)?;
        self.wifi_debug_mode_state_changed(&ctx)
            .await
            .map_err(zbus_to_zbus_fdo)?;
        Ok(())
    }

    #[zbus(property)]
    async fn wifi_backend(&self) -> fdo::Result<String> {
        match get_wifi_backend().await {
            Ok(backend) => Ok(backend.to_string()),
            Err(e) => Err(to_zbus_fdo_error(e)),
        }
    }

    #[zbus(property)]
    async fn set_wifi_backend(
        &self,
        backend: &str,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let backend = match WifiBackend::try_from(backend) {
            Ok(backend) => backend,
            Err(e) => return Err(fdo::Error::InvalidArgs(e.to_string()).into()),
        };
        let _: () = self.proxy.call("SetWifiBackend", &(backend as u32)).await?;
        self.wifi_backend_changed(&ctx).await
    }

    async fn capture_debug_trace_output(&self) -> fdo::Result<String> {
        method!(self, "CaptureDebugTraceOutput")
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.WifiDebugDump1")]
impl WifiDebugDump1 {
    async fn generate_debug_dump(&self) -> fdo::Result<String> {
        method!(self, "GenerateDebugDump")
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.WifiPowerManagement1")]
impl WifiPowerManagement1 {
    #[zbus(property)]
    async fn wifi_power_management_state(&self) -> fdo::Result<u32> {
        match get_wifi_power_management_state().await {
            Ok(state) => Ok(state as u32),
            Err(e) => Err(to_zbus_fdo_error(e)),
        }
    }

    #[zbus(property)]
    async fn set_wifi_power_management_state(
        &self,
        state: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let _: () = self
            .proxy
            .call("SetWifiPowerManagementState", &(state))
            .await?;
        self.wifi_power_management_state_changed(&ctx).await
    }
}

async fn create_platform_interfaces(
    proxy: &Proxy<'static>,
    object_server: &ObjectServer,
    connection: &Connection,
    job_manager: &UnboundedSender<JobManagerCommand>,
) -> Result<()> {
    let Some(config) = platform_config().await? else {
        return Ok(());
    };

    let factory_reset = FactoryReset1 {
        proxy: proxy.clone(),
    };
    let fan_control = FanControl1 {
        proxy: proxy.clone(),
    };
    let storage = Storage1 {
        proxy: proxy.clone(),
        job_manager: job_manager.clone(),
    };
    let update_bios = UpdateBios1 {
        proxy: proxy.clone(),
        job_manager: job_manager.clone(),
    };
    let update_dock = UpdateDock1 {
        proxy: proxy.clone(),
        job_manager: job_manager.clone(),
    };

    if let Some(config) = config.factory_reset.as_ref() {
        match config.is_valid(true).await {
            Ok(true) => {
                object_server.at(MANAGER_PATH, factory_reset).await?;
            }
            Ok(false) => (),
            Err(e) => error!("Failed to verify if factory reset config is valid: {e}"),
        }
    }

    if let Some(config) = config.fan_control.as_ref() {
        match config.is_valid(connection, true).await {
            Ok(true) => {
                object_server.at(MANAGER_PATH, fan_control).await?;
            }
            Ok(false) => (),
            Err(e) => error!("Failed to verify if fan control config is valid: {e}"),
        }
    }

    if let Some(config) = config.storage.as_ref() {
        match config.is_valid(true).await {
            Ok(true) => {
                object_server.at(MANAGER_PATH, storage).await?;
            }
            Ok(false) => (),
            Err(e) => error!("Failed to verify if storage config is valid: {e}"),
        }
    }

    if let Some(config) = config.update_bios.as_ref() {
        match config.is_valid(true).await {
            Ok(true) => {
                object_server.at(MANAGER_PATH, update_bios).await?;
            }
            Ok(false) => (),
            Err(e) => error!("Failed to verify if BIOS update config is valid: {e}"),
        }
    }

    if let Some(config) = config.update_dock.as_ref() {
        match config.is_valid(true).await {
            Ok(true) => {
                object_server.at(MANAGER_PATH, update_dock).await?;
            }
            Ok(false) => (),
            Err(e) => error!("Failed to verify if dock update config is valid: {e}"),
        }
    }

    Ok(())
}

async fn create_device_interfaces(
    proxy: &Proxy<'static>,
    object_server: &ObjectServer,
    tdp_manager: Option<UnboundedSender<TdpManagerCommand>>,
) -> Result<()> {
    let Some(config) = device_config().await? else {
        return Ok(());
    };

    let performance_profile = PerformanceProfile1 {
        proxy: proxy.clone(),
        tdp_limit_manager: tdp_manager.clone(),
    };

    if let Some(manager) = tdp_manager {
        let low_power_mode = LowPowerMode1 {
            manager: manager.clone(),
        };
        if config
            .tdp_limit
            .as_ref()
            .and_then(|config| config.download_mode_limit)
            .is_some()
        {
            object_server.at(MANAGER_PATH, low_power_mode).await?;
        }

        let object_server = object_server.clone();
        tokio::spawn(async move {
            let (tx, rx) = oneshot::channel();
            manager.send(TdpManagerCommand::IsActive(tx))?;
            if rx.await?? {
                let tdp_limit = TdpLimit1 { manager };
                object_server.at(MANAGER_PATH, tdp_limit).await?;
            }
            Ok::<(), Error>(())
        });
    }

    if let Some(config) = config.performance_profile.as_ref() {
        if !get_available_platform_profiles(&config.platform_profile_name)
            .await
            .unwrap_or_default()
            .is_empty()
        {
            object_server.at(MANAGER_PATH, performance_profile).await?;
        }
    }

    Ok(())
}

pub(crate) async fn create_interfaces(
    session: Connection,
    system: Connection,
    daemon: Sender<Command>,
    job_manager: UnboundedSender<JobManagerCommand>,
    tdp_manager: Option<UnboundedSender<TdpManagerCommand>>,
) -> Result<()> {
    let proxy = Builder::<Proxy>::new(&system)
        .destination("com.steampowered.SteamOSManager1")?
        .path("/com/steampowered/SteamOSManager1")?
        .interface("com.steampowered.SteamOSManager1.RootManager")?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;

    let manager = SteamOSManager::new(system.clone(), proxy.clone(), job_manager.clone()).await?;

    let als = AmbientLightSensor1 {
        proxy: proxy.clone(),
    };
    let battery_charge_limit = BatteryChargeLimit1 {
        proxy: proxy.clone(),
    };
    let cpu_scaling = CpuScaling1 {
        proxy: proxy.clone(),
    };
    let gpu_performance_level = GpuPerformanceLevel1 {
        proxy: proxy.clone(),
    };
    let gpu_power_profile = GpuPowerProfile1 {
        proxy: proxy.clone(),
    };
    let hdmi_cec = HdmiCec1::new(&session).await?;
    let manager2 = Manager2 {
        proxy: proxy.clone(),
        channel: daemon,
    };
    let screen_reader = ScreenReader0::new(&session).await?;
    let wifi_debug = WifiDebug1 {
        proxy: proxy.clone(),
    };
    let wifi_debug_dump = WifiDebugDump1 {
        proxy: proxy.clone(),
    };
    let wifi_power_management = WifiPowerManagement1 {
        proxy: proxy.clone(),
    };

    let object_server = session.object_server();
    object_server.at(MANAGER_PATH, manager).await?;

    create_device_interfaces(&proxy, object_server, tdp_manager).await?;
    create_platform_interfaces(&proxy, object_server, &system, &job_manager).await?;

    if device_type().await.unwrap_or_default() == DeviceType::SteamDeck {
        object_server.at(MANAGER_PATH, als).await?;
    }
    if steam_deck_variant().await.unwrap_or_default() == SteamDeckVariant::Galileo {
        object_server.at(MANAGER_PATH, wifi_debug_dump).await?;
    }

    if get_max_charge_level().await.is_ok() {
        object_server.at(MANAGER_PATH, battery_charge_limit).await?;
    }

    object_server.at(MANAGER_PATH, cpu_scaling).await?;

    if !get_available_gpu_performance_levels()
        .await
        .unwrap_or_default()
        .is_empty()
    {
        object_server
            .at(MANAGER_PATH, gpu_performance_level)
            .await?;
    }

    if !get_available_gpu_power_profiles()
        .await
        .unwrap_or_default()
        .is_empty()
    {
        object_server.at(MANAGER_PATH, gpu_power_profile).await?;
    }

    if hdmi_cec.hdmi_cec.get_enabled_state().await.is_ok() {
        object_server.at(MANAGER_PATH, hdmi_cec).await?;
    }

    object_server.at(MANAGER_PATH, manager2).await?;

    object_server.at(MANAGER_PATH, screen_reader).await?;

    if steam_deck_variant().await.unwrap_or_default() == SteamDeckVariant::Galileo {
        object_server.at(MANAGER_PATH, wifi_debug).await?;
    }

    if !list_wifi_interfaces().await.unwrap_or_default().is_empty() {
        object_server
            .at(MANAGER_PATH, wifi_power_management)
            .await?;
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::daemon::channel;
    use crate::daemon::user::UserContext;
    use crate::hardware::test::fake_model;
    use crate::hardware::SteamDeckVariant;
    use crate::platform::{
        BatteryChargeLimitConfig, DeviceConfig, FormatDeviceConfig, PerformanceProfileConfig,
        PlatformConfig, RangeConfig, ResetConfig, ScriptConfig, ServiceConfig, StorageConfig,
        TdpLimitConfig,
    };
    use crate::power::TdpLimitingMethod;
    use crate::systemd::test::{MockManager, MockUnit};
    use crate::{path, power, testing};

    use std::num::NonZeroU32;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::fs::{set_permissions, write};
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
    use tokio::time::sleep;
    use zbus::object_server::Interface;
    use zbus::Connection;

    struct TestHandle {
        _handle: testing::TestHandle,
        connection: Connection,
        _rx_job: UnboundedReceiver<JobManagerCommand>,
        rx_tdp: Option<UnboundedReceiver<TdpManagerCommand>>,
    }

    fn all_platform_config() -> Option<PlatformConfig> {
        Some(PlatformConfig {
            factory_reset: Some(ResetConfig::default()),
            update_bios: Some(ScriptConfig::default()),
            update_dock: Some(ScriptConfig::default()),
            storage: Some(StorageConfig::default()),
            fan_control: Some(ServiceConfig::Systemd(String::from(
                "jupiter-fan-control.service",
            ))),
        })
    }

    fn all_device_config() -> Option<DeviceConfig> {
        Some(DeviceConfig {
            tdp_limit: Some(TdpLimitConfig {
                method: TdpLimitingMethod::GpuHwmon,
                range: Some(RangeConfig::new(3, 15)),
                download_mode_limit: NonZeroU32::new(6),
                firmware_attribute: None,
            }),
            gpu_clocks: Some(RangeConfig::new(200, 1600)),
            battery_charge_limit: Some(BatteryChargeLimitConfig {
                suggested_minimum_limit: Some(10),
                hwmon_name: String::from("steamdeck_hwmon"),
                attribute: String::from("max_battery_charge_level"),
            }),
            performance_profile: Some(PerformanceProfileConfig {
                platform_profile_name: String::from("power-driver"),
                suggested_default: String::from("balanced"),
            }),
        })
    }

    async fn start(
        mut platform_config: Option<PlatformConfig>,
        device_config: Option<DeviceConfig>,
    ) -> Result<TestHandle> {
        let mut handle = testing::start();
        let (tx_ctx, _rx_ctx) = channel::<UserContext>();
        let (tx_job, rx_job) = unbounded_channel::<JobManagerCommand>();
        let (tx_tdp, rx_tdp) = {
            if device_config
                .as_ref()
                .and_then(|config| config.tdp_limit.as_ref())
                .is_some()
            {
                let (tx_tdp, rx_tdp) = unbounded_channel::<TdpManagerCommand>();
                (Some(tx_tdp), Some(rx_tdp))
            } else {
                (None, None)
            }
        };

        if let Some(ref mut config) = platform_config {
            config.set_test_paths();
        }

        handle.test.platform_config.replace(platform_config);
        handle.test.device_config.replace(device_config);
        let connection = handle.new_dbus().await?;
        connection.request_name("org.freedesktop.systemd1").await?;
        sleep(Duration::from_millis(10)).await;
        {
            let object_server = connection.object_server();
            object_server
                .at("/org/freedesktop/systemd1", MockManager::default())
                .await?;

            let mut prc = MockUnit::default();
            prc.unit_file = String::from("disabled");
            object_server
                .at(
                    "/org/freedesktop/systemd1/unit/plasma_2dremotecontrollers_2eservice",
                    prc,
                )
                .await?;
        }

        let exe_path = path("exe");
        write(&exe_path, "").await?;
        set_permissions(&exe_path, PermissionsExt::from_mode(0o700)).await?;

        fake_model(SteamDeckVariant::Galileo).await?;
        handle
            .test
            .process_cb
            .set(|_, _| Ok((0, String::from("Interface wlan0"))));
        power::test::create_nodes().await?;
        create_interfaces(
            connection.clone(),
            connection.clone(),
            tx_ctx,
            tx_job,
            tx_tdp,
        )
        .await?;

        sleep(Duration::from_millis(1)).await;

        Ok(TestHandle {
            _handle: handle,
            connection,
            _rx_job: rx_job,
            rx_tdp,
        })
    }

    #[tokio::test]
    async fn interface_matches() {
        let test = start(None, None).await.expect("start");

        let remote = testing::InterfaceIntrospection::from_remote::<SteamOSManager, _>(
            &test.connection,
            MANAGER_PATH,
        )
        .await
        .expect("remote");
        let local = testing::InterfaceIntrospection::from_local(
            "com.steampowered.SteamOSManager1.Manager.xml",
            "com.steampowered.SteamOSManager1.Manager",
        )
        .await
        .expect("local");
        assert!(remote.compare(&local));
    }

    async fn test_interface_matches<I: Interface>(connection: &Connection) -> Result<bool> {
        let remote =
            testing::InterfaceIntrospection::from_remote::<I, _>(connection, MANAGER_PATH).await?;
        let local = testing::InterfaceIntrospection::from_local(
            "com.steampowered.SteamOSManager1.xml",
            I::name().to_string(),
        )
        .await?;
        Ok(remote.compare(&local))
    }

    async fn test_interface_missing<I: Interface>(connection: &Connection) -> bool {
        let remote =
            testing::InterfaceIntrospection::from_remote::<I, _>(connection, MANAGER_PATH).await;
        remote.is_err()
    }

    #[tokio::test]
    async fn interface_matches_ambient_light_sensor1() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(
            test_interface_matches::<AmbientLightSensor1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_matches_battery_charge_limit() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(
            test_interface_matches::<BatteryChargeLimit1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_matches_cpu_scaling1() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_matches::<CpuScaling1>(&test.connection)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn interface_matches_factory_reset1() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_matches::<FactoryReset1>(&test.connection)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn interface_missing_factory_reset1() {
        let test = start(None, None).await.expect("start");

        assert!(test_interface_missing::<FactoryReset1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_missing_invalid_all_factory_reset1() {
        let mut config = all_platform_config().unwrap();
        config.factory_reset.as_mut().unwrap().all = ScriptConfig {
            script: PathBuf::from("oxo"),
            script_args: Vec::new(),
        };
        let test = start(Some(config), None).await.expect("start");

        assert!(test_interface_missing::<FactoryReset1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_missing_invalid_os_factory_reset1() {
        let mut config = all_platform_config().unwrap();
        config.factory_reset.as_mut().unwrap().os = ScriptConfig {
            script: PathBuf::from("oxo"),
            script_args: Vec::new(),
        };
        let test = start(Some(config), None).await.expect("start");

        assert!(test_interface_missing::<FactoryReset1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_missing_invalid_user_factory_reset1() {
        let mut config = all_platform_config().unwrap();
        config.factory_reset.as_mut().unwrap().user = ScriptConfig {
            script: PathBuf::from("oxo"),
            script_args: Vec::new(),
        };
        let test = start(Some(config), None).await.expect("start");

        assert!(test_interface_missing::<FactoryReset1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_fan_control1() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_matches::<FanControl1>(&test.connection)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn interface_missing_fan_control1() {
        let test = start(None, None).await.expect("start");

        assert!(test_interface_missing::<FanControl1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_gpu_performance_level1() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(
            test_interface_matches::<GpuPerformanceLevel1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_matches_gpu_power_profile1() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_matches::<GpuPowerProfile1>(&test.connection)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn interface_matches_tdp_limit1() {
        let mut test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        let TdpManagerCommand::IsActive(reply) =
            test.rx_tdp.as_mut().unwrap().recv().await.unwrap()
        else {
            panic!();
        };
        reply.send(Ok(true)).unwrap();
        sleep(Duration::from_millis(1)).await;

        assert!(test_interface_matches::<TdpLimit1>(&test.connection)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn interface_missing_tdp_limit1() {
        let test = start(None, None).await.expect("start");

        assert!(test_interface_missing::<TdpLimit1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_inactive_tdp_limit1() {
        let mut test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        let TdpManagerCommand::IsActive(reply) =
            test.rx_tdp.as_mut().unwrap().recv().await.unwrap()
        else {
            panic!();
        };
        reply.send(Ok(false)).unwrap();
        sleep(Duration::from_millis(1)).await;

        assert!(test_interface_missing::<TdpLimit1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_hdmi_cec1() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_matches::<HdmiCec1>(&test.connection)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn interface_matches_low_power_mode1() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_matches::<LowPowerMode1>(&test.connection)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn interface_missing_low_power_mode1() {
        let test = start(None, None).await.expect("start");

        assert!(test_interface_missing::<LowPowerMode1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_manager2() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_matches::<Manager2>(&test.connection)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn interface_matches_performance_profile1() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(
            test_interface_matches::<PerformanceProfile1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_performance_profile1() {
        let test = start(None, None).await.expect("start");

        assert!(test_interface_missing::<PerformanceProfile1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_storage1() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_matches::<Storage1>(&test.connection)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn interface_missing_storage1() {
        let test = start(None, None).await.expect("start");

        assert!(test_interface_missing::<Storage1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_missing_invalid_trim_storage1() {
        let mut config = all_platform_config().unwrap();
        config.storage.as_mut().unwrap().trim_devices = ScriptConfig {
            script: PathBuf::from("oxo"),
            script_args: Vec::new(),
        };
        let test = start(Some(config), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_missing::<Storage1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_missing_invalid_format_storage1() {
        let mut config = all_platform_config().unwrap();
        let mut format_config = FormatDeviceConfig::default();
        format_config.script = PathBuf::from("oxo");
        config.storage.as_mut().unwrap().format_device = format_config;
        let test = start(Some(config), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_missing::<Storage1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_update_bios1() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_matches::<UpdateBios1>(&test.connection)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn interface_missing_update_bios1() {
        let test = start(None, None).await.expect("start");

        assert!(test_interface_missing::<UpdateBios1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_missing_invalid_update_bios1() {
        let mut config = all_platform_config().unwrap();
        config.update_bios = Some(ScriptConfig {
            script: PathBuf::from("oxo"),
            script_args: Vec::new(),
        });
        let test = start(Some(config), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_missing::<UpdateBios1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_update_dock1() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_matches::<UpdateDock1>(&test.connection)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn interface_missing_update_dock1() {
        let test = start(None, None).await.expect("start");

        assert!(test_interface_missing::<UpdateDock1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_missing_invalid_update_dock1() {
        let mut config = all_platform_config().unwrap();
        config.update_dock = Some(ScriptConfig {
            script: PathBuf::from("oxo"),
            script_args: Vec::new(),
        });
        let test = start(Some(config), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_missing::<UpdateDock1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_wifi_power_management1() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(
            test_interface_matches::<WifiPowerManagement1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_matches_wifi_debug() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_matches::<WifiDebug1>(&test.connection)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn interface_matches_wifi_debug_dump() {
        let test = start(all_platform_config(), all_device_config())
            .await
            .expect("start");

        assert!(test_interface_matches::<WifiDebugDump1>(&test.connection)
            .await
            .unwrap());
    }
}
