/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{anyhow, bail, ensure, Result};
use lazy_static::lazy_static;
use num_enum::TryFromPrimitive;
use regex::Regex;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use strum::{Display, EnumString};
use tokio::fs::{self, try_exists, File};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{error, warn};

use crate::hardware::{device_type, DeviceType};
use crate::platform::platform_config;
use crate::{path, write_synced};

const HWMON_PREFIX: &str = "/sys/class/hwmon";

const GPU_HWMON_NAME: &str = "amdgpu";

const CPU_PREFIX: &str = "/sys/devices/system/cpu/cpufreq";

const CPU0_NAME: &str = "policy0";
const CPU_POLICY_NAME: &str = "policy";

const GPU_POWER_PROFILE_SUFFIX: &str = "device/pp_power_profile_mode";
const GPU_PERFORMANCE_LEVEL_SUFFIX: &str = "device/power_dpm_force_performance_level";
const GPU_CLOCKS_SUFFIX: &str = "device/pp_od_clk_voltage";
const GPU_CLOCK_LEVELS_SUFFIX: &str = "device/pp_dpm_sclk";
const CPU_SCALING_GOVERNOR_SUFFIX: &str = "scaling_governor";
const CPU_SCALING_AVAILABLE_GOVERNORS_SUFFIX: &str = "scaling_available_governors";

const TDP_LIMIT1: &str = "power1_cap";
const TDP_LIMIT2: &str = "power2_cap";

lazy_static! {
    static ref GPU_POWER_PROFILE_REGEX: Regex =
        Regex::new(r"^\s*(?<value>[0-9]+)\s+(?<name>[0-9A-Za-z_]+)(?<active>\*)?").unwrap();
    static ref GPU_CLOCK_LEVELS_REGEX: Regex =
        Regex::new(r"^\s*(?<index>[0-9]+): (?<value>[0-9]+)Mhz").unwrap();
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(serialize_all = "snake_case")]
#[repr(u32)]
pub enum GPUPowerProfile {
    // Currently firmware exposes these values, though
    // deck doesn't support them yet
    #[strum(serialize = "3d_full_screen")]
    FullScreen = 1,
    Video = 3,
    VR = 4,
    Compute = 5,
    Custom = 6,
    // Currently only capped and uncapped are supported on
    // deck hardware/firmware. Add more later as needed
    Capped = 8,
    Uncapped = 9,
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone)]
#[strum(serialize_all = "snake_case")]
pub enum GPUPerformanceLevel {
    Auto,
    Low,
    High,
    Manual,
    ProfilePeak,
}

#[derive(Display, EnumString, Hash, Eq, PartialEq, Debug, Copy, Clone)]
#[strum(serialize_all = "lowercase")]
pub enum CPUScalingGovernor {
    Conservative,
    OnDemand,
    UserSpace,
    PowerSave,
    Performance,
    SchedUtil,
}

async fn read_gpu_sysfs_contents<S: AsRef<Path>>(suffix: S) -> Result<String> {
    // Read a given suffix for the GPU
    let base = find_hwmon(GPU_HWMON_NAME).await?;
    fs::read_to_string(base.join(suffix.as_ref()))
        .await
        .map_err(|message| anyhow!("Error opening sysfs file for reading {message}"))
}

async fn write_gpu_sysfs_contents<S: AsRef<Path>>(suffix: S, data: &[u8]) -> Result<()> {
    let base = find_hwmon(GPU_HWMON_NAME).await?;
    write_synced(base.join(suffix), data)
        .await
        .inspect_err(|message| error!("Error writing to sysfs file: {message}"))
}

async fn read_cpu_sysfs_contents<S: AsRef<Path>>(suffix: S) -> Result<String> {
    let base = path(CPU_PREFIX).join(CPU0_NAME);
    fs::read_to_string(base.join(suffix.as_ref()))
        .await
        .map_err(|message| anyhow!("Error opening sysfs file for reading {message}"))
}

async fn write_cpu_governor_sysfs_contents(contents: String) -> Result<()> {
    // Iterate over all policyX paths
    let mut dir = fs::read_dir(path(CPU_PREFIX)).await?;
    let mut wrote_stuff = false;
    loop {
        let Some(entry) = dir.next_entry().await? else {
            ensure!(
                wrote_stuff,
                "No data written, unable to find any policyX sysfs paths"
            );
            return Ok(());
        };
        let file_name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("Unable to convert path to string"))?;
        if !file_name.starts_with(CPU_POLICY_NAME) {
            continue;
        }
        let base = entry.path();
        // Write contents to each one
        wrote_stuff = true;
        write_synced(base.join(CPU_SCALING_GOVERNOR_SUFFIX), contents.as_bytes())
            .await
            .inspect_err(|message| error!("Error writing to sysfs file: {message}"))?;
    }
}

