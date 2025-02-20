/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 * Copyright © 2024 Igalia S.L.
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::ffi::OsStr;
use tokio::fs::File;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;
use tracing::{error, info};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{self, Fd};
use zbus::{fdo, interface, Connection};

use crate::daemon::root::{Command, RootCommand};
use crate::daemon::DaemonCommand;
use crate::error::{to_zbus_error, to_zbus_fdo_error};
use crate::hardware::{
    steam_deck_variant, FactoryResetKind, FanControl, FanControlState, SteamDeckVariant,
};
use crate::job::JobManager;
use crate::platform::platform_config;
use crate::power::{
    set_cpu_scaling_governor, set_gpu_clocks, set_gpu_performance_level, set_gpu_power_profile,
    set_max_charge_level, set_tdp_limit, CPUScalingGovernor, GPUPerformanceLevel, GPUPowerProfile,
};
use crate::process::{run_script, script_output};
use crate::wifi::{
    extract_wifi_trace, generate_wifi_dump, set_wifi_backend, set_wifi_debug_mode,
    set_wifi_power_management_state, WifiBackend, WifiDebugMode, WifiPowerManagement,
};
use crate::{path, API_VERSION};

macro_rules! with_platform_config {
    ($config:ident = $field:ident ($name:literal) => $eval:expr) => {
        if let Some(config) = platform_config()
            .await
            .map_err(to_zbus_fdo_error)?
            .as_ref()
            .and_then(|config| config.$field.as_ref())
        {
            let $config = config;
            $eval
        } else {
            Err(fdo::Error::NotSupported(format!(
                "{} is not supported on this platform",
                $name
            )))
        }
    };
}

#[derive(PartialEq, Debug, Copy, Clone)]
#[repr(u32)]
enum PrepareFactoryResetResult {
    // NOTE: both old PrepareFactoryReset and new PrepareFactoryResetExt use these
    // result values.
    Unknown = 0,
    RebootRequired = 1,
}

pub struct SteamOSManager {
    connection: Connection,
    channel: Sender<Command>,
    wifi_debug_mode: WifiDebugMode,
    fan_control: FanControl,
    // Whether we should use trace-cmd or not.
    // True on galileo devices, false otherwise
    should_trace: bool,
    job_manager: JobManager,
}

