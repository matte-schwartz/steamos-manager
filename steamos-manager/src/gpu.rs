/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{anyhow, bail, ensure, Result};
use async_trait::async_trait;
use num_enum::TryFromPrimitive;
use regex::Regex;
use std::fmt::Display;
use std::ops::RangeInclusive;
use std::path::Path;
use std::str::FromStr;
use std::sync::LazyLock;
use strum::{Display, EnumString, VariantNames};
use tokio::fs::{self, try_exists, File};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::error;

use crate::hardware::{device_config, device_type};
use crate::power::find_hwmon;
use crate::write_synced;

pub(crate) const AMDGPU_HWMON_NAME: &str = "amdgpu";

static AMDGPU_POWER_PROFILE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?<value>[0-9]+)\s+(?<name>[0-9A-Za-z_]+)(?<active>\*)?").unwrap()
});
static AMDGPU_CLOCK_LEVELS_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?<index>[0-9]+): (?<value>[0-9]+)Mhz").unwrap());

#[derive(PartialEq, Debug, Copy, Clone)]
pub enum GpuPowerProfile {
    Amdgpu(AmdgpuPowerProfile),
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(serialize_all = "snake_case")]
#[repr(u32)]
pub enum AmdgpuPowerProfile {
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

#[derive(PartialEq, Debug, Copy, Clone)]
pub enum GpuPerformanceLevel {
    Amdgpu(AmdgpuPerformanceLevel),
    Intel(IntelPerformanceLevel),
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone)]
#[strum(serialize_all = "snake_case")]
pub enum AmdgpuPerformanceLevel {
    Auto,
    Low,
    High,
    Manual,
    ProfilePeak,
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone)]
#[strum(serialize_all = "snake_case")]
pub enum IntelPerformanceLevel {
    Auto,
    Manual,
}

#[derive(Display, EnumString, VariantNames, PartialEq, Debug, Clone)]
#[strum(serialize_all = "snake_case")]
pub enum GpuPowerProfileDriverType {
    Amdgpu,
}

#[derive(Display, EnumString, VariantNames, PartialEq, Debug, Clone)]
#[strum(serialize_all = "snake_case")]
pub enum GpuPerformanceLevelDriverType {
    Amdgpu,
    Intel,
}

#[derive(Debug)]
pub(crate) struct AmdgpuPowerProfileDriver {}

#[derive(Debug)]
pub(crate) struct AmdgpuPerformanceLevelDriver {}

#[derive(Debug)]
pub(crate) struct IntelPerformanceLevelDriver {
    path: std::path::PathBuf,
    driver_type: IntelGpuDriverType,
}

#[derive(Debug, PartialEq)]
enum IntelGpuDriverType {
    I915,
    Xe,
}

#[async_trait]
pub(crate) trait GpuPowerProfileDriver: Send + Sync {
    fn power_profile_from_str(&self, value: &str) -> Result<GpuPowerProfile>;
    async fn get_available_power_profiles(&self) -> Result<Vec<(u32, String)>>;
    async fn get_power_profile(&self) -> Result<GpuPowerProfile>;
    async fn set_power_profile(&self, value: GpuPowerProfile) -> Result<()>;
}

#[async_trait]
pub(crate) trait GpuPerformanceLevelDriver: Send + Sync {
    fn performance_level_from_str(&self, value: &str) -> Result<GpuPerformanceLevel>;
    async fn get_available_performance_levels(&self) -> Result<Vec<GpuPerformanceLevel>>;
    async fn get_performance_level(&self) -> Result<GpuPerformanceLevel>;
    async fn set_performance_level(&self, level: GpuPerformanceLevel) -> Result<()>;

    async fn get_clocks_range(&self) -> Result<RangeInclusive<u32>>;
    async fn get_clocks(&self) -> Result<u32>;
    async fn set_clocks(&self, clocks: u32) -> Result<()>;
}

pub(crate) async fn gpu_power_profile_driver() -> Result<Box<dyn GpuPowerProfileDriver>> {
    let driver = AmdgpuPowerProfileDriver {};
    if !driver
        .get_available_power_profiles()
        .await
        .unwrap_or_default()
        .is_empty()
    {
        return Ok(Box::new(driver));
    }
    bail!("No valid GPU power profile driver found");
}