pub(crate) async fn get_gpu_power_profile() -> Result<GPUPowerProfile> {
    // check which profile is current and return if possible
    let contents = read_gpu_sysfs_contents(GPU_POWER_PROFILE_SUFFIX).await?;

    // NOTE: We don't filter based on deck here because the sysfs
    // firmware support setting the value to no-op values.
    let lines = contents.lines();
    for line in lines {
        let Some(caps) = GPU_POWER_PROFILE_REGEX.captures(line) else {
            continue;
        };

        let name = &caps["name"].to_lowercase();
        if caps.name("active").is_some() {
            match GPUPowerProfile::from_str(name.as_str()) {
                Ok(v) => {
                    return Ok(v);
                }
                Err(e) => bail!("Unable to parse value for GPU power profile: {e}"),
            }
        }
    }
    bail!("Unable to determine current GPU power profile");
}

pub(crate) async fn get_available_gpu_power_profiles() -> Result<Vec<(u32, String)>> {
    let contents = read_gpu_sysfs_contents(GPU_POWER_PROFILE_SUFFIX).await?;
    let deck = device_type().await.unwrap_or_default() == DeviceType::SteamDeck;

    let mut map = Vec::new();
    let lines = contents.lines();
    for line in lines {
        let Some(caps) = GPU_POWER_PROFILE_REGEX.captures(line) else {
            continue;
        };
        let value: u32 = caps["value"]
            .parse()
            .map_err(|message| anyhow!("Unable to parse value for GPU power profile: {message}"))?;
        let name = &caps["name"];
        if deck {
            // Deck is designed to operate in one of the CAPPED or UNCAPPED power profiles,
            // the other profiles aren't correctly tuned for the hardware.
            if value == GPUPowerProfile::Capped as u32 || value == GPUPowerProfile::Uncapped as u32
            {
                map.push((value, name.to_string()));
            } else {
                // Got unsupported value, so don't include it
            }
        } else {
            // Do basic validation to ensure our enum is up to date?
            map.push((value, name.to_string()));
        }
    }
    Ok(map)
}

pub(crate) async fn set_gpu_power_profile(value: GPUPowerProfile) -> Result<()> {
    let profile = (value as u32).to_string();
    write_gpu_sysfs_contents(GPU_POWER_PROFILE_SUFFIX, profile.as_bytes()).await
}

pub(crate) async fn get_available_gpu_performance_levels() -> Result<Vec<GPUPerformanceLevel>> {
    let base = find_hwmon(GPU_HWMON_NAME).await?;
    if try_exists(base.join(GPU_PERFORMANCE_LEVEL_SUFFIX)).await? {
        Ok(vec![
            GPUPerformanceLevel::Auto,
            GPUPerformanceLevel::Low,
            GPUPerformanceLevel::High,
            GPUPerformanceLevel::Manual,
            GPUPerformanceLevel::ProfilePeak,
        ])
    } else {
        Ok(Vec::new())
    }
}

pub(crate) async fn get_gpu_performance_level() -> Result<GPUPerformanceLevel> {
    let level = read_gpu_sysfs_contents(GPU_PERFORMANCE_LEVEL_SUFFIX).await?;
    Ok(GPUPerformanceLevel::from_str(level.trim())?)
}

pub(crate) async fn set_gpu_performance_level(level: GPUPerformanceLevel) -> Result<()> {
    let level: String = level.to_string();
    write_gpu_sysfs_contents(GPU_PERFORMANCE_LEVEL_SUFFIX, level.as_bytes()).await
}

pub(crate) async fn get_available_cpu_scaling_governors() -> Result<Vec<CPUScalingGovernor>> {
    let contents = read_cpu_sysfs_contents(CPU_SCALING_AVAILABLE_GOVERNORS_SUFFIX).await?;
    // Get the list of supported governors from cpu0
    let mut result = Vec::new();

    let words = contents.split_whitespace();
    for word in words {
        match CPUScalingGovernor::from_str(word) {
            Ok(governor) => result.push(governor),
            Err(message) => warn!("Error parsing governor {message}"),
        }
    }

    Ok(result)
}

pub(crate) async fn get_cpu_scaling_governor() -> Result<CPUScalingGovernor> {
    // get the current governor from cpu0 (assume all others are the same)
    let contents = read_cpu_sysfs_contents(CPU_SCALING_GOVERNOR_SUFFIX).await?;

    let contents = contents.trim();
    CPUScalingGovernor::from_str(contents).map_err(|message| {
        anyhow!(
            "Error converting CPU scaling governor sysfs file contents to enumeration: {message}"
        )
    })
}