impl SteamOSManager {
    pub async fn new(connection: Connection, channel: Sender<Command>) -> Result<Self> {
        Ok(SteamOSManager {
            fan_control: FanControl::new(connection.clone()),
            wifi_debug_mode: WifiDebugMode::Off,
            should_trace: steam_deck_variant().await? == SteamDeckVariant::Galileo,
            job_manager: JobManager::new(connection.clone()).await?,
            connection,
            channel,
        })
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.RootManager")]
impl SteamOSManager {
    async fn prepare_factory_reset(&self, kind: u32) -> fdo::Result<u32> {
        // Run steamos-reset with arguments based on flags passed and return 1 on success
        with_platform_config! {
            config = factory_reset("PrepareFactoryReset") => {
                let res =
                match FactoryResetKind::try_from(kind) {
                    Ok(FactoryResetKind::User) => {
                        run_script(&config.user.script, &config.user.script_args).await
                    },
                    Ok(FactoryResetKind::OS) => {
                        run_script(&config.os.script, &config.os.script_args).await
                    },
                    Ok(FactoryResetKind::All) => {
                        run_script(&config.all.script, &config.all.script_args).await
                    },
                    Err(_) => {
                        Err(anyhow!("Unable to generate command arguments for steamos-reset-tool script"))
                    }
                };
                Ok(match res {
                    Ok(()) => PrepareFactoryResetResult::RebootRequired as u32,
                    Err(_) => PrepareFactoryResetResult::Unknown as u32,
                })
            }
        }
    }

    async fn set_wifi_power_management_state(&self, state: u32) -> fdo::Result<()> {
        let state = match WifiPowerManagement::try_from(state) {
            Ok(state) => state,
            Err(err) => return Err(to_zbus_fdo_error(err)),
        };
        set_wifi_power_management_state(state)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn fan_control_state(&self) -> fdo::Result<u32> {
        Ok(self
            .fan_control
            .get_state()
            .await
            .map_err(to_zbus_fdo_error)? as u32)
    }

    #[zbus(property)]
    async fn set_fan_control_state(&self, state: u32) -> zbus::Result<()> {
        let state = match FanControlState::try_from(state) {
            Ok(state) => state,
            Err(err) => return Err(fdo::Error::InvalidArgs(err.to_string()).into()),
        };
        // Run what steamos-polkit-helpers/jupiter-fan-control does
        self.fan_control
            .set_state(state)
            .await
            .map_err(to_zbus_error)
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn als_calibration_gain(&self) -> Vec<f64> {
        // Run script to get calibration value
        let mut gains = Vec::new();
        let indices: &[&str] = match steam_deck_variant().await {
            Ok(SteamDeckVariant::Jupiter) => &["2"],
            Ok(SteamDeckVariant::Galileo) => &["2", "4"],
            _ => return Vec::new(),
        };
        for index in indices {
            let result = script_output(
                "/usr/bin/steamos-polkit-helpers/jupiter-get-als-gain",
                &["-s", index],
            )
            .await;
            gains.push(match result {
                Ok(as_string) => as_string.trim().parse().unwrap_or(-1.0),
                Err(message) => {
                    error!("Unable to run als calibration script: {}", message);
                    -1.0
                }
            });
        }

        gains
    }

    async fn get_als_integration_time_file_descriptor(&self, index: u32) -> fdo::Result<Fd> {
        // Get the file descriptor for the als integration time sysfs path
        let i0 = match steam_deck_variant().await.map_err(to_zbus_fdo_error)? {
            SteamDeckVariant::Jupiter => 1,
            SteamDeckVariant::Galileo => index,
            SteamDeckVariant::Unknown => {
                return Err(fdo::Error::Failed(String::from("Unknown model")))
            }
        };
        let als_path = path(format!("/sys/devices/platform/AMDI0010:00/i2c-0/i2c-PRP0001:0{i0}/iio:device{index}/in_illuminance_integration_time"));
        let result = File::create(als_path).await;

        match result {
            Ok(f) => Ok(Fd::Owned(std::os::fd::OwnedFd::from(f.into_std().await))),
            Err(message) => {
                error!("Error opening sysfs file for giving file descriptor: {message}");
                Err(fdo::Error::IOError(message.to_string()))
            }
        }
    }

    async fn update_bios(&mut self) -> fdo::Result<zvariant::OwnedObjectPath> {
        // Update the bios as needed
        with_platform_config! {
            config = update_bios ("UpdateBios") => {
                self.job_manager
                    .run_process(&config.script, &config.script_args, "updating BIOS")
                    .await
            }
        }
    }

    async fn update_dock(&mut self) -> fdo::Result<zvariant::OwnedObjectPath> {
        // Update the dock firmware as needed
        with_platform_config! {
            config = update_dock ("UpdateDock") => {
                self.job_manager
                    .run_process(&config.script, &config.script_args, "updating dock")
                    .await
            }
        }
    }

    async fn trim_devices(&mut self) -> fdo::Result<zvariant::OwnedObjectPath> {
        // Run steamos-trim-devices script
        with_platform_config! {
            config = storage ("TrimDevices") => {
                self.job_manager
                    .run_process(&config.trim_devices.script, config.trim_devices.script_args.as_ref(), "trimming devices")
                    .await
            }
        }
    }

    async fn format_device(
        &mut self,
        device: &str,
        label: &str,
        validate: bool,
    ) -> fdo::Result<zvariant::OwnedObjectPath> {
        with_platform_config! {
            config = storage ("FormatDevice") => {
                let config = &config.format_device;
                let mut args: Vec<&OsStr> = config.script_args.iter().map(AsRef::as_ref).collect();

                args.extend_from_slice(&[OsStr::new(config.label_flag.as_str()), OsStr::new(label)]);

                match (validate, &config.validate_flag, &config.no_validate_flag) {
                    (true, Some(validate_flag), _) => args.push(OsStr::new(validate_flag)),
                    (false, _, Some(no_validate_flag)) => args.push(OsStr::new(no_validate_flag)),
                    _ => (),
                }

                if let Some(device_flag) = &config.device_flag {
                    args.push(OsStr::new(device_flag));
                }
                args.push(OsStr::new(device));

                self.job_manager
                    .run_process(
                        &config.script,
                        &args,
                        format!("formatting {device}").as_str(),
                    )
                    .await
            }
        }
    }

    async fn set_gpu_power_profile(&self, value: &str) -> fdo::Result<()> {
        let profile = GPUPowerProfile::try_from(value).map_err(to_zbus_fdo_error)?;
        set_gpu_power_profile(profile)
            .await
            .inspect_err(|message| error!("Error setting GPU power profile: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    async fn set_cpu_scaling_governor(&self, governor: String) -> fdo::Result<()> {
        let g = CPUScalingGovernor::try_from(governor.as_str()).map_err(to_zbus_fdo_error)?;
        set_cpu_scaling_governor(g)
            .await
            .inspect_err(|message| error!("Error setting CPU scaling governor: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    async fn set_gpu_performance_level(&self, level: &str) -> fdo::Result<()> {
        let level = match GPUPerformanceLevel::try_from(level) {
            Ok(level) => level,
            Err(e) => return Err(to_zbus_fdo_error(e)),
        };
        set_gpu_performance_level(level)
            .await
            .inspect_err(|message| error!("Error setting GPU performance level: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    async fn set_manual_gpu_clock(&self, clocks: u32) -> fdo::Result<()> {
        set_gpu_clocks(clocks)
            .await
            .inspect_err(|message| error!("Error setting manual GPU clock: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    async fn set_tdp_limit(&self, limit: u32) -> fdo::Result<()> {
        set_tdp_limit(limit).await.map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn wifi_debug_mode_state(&self) -> u32 {
        // Get the wifi debug mode
        self.wifi_debug_mode as u32
    }

    async fn set_wifi_debug_mode(
        &mut self,
        mode: u32,
        options: HashMap<&str, zvariant::Value<'_>>,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        // Set the wifi debug mode to mode, using an int for flexibility going forward but only
        // doing things on 0 or 1 for now
        let wanted_mode = match WifiDebugMode::try_from(mode) {
            Ok(mode) => mode,
            Err(e) => return Err(fdo::Error::InvalidArgs(e.to_string())),
        };

        if self.wifi_debug_mode == wanted_mode {
            info!("Not changing wifi debug mode since it's already set to {wanted_mode}");
            return Ok(());
        }

        let buffer_size = match options
            .get("buffer_size")
            .map(zbus::zvariant::Value::downcast_ref)
        {
            Some(Ok(v)) => v,
            None => 20000,
            Some(Err(e)) => return Err(fdo::Error::InvalidArgs(e.to_string())),
        };
        match set_wifi_debug_mode(
            wanted_mode,
            buffer_size,
            self.should_trace,
            self.connection.clone(),
        )
        .await
        {
            Ok(()) => {
                self.wifi_debug_mode = wanted_mode;
                self.wifi_debug_mode_state_changed(&ctx).await?;
                Ok(())
            }
            Err(e) => {
                error!("Error setting wifi debug mode: {e}");
                Err(to_zbus_fdo_error(e))
            }
        }
    }

    async fn set_wifi_backend(&mut self, backend: u32) -> fdo::Result<()> {
        if self.wifi_debug_mode == WifiDebugMode::Tracing {
            return Err(fdo::Error::Failed(String::from(
                "operation not supported when wifi_debug_mode=tracing",
            )));
        }
        let backend = match WifiBackend::try_from(backend) {
            Ok(backend) => backend,
            Err(e) => return Err(fdo::Error::InvalidArgs(e.to_string())),
        };
        set_wifi_backend(backend)
            .await
            .inspect_err(|message| error!("Error setting wifi backend: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    async fn capture_debug_trace_output(&self) -> fdo::Result<String> {
        Ok(extract_wifi_trace()
            .await
            .inspect_err(|message| error!("Error capturing trace output: {message}"))
            .map_err(to_zbus_fdo_error)?
            .into_os_string()
            .to_string_lossy()
            .into())
    }

    async fn generate_debug_dump(&self) -> fdo::Result<String> {
        Ok(generate_wifi_dump()
            .await
            .inspect_err(|message| error!("Error capturing dump output: {message}"))
            .map_err(to_zbus_fdo_error)?
            .into_os_string()
            .to_string_lossy()
            .into())
    }

    #[zbus(property)]
    async fn inhibit_ds(&self) -> fdo::Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(DaemonCommand::ContextCommand(RootCommand::GetDsInhibit(tx)))
            .await
            .inspect_err(|message| error!("Error sending GetDsInhibit command: {message}"))
            .map_err(to_zbus_fdo_error)?;
        rx.await
            .inspect_err(|message| error!("Error receiving GetDsInhibit reply: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn set_inhibit_ds(&self, enable: bool) -> zbus::Result<()> {
        self.channel
            .send(DaemonCommand::ContextCommand(RootCommand::SetDsInhibit(
                enable,
            )))
            .await
            .inspect_err(|message| error!("Error sending SetDsInhibit command: {message}"))
            .map_err(to_zbus_error)
    }

    async fn reload_config(&self) -> fdo::Result<()> {
        self.channel
            .send(DaemonCommand::ReadConfig)
            .await
            .inspect_err(|message| error!("Error sending ReadConfig command: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    async fn set_max_charge_level(&self, level: i32) -> fdo::Result<()> {
        set_max_charge_level(if level == -1 { 0 } else { level })
            .await
            .map_err(to_zbus_fdo_error)
    }

    /// A version property.
    #[zbus(property(emits_changed_signal = "const"))]
    async fn version(&self) -> u32 {
        API_VERSION
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::daemon::channel;
    use crate::daemon::root::RootContext;
    use crate::hardware::test::fake_model;
    use crate::platform::{PlatformConfig, ResetConfig};
    use crate::power::test::{format_clocks, read_clocks};
    use crate::power::{self, get_gpu_performance_level};
    use crate::process::test::{code, exit, ok};
    use crate::testing;
    use std::time::Duration;
    use tokio::fs::{create_dir_all, write};
    use tokio::time::sleep;
    use zbus::Connection;

    struct TestHandle {
        h: testing::TestHandle,
        connection: Connection,
    }

    async fn start() -> Result<TestHandle> {
        let mut handle = testing::start();
        fake_model(SteamDeckVariant::Jupiter).await?;
        create_dir_all(crate::path("/etc/NetworkManager/conf.d")).await?;
        write(
            crate::path("/etc/NetworkManager/conf.d/99-valve-wifi-backend.conf"),
            "wifi.backend=iwd\n",
        )
        .await?;

        let (tx, _rx) = channel::<RootContext>();
        let connection = handle.new_dbus().await?;
        let manager = SteamOSManager::new(connection.clone(), tx).await?;
        connection
            .object_server()
            .at("/com/steampowered/SteamOSManager1", manager)
            .await?;

        sleep(Duration::from_millis(1)).await;

        Ok(TestHandle {
            h: handle,
            connection,
        })
    }

    #[zbus::proxy(
        interface = "com.steampowered.SteamOSManager1.RootManager",
        default_path = "/com/steampowered/SteamOSManager1"
    )]
    trait PrepareFactoryReset {
        fn prepare_factory_reset(&self, kind: u32) -> zbus::Result<u32>;
    }

    #[tokio::test]
    async fn prepare_factory_reset() {
        let test = start().await.expect("start");

        let mut config = PlatformConfig::default();
        config.factory_reset = Some(ResetConfig::default());
        test.h.test.platform_config.replace(Some(config));

        let name = test.connection.unique_name().unwrap();
        let proxy = PrepareFactoryResetProxy::new(&test.connection, name.clone())
            .await
            .unwrap();

        test.h.test.process_cb.set(ok);
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::All as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::RebootRequired as u32
        );

        test.h.test.process_cb.set(code);
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::All as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::Unknown as u32
        );

        test.h.test.process_cb.set(exit);
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::All as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::Unknown as u32
        );

        test.h.test.process_cb.set(ok);
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::OS as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::RebootRequired as u32
        );

        test.h.test.process_cb.set(code);
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::OS as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::Unknown as u32
        );

        test.h.test.process_cb.set(exit);
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::OS as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::Unknown as u32
        );

        test.h.test.process_cb.set(ok);
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::User as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::RebootRequired as u32
        );

        test.h.test.process_cb.set(code);
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::User as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::Unknown as u32
        );

        test.h.test.process_cb.set(exit);
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::User as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::Unknown as u32
        );

        test.connection.close().await.unwrap();
    }

    #[zbus::proxy(
        interface = "com.steampowered.SteamOSManager1.RootManager",
        default_path = "/com/steampowered/SteamOSManager1"
    )]
    trait AlsCalibrationGain {
        #[zbus(property(emits_changed_signal = "false"))]
        fn als_calibration_gain(&self) -> zbus::Result<Vec<f64>>;
    }

    #[tokio::test]
    async fn als_calibration_gain() {
        let test = start().await.expect("start");
        let name = test.connection.unique_name().unwrap();
        let proxy = AlsCalibrationGainProxy::new(&test.connection, name.clone())
            .await
            .unwrap();

        test.h
            .test
            .process_cb
            .set(|_, _| Ok((0, String::from("0.0\n"))));

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");
        assert_eq!(proxy.als_calibration_gain().await.unwrap(), &[0.0]);

        fake_model(SteamDeckVariant::Galileo)
            .await
            .expect("fake_model");
        assert_eq!(proxy.als_calibration_gain().await.unwrap(), &[0.0, 0.0]);

        fake_model(SteamDeckVariant::Unknown)
            .await
            .expect("fake_model");
        assert_eq!(proxy.als_calibration_gain().await.unwrap(), &[]);

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");

        test.h
            .test
            .process_cb
            .set(|_, _| Ok((0, String::from("1.0\n"))));
        assert_eq!(proxy.als_calibration_gain().await.unwrap(), &[1.0]);

        test.h
            .test
            .process_cb
            .set(|_, _| Ok((0, String::from("big\n"))));
        assert_eq!(proxy.als_calibration_gain().await.unwrap(), &[-1.0]);

        test.connection.close().await.unwrap();
    }

    #[zbus::proxy(
        interface = "com.steampowered.SteamOSManager1.RootManager",
        default_path = "/com/steampowered/SteamOSManager1"
    )]
    trait GpuPerformanceLevel {
        fn set_gpu_performance_level(&self, level: String) -> zbus::Result<()>;
    }

    #[tokio::test]
    async fn gpu_performance_level() {
        let test = start().await.expect("start");
        power::test::setup().await.expect("setup");

        let name = test.connection.unique_name().unwrap();
        let proxy = GpuPerformanceLevelProxy::new(&test.connection, name.clone())
            .await
            .unwrap();
        proxy
            .set_gpu_performance_level(GPUPerformanceLevel::Low.to_string())
            .await
            .expect("proxy_set");
        assert_eq!(
            get_gpu_performance_level().await.unwrap(),
            GPUPerformanceLevel::Low
        );

        test.connection.close().await.unwrap();
    }

    #[zbus::proxy(
        interface = "com.steampowered.SteamOSManager1.RootManager",
        default_path = "/com/steampowered/SteamOSManager1"
    )]
    trait ManualGpuClock {
        fn set_manual_gpu_clock(&self, clocks: u32) -> zbus::Result<()>;
    }

    #[tokio::test]
    async fn manual_gpu_clock() {
        let test = start().await.expect("start");

        let name = test.connection.unique_name().unwrap();
        let proxy = ManualGpuClockProxy::new(&test.connection, name.clone())
            .await
            .unwrap();

        power::test::setup().await.expect("setup");
        proxy.set_manual_gpu_clock(200).await.expect("proxy_set");
        assert_eq!(read_clocks().await.unwrap(), format_clocks(200));

        test.connection.close().await.unwrap();
    }

    #[zbus::proxy(
        interface = "com.steampowered.SteamOSManager1.RootManager",
        default_path = "/com/steampowered/SteamOSManager1"
    )]
    trait Version {
        #[zbus(property)]
        fn version(&self) -> zbus::Result<u32>;
    }

    #[tokio::test]
    async fn version() {
        let test = start().await.expect("start");
        let name = test.connection.unique_name().unwrap();
        let proxy = VersionProxy::new(&test.connection, name.clone())
            .await
            .unwrap();
        assert_eq!(proxy.version().await, Ok(API_VERSION));

        test.connection.close().await.unwrap();
    }
}