pub(crate) async fn gpu_performance_level_driver() -> Result<Box<dyn GpuPerformanceLevelDriver>> {
    // Prefer amdgpu when present
    if AmdgpuPerformanceLevelDriver::is_supported()
        .await
        .unwrap_or(false)
    {
        return Ok(Box::new(AmdgpuPerformanceLevelDriver {}));
    }

    // Fallback: Intel i915/xe
    if let Ok(intel_driver) = IntelPerformanceLevelDriver::new().await {
        return Ok(Box::new(intel_driver));
    }

    bail!("No valid GPU performance level driver found");
}

impl Display for GpuPerformanceLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            GpuPerformanceLevel::Amdgpu(v) => write!(f, "{v}"),
            GpuPerformanceLevel::Intel(v) => write!(f, "{v}"),
        }
    }
}

impl Display for GpuPowerProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            GpuPowerProfile::Amdgpu(v) => write!(f, "{v}"),
        }
    }
}

trait AmdgpuGpuPerfDriver {
    async fn read_sysfs_contents<S: AsRef<Path>>(suffix: S) -> Result<String> {
        // Read a given suffix for the GPU
        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;
        fs::read_to_string(base.join(suffix.as_ref()))
            .await
            .map_err(|message| anyhow!("Error opening sysfs file for reading {message}"))
    }

    async fn write_sysfs_contents<S: AsRef<Path>>(suffix: S, data: &[u8]) -> Result<()> {
        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;
        write_synced(base.join(suffix), data)
            .await
            .inspect_err(|message| error!("Error writing to sysfs file: {message}"))
    }
}

impl AmdgpuPowerProfileDriver {
    const POWER_PROFILE_SUFFIX: &str = "device/pp_power_profile_mode";
}

impl AmdgpuGpuPerfDriver for AmdgpuPowerProfileDriver {}

#[async_trait]
impl GpuPowerProfileDriver for AmdgpuPowerProfileDriver {
    fn power_profile_from_str(&self, value: &str) -> Result<GpuPowerProfile> {
        Ok(GpuPowerProfile::Amdgpu(AmdgpuPowerProfile::from_str(
            value,
        )?))
    }

    async fn get_power_profile(&self) -> Result<GpuPowerProfile> {
        // check which profile is current and return if possible
        let contents = Self::read_sysfs_contents(Self::POWER_PROFILE_SUFFIX).await?;

        // NOTE: We don't filter based on deck here because the sysfs
        // firmware support setting the value to no-op values.
        let lines = contents.lines();
        for line in lines {
            let Some(caps) = AMDGPU_POWER_PROFILE_REGEX.captures(line) else {
                continue;
            };

            let name = &caps["name"].to_lowercase();
            if caps.name("active").is_some() {
                match AmdgpuPowerProfile::from_str(name.as_str()) {
                    Ok(v) => {
                        return Ok(GpuPowerProfile::Amdgpu(v));
                    }
                    Err(e) => bail!("Unable to parse value for GPU power profile: {e}"),
                }
            }
        }
        bail!("Unable to determine current GPU power profile");
    }