pub(crate) async fn set_cpu_scaling_governor(governor: CPUScalingGovernor) -> Result<()> {
    // Set the given governor on all cpus
    let name = governor.to_string();
    write_cpu_governor_sysfs_contents(name).await
}

pub(crate) async fn get_gpu_clocks_range() -> Result<RangeInclusive<u32>> {
    if let Some(range) = platform_config()
        .await?
        .as_ref()
        .and_then(|config| config.gpu_clocks)
    {
        return Ok(range.min..=range.max);
    }
    let contents = read_gpu_sysfs_contents(GPU_CLOCK_LEVELS_SUFFIX).await?;
    let lines = contents.lines();
    let mut min = 1_000_000;
    let mut max = 0;

    for line in lines {
        let Some(caps) = GPU_CLOCK_LEVELS_REGEX.captures(line) else {
            continue;
        };
        let value: u32 = caps["value"]
            .parse()
            .map_err(|message| anyhow!("Unable to parse value for GPU power profile: {message}"))?;
        if value < min {
            min = value;
        }
        if value > max {
            max = value;
        }
    }

    ensure!(min <= max, "Could not read any clocks");
    Ok(min..=max)
}

pub(crate) async fn set_gpu_clocks(clocks: u32) -> Result<()> {
    // Set GPU clocks to given value valid
    // Only used when GPU Performance Level is manual, but write whenever called.
    let base = find_hwmon(GPU_HWMON_NAME).await?;
    let mut myfile = File::create(base.join(GPU_CLOCKS_SUFFIX))
        .await
        .inspect_err(|message| error!("Error opening sysfs file for writing: {message}"))?;

    let data = format!("s 0 {clocks}\n");
    myfile
        .write(data.as_bytes())
        .await
        .inspect_err(|message| error!("Error writing to sysfs file: {message}"))?;
    myfile.flush().await?;

    let data = format!("s 1 {clocks}\n");
    myfile
        .write(data.as_bytes())
        .await
        .inspect_err(|message| error!("Error writing to sysfs file: {message}"))?;
    myfile.flush().await?;

    myfile
        .write("c\n".as_bytes())
        .await
        .inspect_err(|message| error!("Error writing to sysfs file: {message}"))?;
    myfile.flush().await?;

    Ok(())
}

pub(crate) async fn get_gpu_clocks() -> Result<u32> {
    let base = find_hwmon(GPU_HWMON_NAME).await?;
    let clocks_file = File::open(base.join(GPU_CLOCKS_SUFFIX)).await?;
    let mut reader = BufReader::new(clocks_file);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        if line != "OD_SCLK:\n" {
            continue;
        }

        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        let mhz = match line.split_whitespace().nth(1) {
            Some(mhz) if mhz.ends_with("Mhz") => mhz.trim_end_matches("Mhz"),
            _ => break,
        };

        return Ok(mhz.parse()?);
    }
    Ok(0)
}

async fn find_hwmon(hwmon: &str) -> Result<PathBuf> {
    let mut dir = fs::read_dir(path(HWMON_PREFIX)).await?;
    loop {
        let base = match dir.next_entry().await? {
            Some(entry) => entry.path(),
            None => bail!("hwmon not found"),
        };
        let file_name = base.join("name");
        let name = fs::read_to_string(file_name.as_path())
            .await?
            .trim()
            .to_string();
        if name == hwmon {
            return Ok(base);
        }
    }
}

pub(crate) async fn get_tdp_limit() -> Result<u32> {
    let base = find_hwmon(GPU_HWMON_NAME).await?;
    let power1cap = fs::read_to_string(base.join(TDP_LIMIT1)).await?;
    let power1cap: u32 = power1cap.trim_end().parse()?;
    Ok(power1cap / 1_000_000)
}

pub(crate) async fn set_tdp_limit(limit: u32) -> Result<()> {
    ensure!(
        get_tdp_limit_range().await?.contains(&limit),
        "Invalid limit"
    );
    let data = format!("{limit}000000");

    let base = find_hwmon(GPU_HWMON_NAME).await?;
    write_synced(base.join(TDP_LIMIT1), data.as_bytes())
        .await
        .inspect_err(|message| {
            error!("Error opening sysfs power1_cap file for writing TDP limits {message}");
        })?;

    if let Ok(mut power2file) = File::create(base.join(TDP_LIMIT2)).await {
        power2file
            .write(data.as_bytes())
            .await
            .inspect_err(|message| error!("Error writing to power2_cap file: {message}"))?;
        power2file.flush().await?;
    }
    Ok(())
}

pub(crate) async fn get_tdp_limit_range() -> Result<RangeInclusive<u32>> {
    let range = platform_config()
        .await?
        .as_ref()
        .and_then(|config| config.tdp_limit)
        .ok_or(anyhow!("No TDP limit range configured"))?;
    Ok(range.min..=range.max)
}

pub(crate) async fn get_max_charge_level() -> Result<i32> {
    let config = platform_config().await?;
    let config = config
        .as_ref()
        .and_then(|config| config.battery_charge_limit.as_ref())
        .ok_or(anyhow!("No battery charge limit configured"))?;
    let base = find_hwmon(config.hwmon_name.as_str()).await?;

    fs::read_to_string(base.join(config.attribute.as_str()))
        .await
        .map_err(|message| anyhow!("Error reading sysfs: {message}"))?
        .trim()
        .parse()
        .map_err(|e| anyhow!("Error parsing value: {e}"))
}

pub(crate) async fn set_max_charge_level(limit: i32) -> Result<()> {
    ensure!((0..=100).contains(&limit), "Invalid limit");
    let data = limit.to_string();
    let config = platform_config().await?;
    let config = config
        .as_ref()
        .and_then(|config| config.battery_charge_limit.as_ref())
        .ok_or(anyhow!("No battery charge limit configured"))?;
    let base = find_hwmon(config.hwmon_name.as_str()).await?;

    write_synced(base.join(config.attribute.as_str()), data.as_bytes())
        .await
        .inspect_err(|message| error!("Error writing to sysfs file: {message}"))
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;
    use crate::hardware::test::fake_model;
    use crate::hardware::SteamDeckVariant;
    use crate::platform::{BatteryChargeLimitConfig, PlatformConfig, RangeConfig};
    use crate::{enum_roundtrip, testing};
    use anyhow::anyhow;
    use tokio::fs::{create_dir_all, read_to_string, remove_dir, write};

    pub async fn setup() -> Result<()> {
        // Use hwmon5 just as a test. We needed a subfolder of HWMON_PREFIX
        // and this is as good as any.
        let base = path(HWMON_PREFIX).join("hwmon5");
        let filename = base.join(GPU_PERFORMANCE_LEVEL_SUFFIX);
        // Creates hwmon path, including device subpath
        create_dir_all(filename.parent().unwrap()).await?;
        // Writes name file as addgpu so find_hwmon() will find it.
        write_synced(base.join("name"), GPU_HWMON_NAME.as_bytes()).await?;
        Ok(())
    }

    pub async fn create_nodes() -> Result<()> {
        setup().await?;
        let base = find_hwmon(GPU_HWMON_NAME).await?;

        let filename = base.join(GPU_PERFORMANCE_LEVEL_SUFFIX);
        write(filename.as_path(), "auto\n").await?;

        let filename = base.join(GPU_POWER_PROFILE_SUFFIX);
        let contents = " 1 3D_FULL_SCREEN
 3          VIDEO*
 4             VR
 5        COMPUTE
 6         CUSTOM
 8         CAPPED
 9       UNCAPPED";
        write(filename.as_path(), contents).await?;

        let filename = base.join(TDP_LIMIT1);
        write(filename.as_path(), "15000000\n").await?;

        let base = path(HWMON_PREFIX).join("hwmon6");
        create_dir_all(&base).await?;

        write(base.join("name"), "steamdeck_hwmon\n").await?;

        write(base.join("max_battery_charge_level"), "10\n").await?;

        Ok(())
    }

    pub async fn write_clocks(mhz: u32) {
        let base = find_hwmon(GPU_HWMON_NAME).await.unwrap();
        let filename = base.join(GPU_CLOCKS_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        let contents = format!(
            "OD_SCLK:
0:       {mhz}Mhz
1:       {mhz}Mhz
OD_RANGE:
SCLK:     200Mhz       1600Mhz
CCLK:    1400Mhz       3500Mhz
CCLK_RANGE in Core0:
0:       1400Mhz
1:       3500Mhz\n"
        );

        write(filename.as_path(), contents).await.expect("write");
    }

    pub async fn read_clocks() -> Result<String, std::io::Error> {
        let base = find_hwmon(GPU_HWMON_NAME).await.unwrap();
        read_to_string(base.join(GPU_CLOCKS_SUFFIX)).await
    }

    pub fn format_clocks(mhz: u32) -> String {
        format!("s 0 {mhz}\ns 1 {mhz}\nc\n")
    }

    #[tokio::test]
    async fn test_get_gpu_performance_level() {
        let _h = testing::start();

        setup().await.expect("setup");
        let base = find_hwmon(GPU_HWMON_NAME).await.unwrap();
        let filename = base.join(GPU_PERFORMANCE_LEVEL_SUFFIX);
        assert!(get_gpu_performance_level().await.is_err());

        write(filename.as_path(), "auto\n").await.expect("write");
        assert_eq!(
            get_gpu_performance_level().await.unwrap(),
            GPUPerformanceLevel::Auto
        );

        write(filename.as_path(), "low\n").await.expect("write");
        assert_eq!(
            get_gpu_performance_level().await.unwrap(),
            GPUPerformanceLevel::Low
        );

        write(filename.as_path(), "high\n").await.expect("write");
        assert_eq!(
            get_gpu_performance_level().await.unwrap(),
            GPUPerformanceLevel::High
        );

        write(filename.as_path(), "manual\n").await.expect("write");
        assert_eq!(
            get_gpu_performance_level().await.unwrap(),
            GPUPerformanceLevel::Manual
        );

        write(filename.as_path(), "profile_peak\n")
            .await
            .expect("write");
        assert_eq!(
            get_gpu_performance_level().await.unwrap(),
            GPUPerformanceLevel::ProfilePeak
        );

        write(filename.as_path(), "nothing\n").await.expect("write");
        assert!(get_gpu_performance_level().await.is_err());
    }

    #[tokio::test]
    async fn test_set_gpu_performance_level() {
        let _h = testing::start();

        setup().await.expect("setup");
        let base = find_hwmon(GPU_HWMON_NAME).await.unwrap();
        let filename = base.join(GPU_PERFORMANCE_LEVEL_SUFFIX);

        set_gpu_performance_level(GPUPerformanceLevel::Auto)
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "auto"
        );
        set_gpu_performance_level(GPUPerformanceLevel::Low)
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "low"
        );
        set_gpu_performance_level(GPUPerformanceLevel::High)
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "high"
        );
        set_gpu_performance_level(GPUPerformanceLevel::Manual)
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "manual"
        );
        set_gpu_performance_level(GPUPerformanceLevel::ProfilePeak)
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "profile_peak"
        );
    }

    #[tokio::test]
    async fn test_get_tdp_limit() {
        let _h = testing::start();

        setup().await.expect("setup");
        let hwmon = path(HWMON_PREFIX);

        assert!(get_tdp_limit().await.is_err());

        write(hwmon.join("hwmon5").join(TDP_LIMIT1), "15000000\n")
            .await
            .expect("write");
        assert_eq!(get_tdp_limit().await.unwrap(), 15);
    }

    #[tokio::test]
    async fn test_set_tdp_limit() {
        let handle = testing::start();

        let mut platform_config = PlatformConfig::default();
        platform_config.tdp_limit = Some(RangeConfig {
            min: 3,
            max: 15,
        });
        handle.test.platform_config.replace(Some(platform_config));

        assert_eq!(
            set_tdp_limit(2).await.unwrap_err().to_string(),
            anyhow!("Invalid limit").to_string()
        );
        assert_eq!(
            set_tdp_limit(20).await.unwrap_err().to_string(),
            anyhow!("Invalid limit").to_string()
        );
        assert!(set_tdp_limit(10).await.is_err());

        let hwmon = path(HWMON_PREFIX);
        assert_eq!(
            set_tdp_limit(10).await.unwrap_err().to_string(),
            anyhow!("No such file or directory (os error 2)").to_string()
        );

        setup().await.expect("setup");
        let hwmon = hwmon.join("hwmon5");
        create_dir_all(hwmon.join(TDP_LIMIT1))
            .await
            .expect("create_dir_all");
        create_dir_all(hwmon.join(TDP_LIMIT2))
            .await
            .expect("create_dir_all");
        assert_eq!(
            set_tdp_limit(10).await.unwrap_err().to_string(),
            anyhow!("Is a directory (os error 21)").to_string()
        );

        remove_dir(hwmon.join(TDP_LIMIT1))
            .await
            .expect("remove_dir");
        write(hwmon.join(TDP_LIMIT1), "0").await.expect("write");
        assert!(set_tdp_limit(10).await.is_ok());
        let power1_cap = read_to_string(hwmon.join(TDP_LIMIT1))
            .await
            .expect("power1_cap");
        assert_eq!(power1_cap, "10000000");

        remove_dir(hwmon.join(TDP_LIMIT2))
            .await
            .expect("remove_dir");
        write(hwmon.join(TDP_LIMIT2), "0").await.expect("write");
        assert!(set_tdp_limit(15).await.is_ok());
        let power1_cap = read_to_string(hwmon.join(TDP_LIMIT1))
            .await
            .expect("power1_cap");
        assert_eq!(power1_cap, "15000000");
        let power2_cap = read_to_string(hwmon.join(TDP_LIMIT2))
            .await
            .expect("power2_cap");
        assert_eq!(power2_cap, "15000000");
    }

    #[tokio::test]
    async fn test_get_gpu_clocks() {
        let _h = testing::start();

        assert!(get_gpu_clocks().await.is_err());
        setup().await.expect("setup");

        let base = find_hwmon(GPU_HWMON_NAME).await.unwrap();
        let filename = base.join(GPU_CLOCKS_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");
        write(filename.as_path(), b"").await.expect("write");

        assert_eq!(get_gpu_clocks().await.unwrap(), 0);
        write_clocks(1600).await;

        assert_eq!(get_gpu_clocks().await.unwrap(), 1600);
    }

    #[tokio::test]
    async fn test_set_gpu_clocks() {
        let _h = testing::start();

        assert!(set_gpu_clocks(1600).await.is_err());
        setup().await.expect("setup");

        assert!(set_gpu_clocks(200).await.is_ok());

        assert_eq!(read_clocks().await.unwrap(), format_clocks(200));

        assert!(set_gpu_clocks(1600).await.is_ok());
        assert_eq!(read_clocks().await.unwrap(), format_clocks(1600));
    }

    #[tokio::test]
    async fn test_get_gpu_clocks_range() {
        let _h = testing::start();

        setup().await.expect("setup");
        let base = find_hwmon(GPU_HWMON_NAME).await.unwrap();
        let filename = base.join(GPU_CLOCK_LEVELS_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        assert!(get_gpu_clocks_range().await.is_err());

        write(filename.as_path(), &[] as &[u8; 0])
            .await
            .expect("write");
        assert!(get_gpu_clocks_range().await.is_err());

        let contents = "0: 200Mhz *
1: 1100Mhz
2: 1600Mhz";
        write(filename.as_path(), contents).await.expect("write");
        assert_eq!(get_gpu_clocks_range().await.unwrap(), 200..=1600);

        let contents = "0: 1600Mhz *
1: 200Mhz
2: 1100Mhz";
        write(filename.as_path(), contents).await.expect("write");
        assert_eq!(get_gpu_clocks_range().await.unwrap(), 200..=1600);
    }

    #[test]
    fn gpu_power_profile_roundtrip() {
        enum_roundtrip!(GPUPowerProfile {
            1: u32 = FullScreen,
            3: u32 = Video,
            4: u32 = VR,
            5: u32 = Compute,
            6: u32 = Custom,
            8: u32 = Capped,
            9: u32 = Uncapped,
            "3d_full_screen": str = FullScreen,
            "video": str = Video,
            "vr": str = VR,
            "compute": str = Compute,
            "custom": str = Custom,
            "capped": str = Capped,
            "uncapped": str = Uncapped,
        });
        assert!(GPUPowerProfile::try_from(0).is_err());
        assert!(GPUPowerProfile::try_from(2).is_err());
        assert!(GPUPowerProfile::try_from(10).is_err());
        assert!(GPUPowerProfile::from_str("fullscreen").is_err());
    }

    #[test]
    fn cpu_governor_roundtrip() {
        enum_roundtrip!(CPUScalingGovernor {
            "conservative": str = Conservative,
            "ondemand": str = OnDemand,
            "userspace": str = UserSpace,
            "powersave": str = PowerSave,
            "performance": str = Performance,
            "schedutil": str = SchedUtil,
        });
        assert!(CPUScalingGovernor::from_str("usersave").is_err());
    }

    #[test]
    fn gpu_performance_level_roundtrip() {
        enum_roundtrip!(GPUPerformanceLevel {
            "auto": str = Auto,
            "low": str = Low,
            "high": str = High,
            "manual": str = Manual,
            "profile_peak": str = ProfilePeak,
        });
        assert!(GPUPerformanceLevel::from_str("peak_performance").is_err());
    }

    #[tokio::test]
    async fn read_power_profiles() {
        let _h = testing::start();

        setup().await.expect("setup");
        let base = find_hwmon(GPU_HWMON_NAME).await.unwrap();
        let filename = base.join(GPU_POWER_PROFILE_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        let contents = " 1 3D_FULL_SCREEN
 3          VIDEO*
 4             VR
 5        COMPUTE
 6         CUSTOM
 8         CAPPED
 9       UNCAPPED";

        write(filename.as_path(), contents).await.expect("write");

        fake_model(SteamDeckVariant::Unknown)
            .await
            .expect("fake_model");

        let profiles = get_available_gpu_power_profiles().await.expect("get");
        assert_eq!(
            profiles,
            &[
                (
                    GPUPowerProfile::FullScreen as u32,
                    String::from("3D_FULL_SCREEN")
                ),
                (GPUPowerProfile::Video as u32, String::from("VIDEO")),
                (GPUPowerProfile::VR as u32, String::from("VR")),
                (GPUPowerProfile::Compute as u32, String::from("COMPUTE")),
                (GPUPowerProfile::Custom as u32, String::from("CUSTOM")),
                (GPUPowerProfile::Capped as u32, String::from("CAPPED")),
                (GPUPowerProfile::Uncapped as u32, String::from("UNCAPPED"))
            ]
        );

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");

        let profiles = get_available_gpu_power_profiles().await.expect("get");
        assert_eq!(
            profiles,
            &[
                (GPUPowerProfile::Capped as u32, String::from("CAPPED")),
                (GPUPowerProfile::Uncapped as u32, String::from("UNCAPPED"))
            ]
        );
    }

    #[tokio::test]
    async fn read_unknown_power_profiles() {
        let _h = testing::start();

        setup().await.expect("setup");
        let base = find_hwmon(GPU_HWMON_NAME).await.unwrap();
        let filename = base.join(GPU_POWER_PROFILE_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        let contents = " 1 3D_FULL_SCREEN
 2            CGA
 3          VIDEO*
 4             VR
 5        COMPUTE
 6         CUSTOM
 8         CAPPED
 9       UNCAPPED";

        write(filename.as_path(), contents).await.expect("write");

        fake_model(SteamDeckVariant::Unknown)
            .await
            .expect("fake_model");

        let profiles = get_available_gpu_power_profiles().await.expect("get");
        assert_eq!(
            profiles,
            &[
                (
                    GPUPowerProfile::FullScreen as u32,
                    String::from("3D_FULL_SCREEN")
                ),
                (2, String::from("CGA")),
                (GPUPowerProfile::Video as u32, String::from("VIDEO")),
                (GPUPowerProfile::VR as u32, String::from("VR")),
                (GPUPowerProfile::Compute as u32, String::from("COMPUTE")),
                (GPUPowerProfile::Custom as u32, String::from("CUSTOM")),
                (GPUPowerProfile::Capped as u32, String::from("CAPPED")),
                (GPUPowerProfile::Uncapped as u32, String::from("UNCAPPED"))
            ]
        );

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");

        let profiles = get_available_gpu_power_profiles().await.expect("get");
        assert_eq!(
            profiles,
            &[
                (GPUPowerProfile::Capped as u32, String::from("CAPPED")),
                (GPUPowerProfile::Uncapped as u32, String::from("UNCAPPED"))
            ]
        );
    }

    #[tokio::test]
    async fn read_power_profile() {
        let _h = testing::start();

        setup().await.expect("setup");
        let base = find_hwmon(GPU_HWMON_NAME).await.unwrap();
        let filename = base.join(GPU_POWER_PROFILE_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        let contents = " 1 3D_FULL_SCREEN
 3          VIDEO*
 4             VR
 5        COMPUTE
 6         CUSTOM
 8         CAPPED
 9       UNCAPPED";

        write(filename.as_path(), contents).await.expect("write");

        fake_model(SteamDeckVariant::Unknown)
            .await
            .expect("fake_model");
        assert_eq!(
            get_gpu_power_profile().await.expect("get"),
            GPUPowerProfile::Video
        );

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");
        assert_eq!(
            get_gpu_power_profile().await.expect("get"),
            GPUPowerProfile::Video
        );
    }

    #[tokio::test]
    async fn read_no_power_profile() {
        let _h = testing::start();

        setup().await.expect("setup");
        let base = find_hwmon(GPU_HWMON_NAME).await.unwrap();
        let filename = base.join(GPU_POWER_PROFILE_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        let contents = " 1 3D_FULL_SCREEN
 3          VIDEO
 4             VR
 5        COMPUTE
 6         CUSTOM
 8         CAPPED
 9       UNCAPPED";

        write(filename.as_path(), contents).await.expect("write");

        fake_model(SteamDeckVariant::Unknown)
            .await
            .expect("fake_model");
        assert!(get_gpu_power_profile().await.is_err());

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");
        assert!(get_gpu_power_profile().await.is_err());
    }

    #[tokio::test]
    async fn read_unknown_power_profile() {
        let _h = testing::start();

        setup().await.expect("setup");
        let base = find_hwmon(GPU_HWMON_NAME).await.unwrap();
        let filename = base.join(GPU_POWER_PROFILE_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        let contents = " 1 3D_FULL_SCREEN
 2            CGA*
 3          VIDEO
 4             VR
 5        COMPUTE
 6         CUSTOM
 8         CAPPED
 9       UNCAPPED";

        write(filename.as_path(), contents).await.expect("write");

        fake_model(SteamDeckVariant::Unknown)
            .await
            .expect("fake_model");
        assert!(get_gpu_power_profile().await.is_err());

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");
        assert!(get_gpu_power_profile().await.is_err());
    }

    #[tokio::test]
    async fn read_cpu_available_governors() {
        let _h = testing::start();

        let base = path(CPU_PREFIX).join(CPU0_NAME);
        create_dir_all(&base).await.expect("create_dir_all");

        let contents = "conservative ondemand userspace powersave performance schedutil";
        write(base.join(CPU_SCALING_AVAILABLE_GOVERNORS_SUFFIX), contents)
            .await
            .expect("write");

        assert_eq!(
            get_available_cpu_scaling_governors().await.unwrap(),
            vec![
                CPUScalingGovernor::Conservative,
                CPUScalingGovernor::OnDemand,
                CPUScalingGovernor::UserSpace,
                CPUScalingGovernor::PowerSave,
                CPUScalingGovernor::Performance,
                CPUScalingGovernor::SchedUtil
            ]
        );
    }

    #[tokio::test]
    async fn read_invalid_cpu_available_governors() {
        let _h = testing::start();

        let base = path(CPU_PREFIX).join(CPU0_NAME);
        create_dir_all(&base).await.expect("create_dir_all");

        let contents =
            "conservative ondemand userspace rescascade powersave performance schedutil\n";
        write(base.join(CPU_SCALING_AVAILABLE_GOVERNORS_SUFFIX), contents)
            .await
            .expect("write");

        assert_eq!(
            get_available_cpu_scaling_governors().await.unwrap(),
            vec![
                CPUScalingGovernor::Conservative,
                CPUScalingGovernor::OnDemand,
                CPUScalingGovernor::UserSpace,
                CPUScalingGovernor::PowerSave,
                CPUScalingGovernor::Performance,
                CPUScalingGovernor::SchedUtil
            ]
        );
    }

    #[tokio::test]
    async fn read_cpu_governor() {
        let _h = testing::start();

        let base = path(CPU_PREFIX).join(CPU0_NAME);
        create_dir_all(&base).await.expect("create_dir_all");

        let contents = "ondemand\n";
        write(base.join(CPU_SCALING_GOVERNOR_SUFFIX), contents)
            .await
            .expect("write");

        assert_eq!(
            get_cpu_scaling_governor().await.unwrap(),
            CPUScalingGovernor::OnDemand
        );
    }

    #[tokio::test]
    async fn read_invalid_cpu_governor() {
        let _h = testing::start();

        let base = path(CPU_PREFIX).join(CPU0_NAME);
        create_dir_all(&base).await.expect("create_dir_all");

        let contents = "rescascade\n";
        write(base.join(CPU_SCALING_GOVERNOR_SUFFIX), contents)
            .await
            .expect("write");

        assert!(get_cpu_scaling_governor().await.is_err());
    }

    #[tokio::test]
    async fn read_max_charge_level() {
        let handle = testing::start();

        let mut platform_config = PlatformConfig::default();
        platform_config.battery_charge_limit = Some(BatteryChargeLimitConfig {
            suggested_minimum_limit: Some(10),
            hwmon_name: String::from("steamdeck_hwmon"),
            attribute: String::from("max_battery_charge_level"),
        });
        handle.test.platform_config.replace(Some(platform_config));

        let base = path(HWMON_PREFIX).join("hwmon6");
        create_dir_all(&base).await.expect("create_dir_all");

        write(base.join("name"), "steamdeck_hwmon\n")
            .await
            .expect("write");

        assert_eq!(
            find_hwmon("steamdeck_hwmon").await.unwrap(),
            path(HWMON_PREFIX).join("hwmon6")
        );

        write(base.join("max_battery_charge_level"), "10\n")
            .await
            .expect("write");

        assert_eq!(get_max_charge_level().await.unwrap(), 10);

        set_max_charge_level(99).await.expect("set");
        assert_eq!(get_max_charge_level().await.unwrap(), 99);

        set_max_charge_level(0).await.expect("set");
        assert_eq!(get_max_charge_level().await.unwrap(), 0);

        assert!(set_max_charge_level(101).await.is_err());
        assert!(set_max_charge_level(-1).await.is_err());
    }
}