    async fn get_available_power_profiles(&self) -> Result<Vec<(u32, String)>> {
        let contents = Self::read_sysfs_contents(Self::POWER_PROFILE_SUFFIX).await?;
        let deck = device_type().await.unwrap_or_default() == "steam_deck";

        let mut map = Vec::new();
        let lines = contents.lines();
        for line in lines {
            let Some(caps) = AMDGPU_POWER_PROFILE_REGEX.captures(line) else {
                continue;
            };
            let value: u32 = caps["value"].parse().map_err(|message| {
                anyhow!("Unable to parse value for GPU power profile: {message}")
            })?;
            let name = &caps["name"];
            if deck {
                // Deck is designed to operate in one of the CAPPED or UNCAPPED power profiles,
                // the other profiles aren't correctly tuned for the hardware.
                if value == AmdgpuPowerProfile::Capped as u32
                    || value == AmdgpuPowerProfile::Uncapped as u32
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

    async fn set_power_profile(&self, value: GpuPowerProfile) -> Result<()> {
        #[allow(irrefutable_let_patterns)] // Remove when more values are added
        let GpuPowerProfile::Amdgpu(value) = value
        else {
            bail!("This is not an amdgpu-compatible profile");
        };
        let profile = (value as u32).to_string();
        Self::write_sysfs_contents(Self::POWER_PROFILE_SUFFIX, profile.as_bytes()).await
    }
}

impl AmdgpuPerformanceLevelDriver {
    const CLOCKS_SUFFIX: &str = "device/pp_od_clk_voltage";
    const CLOCK_LEVELS_SUFFIX: &str = "device/pp_dpm_sclk";
    const PERFORMANCE_LEVEL_SUFFIX: &str = "device/power_dpm_force_performance_level";

    // Probe for amdgpu perf-level support by checking the performance-level node.
    async fn is_supported() -> Result<bool> {
        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;
        Ok(try_exists(base.join(Self::PERFORMANCE_LEVEL_SUFFIX)).await?)
    }
}

impl AmdgpuGpuPerfDriver for AmdgpuPerformanceLevelDriver {}

#[async_trait]
impl GpuPerformanceLevelDriver for AmdgpuPerformanceLevelDriver {
    fn performance_level_from_str(&self, value: &str) -> Result<GpuPerformanceLevel> {
        Ok(GpuPerformanceLevel::Amdgpu(
            AmdgpuPerformanceLevel::from_str(value)?,
        ))
    }

    async fn get_available_performance_levels(&self) -> Result<Vec<GpuPerformanceLevel>> {
        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;
        if try_exists(base.join(Self::PERFORMANCE_LEVEL_SUFFIX)).await? {
            Ok(vec![
                GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Auto),
                GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Low),
                GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::High),
                GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Manual),
                GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::ProfilePeak),
            ])
        } else {
            Ok(Vec::new())
        }
    }

    async fn get_performance_level(&self) -> Result<GpuPerformanceLevel> {
        let level = Self::read_sysfs_contents(Self::PERFORMANCE_LEVEL_SUFFIX).await?;
        Ok(GpuPerformanceLevel::Amdgpu(
            AmdgpuPerformanceLevel::from_str(level.trim())?,
        ))
    }

    async fn set_performance_level(&self, level: GpuPerformanceLevel) -> Result<()> {
        #[allow(irrefutable_let_patterns)] // Remove when more values are added
        let GpuPerformanceLevel::Amdgpu(level) = level
        else {
            bail!("This is not an amdgpu-compatible performance level");
        };
        let level: String = level.to_string();
        Self::write_sysfs_contents(Self::PERFORMANCE_LEVEL_SUFFIX, level.as_bytes()).await
    }

    async fn get_clocks_range(&self) -> Result<RangeInclusive<u32>> {
        if let Some(range) = device_config()
            .await?
            .as_ref()
            .and_then(|config| config.gpu_clocks)
        {
            return Ok(range.min..=range.max);
        }
        let contents = Self::read_sysfs_contents(Self::CLOCK_LEVELS_SUFFIX).await?;
        let lines = contents.lines();
        let mut min = 1_000_000;
        let mut max = 0;

        for line in lines {
            let Some(caps) = AMDGPU_CLOCK_LEVELS_REGEX.captures(line) else {
                continue;
            };
            let value: u32 = caps["value"].parse().map_err(|message| {
                anyhow!("Unable to parse value for GPU power profile: {message}")
            })?;
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

    async fn set_clocks(&self, clocks: u32) -> Result<()> {
        // Set GPU clocks to given value valid
        // Only used when GPU Performance Level is manual, but write whenever called.
        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;
        let mut myfile = File::create(base.join(Self::CLOCKS_SUFFIX))
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

    async fn get_clocks(&self) -> Result<u32> {
        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;
        let clocks_file = File::open(base.join(Self::CLOCKS_SUFFIX)).await?;
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
}

impl IntelPerformanceLevelDriver {
    async fn new() -> Result<Self> {
        for card_num in 0..4 {
            let card_path = std::path::PathBuf::from(format!("/sys/class/drm/card{card_num}"));
            if !try_exists(&card_path).await? {
                continue;
            }

            // Check for Intel Xe
            if try_exists(card_path.join("device/tile0/gt0/freq0/min_freq")).await? {
                return Ok(IntelPerformanceLevelDriver {
                    path: card_path,
                    driver_type: IntelGpuDriverType::Xe,
                });
            }

            // Check for Intel i915
            if try_exists(card_path.join("gt_min_freq_mhz")).await? {
                return Ok(IntelPerformanceLevelDriver {
                    path: card_path,
                    driver_type: IntelGpuDriverType::I915,
                });
            }
        }
        bail!("No supported Intel GPU found")
    }

    fn min_freq_path(&self) -> std::path::PathBuf {
        match self.driver_type {
            IntelGpuDriverType::I915 => self.path.join("gt_min_freq_mhz"),
            IntelGpuDriverType::Xe => self.path.join("device/tile0/gt0/freq0/min_freq"),
        }
    }

    fn max_freq_path(&self) -> std::path::PathBuf {
        match self.driver_type {
            IntelGpuDriverType::I915 => self.path.join("gt_max_freq_mhz"),
            IntelGpuDriverType::Xe => self.path.join("device/tile0/gt0/freq0/max_freq"),
        }
    }

    fn rpn_freq_path(&self) -> std::path::PathBuf {
        match self.driver_type {
            IntelGpuDriverType::I915 => self.path.join("gt_RPn_freq_mhz"),
            IntelGpuDriverType::Xe => self.path.join("device/tile0/gt0/freq0/rpn_freq"),
        }
    }

    fn rp0_freq_path(&self) -> std::path::PathBuf {
        match self.driver_type {
            IntelGpuDriverType::I915 => self.path.join("gt_RP0_freq_mhz"),
            IntelGpuDriverType::Xe => self.path.join("device/tile0/gt0/freq0/rp0_freq"),
        }
    }

    async fn read_freq(&self, path: std::path::PathBuf) -> Result<u32> {
        let val_str = fs::read_to_string(&path).await.map_err(|e| {
            anyhow!(
                "Failed to read intel gpu frequency from {}: {}",
                path.display(),
                e
            )
        })?;
        val_str.trim().parse::<u32>().map_err(|e| {
            anyhow!(
                "Failed to parse intel gpu frequency '{}': {e}",
                val_str.trim()
            )
        })
    }

    async fn write_freq(&self, path: std::path::PathBuf, clocks: u32) -> Result<()> {
        write_synced(path, clocks.to_string().as_bytes()).await
    }

    async fn get_min_clocks(&self) -> Result<u32> {
        self.read_freq(self.min_freq_path()).await
    }

    async fn set_min_clocks(&self, clocks: u32) -> Result<()> {
        self.write_freq(self.min_freq_path(), clocks).await
    }

    async fn get_max_clocks(&self) -> Result<u32> {
        self.read_freq(self.max_freq_path()).await
    }

    async fn set_max_clocks(&self, clocks: u32) -> Result<()> {
        self.write_freq(self.max_freq_path(), clocks).await
    }
}

#[async_trait]
impl GpuPerformanceLevelDriver for IntelPerformanceLevelDriver {
    fn performance_level_from_str(&self, value: &str) -> Result<GpuPerformanceLevel> {
        Ok(GpuPerformanceLevel::Intel(IntelPerformanceLevel::from_str(
            value,
        )?))
    }

    async fn get_available_performance_levels(&self) -> Result<Vec<GpuPerformanceLevel>> {
        Ok(vec![
            GpuPerformanceLevel::Intel(IntelPerformanceLevel::Auto),
            GpuPerformanceLevel::Intel(IntelPerformanceLevel::Manual),
        ])
    }

    async fn get_performance_level(&self) -> Result<GpuPerformanceLevel> {
        let range = self.get_clocks_range().await?;
        let min_clock = self.get_min_clocks().await?;
        let max_clock = self.get_max_clocks().await?;

        if min_clock == *range.start() && max_clock == *range.end() {
            Ok(GpuPerformanceLevel::Intel(IntelPerformanceLevel::Auto))
        } else {
            Ok(GpuPerformanceLevel::Intel(IntelPerformanceLevel::Manual))
        }
    }

    async fn set_performance_level(&self, level: GpuPerformanceLevel) -> Result<()> {
        let GpuPerformanceLevel::Intel(level) = level else {
            bail!("This is not an Intel-compatible performance level");
        };

        match level {
            IntelPerformanceLevel::Auto => {
                let range = self.get_clocks_range().await?;
                self.set_min_clocks(*range.start()).await?;
                self.set_max_clocks(*range.end()).await
            }
            IntelPerformanceLevel::Manual => {
                // In manual mode, user is expected to set clocks separately.
                Ok(())
            }
        }
    }

    async fn get_clocks_range(&self) -> Result<RangeInclusive<u32>> {
        if let Some(range) = device_config()
            .await?
            .as_ref()
            .and_then(|config| config.gpu_clocks)
        {
            return Ok(range.min..=range.max);
        }
        let min = self.read_freq(self.rpn_freq_path()).await?;
        let max = self.read_freq(self.rp0_freq_path()).await?;
        Ok(min..=max)
    }

    async fn get_clocks(&self) -> Result<u32> {
        // For Intel, "get_clocks" returns the current minimum frequency,
        // as that's what's often manipulated in manual mode.
        self.get_min_clocks().await
    }

    async fn set_clocks(&self, clocks: u32) -> Result<()> {
        // Set both min and max to the same value to fix the frequency
        self.set_min_clocks(clocks).await?;
        self.set_max_clocks(clocks).await
    }
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;
    use crate::hardware::test::fake_model;
    use crate::hardware::SteamDeckVariant;
    use crate::power::HWMON_PREFIX;
    use crate::{enum_roundtrip, path, testing};
    use tokio::fs::{create_dir_all, read_to_string, write};

    pub async fn setup() -> Result<()> {
        // Use hwmon5 just as a test. We needed a subfolder of HWMON_PREFIX
        // and this is as good as any.
        let base = path(HWMON_PREFIX).join("hwmon5");
        let filename = base.join(AmdgpuPerformanceLevelDriver::PERFORMANCE_LEVEL_SUFFIX);
        // Creates hwmon path, including device subpath
        create_dir_all(filename.parent().unwrap()).await?;
        // Writes name file as addgpu so find_hwmon() will find it.
        write_synced(base.join("name"), AMDGPU_HWMON_NAME.as_bytes()).await?;
        Ok(())
    }

    pub async fn create_nodes() -> Result<()> {
        setup().await?;
        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;

        let filename = base.join(AmdgpuPerformanceLevelDriver::PERFORMANCE_LEVEL_SUFFIX);
        write(filename.as_path(), "auto\n").await?;

        let filename = base.join(AmdgpuPowerProfileDriver::POWER_PROFILE_SUFFIX);
        let contents = " 1 3D_FULL_SCREEN
 3          VIDEO*
 4             VR
 5        COMPUTE
 6         CUSTOM
 8         CAPPED
 9       UNCAPPED";
        write(filename.as_path(), contents).await?;

        Ok(())
    }

    pub async fn write_clocks(mhz: u32) {
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPerformanceLevelDriver::CLOCKS_SUFFIX);
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
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        read_to_string(base.join(AmdgpuPerformanceLevelDriver::CLOCKS_SUFFIX)).await
    }

    pub fn format_clocks(mhz: u32) -> String {
        format!("s 0 {mhz}\ns 1 {mhz}\nc\n")
    }

    #[tokio::test]
    async fn test_get_gpu_performance_level() {
        let _h = testing::start();
        let driver = AmdgpuPerformanceLevelDriver {};

        setup().await.expect("setup");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPerformanceLevelDriver::PERFORMANCE_LEVEL_SUFFIX);
        assert!(driver.get_performance_level().await.is_err());

        write(filename.as_path(), "auto\n").await.expect("write");
        assert_eq!(
            driver.get_performance_level().await.unwrap(),
            GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Auto)
        );

        write(filename.as_path(), "low\n").await.expect("write");
        assert_eq!(
            driver.get_performance_level().await.unwrap(),
            GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Low)
        );

        write(filename.as_path(), "high\n").await.expect("write");
        assert_eq!(
            driver.get_performance_level().await.unwrap(),
            GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::High)
        );

        write(filename.as_path(), "manual\n").await.expect("write");
        assert_eq!(
            driver.get_performance_level().await.unwrap(),
            GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Manual)
        );

        write(filename.as_path(), "profile_peak\n")
            .await
            .expect("write");
        assert_eq!(
            driver.get_performance_level().await.unwrap(),
            GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::ProfilePeak)
        );

        write(filename.as_path(), "nothing\n").await.expect("write");
        assert!(driver.get_performance_level().await.is_err());
    }

    #[tokio::test]
    async fn test_set_gpu_performance_level() {
        let _h = testing::start();
        let driver = AmdgpuPerformanceLevelDriver {};

        setup().await.expect("setup");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPerformanceLevelDriver::PERFORMANCE_LEVEL_SUFFIX);

        driver
            .set_performance_level(GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Auto))
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "auto"
        );
        driver
            .set_performance_level(GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Low))
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "low"
        );
        driver
            .set_performance_level(GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::High))
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "high"
        );
        driver
            .set_performance_level(GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Manual))
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "manual"
        );
        driver
            .set_performance_level(GpuPerformanceLevel::Amdgpu(
                AmdgpuPerformanceLevel::ProfilePeak,
            ))
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "profile_peak"
        );
    }

    #[tokio::test]
    async fn test_get_gpu_clocks() {
        let _h = testing::start();
        let driver = AmdgpuPerformanceLevelDriver {};

        assert!(driver.get_clocks().await.is_err());
        setup().await.expect("setup");

        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPerformanceLevelDriver::CLOCKS_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");
        write(filename.as_path(), b"").await.expect("write");

        assert_eq!(driver.get_clocks().await.unwrap(), 0);
        write_clocks(1600).await;

        assert_eq!(driver.get_clocks().await.unwrap(), 1600);
    }

    #[tokio::test]
    async fn test_set_gpu_clocks() {
        let _h = testing::start();
        let driver = AmdgpuPerformanceLevelDriver {};

        assert!(driver.set_clocks(1600).await.is_err());
        setup().await.expect("setup");

        assert!(driver.set_clocks(200).await.is_ok());

        assert_eq!(read_clocks().await.unwrap(), format_clocks(200));

        assert!(driver.set_clocks(1600).await.is_ok());
        assert_eq!(read_clocks().await.unwrap(), format_clocks(1600));
    }

    #[tokio::test]
    async fn test_get_gpu_clocks_range() {
        let _h = testing::start();
        let driver = AmdgpuPerformanceLevelDriver {};

        setup().await.expect("setup");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPerformanceLevelDriver::CLOCK_LEVELS_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        assert!(driver.get_clocks_range().await.is_err());

        write(filename.as_path(), &[] as &[u8; 0])
            .await
            .expect("write");
        assert!(driver.get_clocks_range().await.is_err());

        let contents = "0: 200Mhz *
1: 1100Mhz
2: 1600Mhz";
        write(filename.as_path(), contents).await.expect("write");
        assert_eq!(driver.get_clocks_range().await.unwrap(), 200..=1600);

        let contents = "0: 1600Mhz *
1: 200Mhz
2: 1100Mhz";
        write(filename.as_path(), contents).await.expect("write");
        assert_eq!(driver.get_clocks_range().await.unwrap(), 200..=1600);
    }

    #[test]
    fn gpu_power_profile_roundtrip() {
        enum_roundtrip!(AmdgpuPowerProfile {
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
        assert!(AmdgpuPowerProfile::try_from(0).is_err());
        assert!(AmdgpuPowerProfile::try_from(2).is_err());
        assert!(AmdgpuPowerProfile::try_from(10).is_err());
        assert!(AmdgpuPowerProfile::from_str("fullscreen").is_err());
    }

    #[test]
    fn gpu_performance_level_roundtrip() {
        enum_roundtrip!(AmdgpuPerformanceLevel {
            "auto": str = Auto,
            "low": str = Low,
            "high": str = High,
            "manual": str = Manual,
            "profile_peak": str = ProfilePeak,
        });
        assert!(AmdgpuPerformanceLevel::from_str("peak_performance").is_err());
    }

    #[tokio::test]
    async fn read_power_profiles() {
        let _h = testing::start();
        let driver = AmdgpuPowerProfileDriver {};

        setup().await.expect("setup");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPowerProfileDriver::POWER_PROFILE_SUFFIX);
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

        let profiles = driver.get_available_power_profiles().await.expect("get");
        assert_eq!(
            profiles,
            &[
                (
                    AmdgpuPowerProfile::FullScreen as u32,
                    String::from("3D_FULL_SCREEN")
                ),
                (AmdgpuPowerProfile::Video as u32, String::from("VIDEO")),
                (AmdgpuPowerProfile::VR as u32, String::from("VR")),
                (AmdgpuPowerProfile::Compute as u32, String::from("COMPUTE")),
                (AmdgpuPowerProfile::Custom as u32, String::from("CUSTOM")),
                (AmdgpuPowerProfile::Capped as u32, String::from("CAPPED")),
                (
                    AmdgpuPowerProfile::Uncapped as u32,
                    String::from("UNCAPPED")
                )
            ]
        );

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");

        let profiles = driver.get_available_power_profiles().await.expect("get");
        assert_eq!(
            profiles,
            &[
                (AmdgpuPowerProfile::Capped as u32, String::from("CAPPED")),
                (
                    AmdgpuPowerProfile::Uncapped as u32,
                    String::from("UNCAPPED")
                )
            ]
        );
    }

    #[tokio::test]
    async fn read_unknown_power_profiles() {
        let _h = testing::start();
        let driver = AmdgpuPowerProfileDriver {};

        setup().await.expect("setup");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPowerProfileDriver::POWER_PROFILE_SUFFIX);
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

        let profiles = driver.get_available_power_profiles().await.expect("get");
        assert_eq!(
            profiles,
            &[
                (
                    AmdgpuPowerProfile::FullScreen as u32,
                    String::from("3D_FULL_SCREEN")
                ),
                (2, String::from("CGA")),
                (AmdgpuPowerProfile::Video as u32, String::from("VIDEO")),
                (AmdgpuPowerProfile::VR as u32, String::from("VR")),
                (AmdgpuPowerProfile::Compute as u32, String::from("COMPUTE")),
                (AmdgpuPowerProfile::Custom as u32, String::from("CUSTOM")),
                (AmdgpuPowerProfile::Capped as u32, String::from("CAPPED")),
                (
                    AmdgpuPowerProfile::Uncapped as u32,
                    String::from("UNCAPPED")
                )
            ]
        );

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");

        let profiles = driver.get_available_power_profiles().await.expect("get");
        assert_eq!(
            profiles,
            &[
                (AmdgpuPowerProfile::Capped as u32, String::from("CAPPED")),
                (
                    AmdgpuPowerProfile::Uncapped as u32,
                    String::from("UNCAPPED")
                )
            ]
        );
    }

    #[tokio::test]
    async fn read_power_profile() {
        let _h = testing::start();
        let driver = AmdgpuPowerProfileDriver {};

        setup().await.expect("setup");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPowerProfileDriver::POWER_PROFILE_SUFFIX);
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
            driver.get_power_profile().await.expect("get"),
            GpuPowerProfile::Amdgpu(AmdgpuPowerProfile::Video)
        );

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");
        assert_eq!(
            driver.get_power_profile().await.expect("get"),
            GpuPowerProfile::Amdgpu(AmdgpuPowerProfile::Video)
        );
    }

    #[tokio::test]
    async fn read_no_power_profile() {
        let _h = testing::start();
        let driver = AmdgpuPowerProfileDriver {};

        setup().await.expect("setup");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPowerProfileDriver::POWER_PROFILE_SUFFIX);
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
        assert!(driver.get_power_profile().await.is_err());

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");
        assert!(driver.get_power_profile().await.is_err());
    }

    #[tokio::test]
    async fn read_unknown_power_profile() {
        let _h = testing::start();
        let driver = AmdgpuPowerProfileDriver {};

        setup().await.expect("setup");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPowerProfileDriver::POWER_PROFILE_SUFFIX);
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
        assert!(driver.get_power_profile().await.is_err());

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");
        assert!(driver.get_power_profile().await.is_err());
    }
}
