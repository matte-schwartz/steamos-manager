/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{bail, ensure, Result};
use num_enum::TryFromPrimitive;
use std::str::FromStr;
use strum::{Display, EnumString};
use tokio::fs;
use zbus::Connection;

use crate::path;
use crate::platform::{platform_config, ServiceConfig};
use crate::process::{run_script, script_exit_code};
use crate::systemd::SystemdUnit;

const SYS_VENDOR_PATH: &str = "/sys/class/dmi/id/sys_vendor";
const BOARD_NAME_PATH: &str = "/sys/class/dmi/id/board_name";
const PRODUCT_NAME_PATH: &str = "/sys/class/dmi/id/product_name";

#[derive(Display, EnumString, PartialEq, Debug, Default, Copy, Clone)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub(crate) enum SteamDeckVariant {
    #[default]
    Unknown,
    Jupiter,
    Galileo,
}

#[derive(Display, EnumString, PartialEq, Debug, Default, Copy, Clone)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub(crate) enum DeviceType {
    #[default]
    Unknown,
    SteamDeck,
    LegionGo,
    LegionGoS,
    RogAlly,
    RogAllyX,
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(ascii_case_insensitive)]
#[repr(u32)]
pub enum FanControlState {
    #[strum(to_string = "BIOS")]
    Bios = 0,
    #[strum(to_string = "OS")]
    Os = 1,
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(ascii_case_insensitive)]
#[repr(u32)]
pub enum FactoryResetKind {
    User = 1,
    OS = 2,
    All = 3,
}

pub(crate) async fn steam_deck_variant() -> Result<SteamDeckVariant> {
    let sys_vendor = fs::read_to_string(path(SYS_VENDOR_PATH)).await?;
    if sys_vendor.trim_end() != "Valve" {
        return Ok(SteamDeckVariant::Unknown);
    }
    let board_name = fs::read_to_string(path(BOARD_NAME_PATH)).await?;
    Ok(SteamDeckVariant::from_str(board_name.trim_end()).unwrap_or_default())
}

pub(crate) async fn device_type() -> Result<DeviceType> {
    Ok(device_variant().await?.0)
}

pub(crate) async fn device_variant() -> Result<(DeviceType, String)> {
    let sys_vendor = fs::read_to_string(path(SYS_VENDOR_PATH)).await?;
    let product_name = fs::read_to_string(path(PRODUCT_NAME_PATH)).await?;
    let product_name = product_name.trim_end();
    let board_name = fs::read_to_string(path(BOARD_NAME_PATH)).await?;
    let board_name = board_name.trim_end();
    Ok(match (sys_vendor.trim_end(), product_name, board_name) {
        ("ASUSTeK COMPUTER INC.", _, "RC71L") => (DeviceType::RogAlly, board_name.to_string()),
        ("ASUSTeK COMPUTER INC.", _, "RC72LA") => (DeviceType::RogAllyX, board_name.to_string()),
        ("LENOVO", "83E1", _) => (DeviceType::LegionGo, product_name.to_string()),
        ("LENOVO", "83L3" | "83N6" | "83Q2" | "83Q3", _) => {
            (DeviceType::LegionGoS, product_name.to_string())
        }
        ("Valve", _, "Jupiter" | "Galileo") => (DeviceType::SteamDeck, board_name.to_string()),
        _ => (DeviceType::Unknown, String::from("unknown")),
    })
}

pub(crate) struct FanControl {
    connection: Connection,
}

impl FanControl {
    pub fn new(connection: Connection) -> FanControl {
        FanControl { connection }
    }

    pub async fn get_state(&self) -> Result<FanControlState> {
        let config = platform_config().await?;
        match config
            .as_ref()
            .and_then(|config| config.fan_control.as_ref())
        {
            Some(ServiceConfig::Systemd(service)) => {
                let jupiter_fan_control =
                    SystemdUnit::new(self.connection.clone(), service).await?;
                let active = jupiter_fan_control.active().await?;
                Ok(if active {
                    FanControlState::Os
                } else {
                    FanControlState::Bios
                })
            }
            Some(ServiceConfig::Script {
                start: _,
                stop: _,
                status,
            }) => {
                let res = script_exit_code(&status.script, &status.script_args).await?;
                ensure!(res >= 0, "Script exited abnormally");
                Ok(FanControlState::try_from(res as u32)?)
            }
            None => bail!("Fan control not configured"),
        }
    }

    pub async fn set_state(&self, state: FanControlState) -> Result<()> {
        // Run what steamos-polkit-helpers/jupiter-fan-control does
        let config = platform_config().await?;
        match config
            .as_ref()
            .and_then(|config| config.fan_control.as_ref())
        {
            Some(ServiceConfig::Systemd(service)) => {
                let jupiter_fan_control =
                    SystemdUnit::new(self.connection.clone(), service).await?;
                match state {
                    FanControlState::Os => jupiter_fan_control.start().await,
                    FanControlState::Bios => jupiter_fan_control.stop().await,
                }
            }
            Some(ServiceConfig::Script {
                start,
                stop,
                status: _,
            }) => match state {
                FanControlState::Os => run_script(&start.script, &start.script_args).await,
                FanControlState::Bios => run_script(&stop.script, &stop.script_args).await,
            },
            None => bail!("Fan control not configured"),
        }
    }
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::error::to_zbus_fdo_error;
    use crate::platform::{PlatformConfig, ServiceConfig};
    use crate::{enum_roundtrip, testing};
    use std::time::Duration;
    use tokio::fs::{create_dir_all, write};
    use tokio::time::sleep;
    use zbus::fdo;
    use zbus::zvariant::{ObjectPath, OwnedObjectPath};

    pub(crate) async fn fake_model(model: SteamDeckVariant) -> Result<()> {
        create_dir_all(crate::path("/sys/class/dmi/id")).await?;
        match model {
            SteamDeckVariant::Unknown => write(crate::path(SYS_VENDOR_PATH), "LENOVO\n").await?,
            SteamDeckVariant::Jupiter => {
                write(crate::path(SYS_VENDOR_PATH), "Valve\n").await?;
                write(crate::path(BOARD_NAME_PATH), "Jupiter\n").await?;
                write(crate::path(PRODUCT_NAME_PATH), "Jupiter\n").await?;
            }
            SteamDeckVariant::Galileo => {
                write(crate::path(SYS_VENDOR_PATH), "Valve\n").await?;
                write(crate::path(BOARD_NAME_PATH), "Galileo\n").await?;
                write(crate::path(PRODUCT_NAME_PATH), "Galileo\n").await?;
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn board_lookup() {
        let _h = testing::start();

        create_dir_all(crate::path("/sys/class/dmi/id"))
            .await
            .expect("create_dir_all");
        assert!(steam_deck_variant().await.is_err());
        assert!(device_variant().await.is_err());

        write(crate::path(SYS_VENDOR_PATH), "LENOVO\n")
            .await
            .expect("write");
        write(crate::path(BOARD_NAME_PATH), "INVALID\n")
            .await
            .expect("write");
        write(crate::path(PRODUCT_NAME_PATH), "INVALID\n")
            .await
            .expect("write");
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (DeviceType::Unknown, String::from("unknown"))
        );

        write(crate::path(PRODUCT_NAME_PATH), "83L3\n")
            .await
            .expect("write");
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (DeviceType::LegionGoS, String::from("83L3"))
        );

        write(crate::path(PRODUCT_NAME_PATH), "83N6\n")
            .await
            .expect("write");
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (DeviceType::LegionGoS, String::from("83N6"))
        );

        write(crate::path(PRODUCT_NAME_PATH), "83Q2\n")
            .await
            .expect("write");
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (DeviceType::LegionGoS, String::from("83Q2"))
        );

        write(crate::path(PRODUCT_NAME_PATH), "83Q3\n")
            .await
            .expect("write");
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (DeviceType::LegionGoS, String::from("83Q3"))
        );

        write(crate::path(SYS_VENDOR_PATH), "Valve\n")
            .await
            .expect("write");
        write(crate::path(BOARD_NAME_PATH), "Jupiter\n")
            .await
            .expect("write");
        write(crate::path(PRODUCT_NAME_PATH), "Jupiter\n")
            .await
            .expect("write");
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Jupiter
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (DeviceType::SteamDeck, String::from("Jupiter"))
        );

        write(crate::path(BOARD_NAME_PATH), "Galileo\n")
            .await
            .expect("write");
        write(crate::path(PRODUCT_NAME_PATH), "Galileo\n")
            .await
            .expect("write");
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Galileo
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (DeviceType::SteamDeck, String::from("Galileo"))
        );

        write(crate::path(BOARD_NAME_PATH), "Neptune\n")
            .await
            .expect("write");
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (DeviceType::Unknown, String::from("unknown"))
        );
    }

    #[test]
    fn fan_control_state_roundtrip() {
        enum_roundtrip!(FanControlState {
            0: u32 = Bios,
            1: u32 = Os,
            "BIOS": str = Bios,
            "OS": str = Os,
        });
        assert_eq!(
            FanControlState::from_str("os").unwrap(),
            FanControlState::Os
        );
        assert_eq!(
            FanControlState::from_str("bios").unwrap(),
            FanControlState::Bios
        );
        assert!(FanControlState::try_from(2).is_err());
        assert!(FanControlState::from_str("on").is_err());
    }

    #[derive(Default)]
    struct MockUnit {
        active: bool,
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Unit")]
    impl MockUnit {
        #[zbus(property)]
        fn active_state(&self) -> fdo::Result<String> {
            if self.active {
                Ok(String::from("active"))
            } else {
                Ok(String::from("inactive"))
            }
        }

        async fn start(&mut self, mode: &str) -> fdo::Result<OwnedObjectPath> {
            if mode != "fail" {
                return Err(to_zbus_fdo_error("Invalid mode"));
            }
            self.active = true;
            let path = ObjectPath::try_from("/start/0").map_err(to_zbus_fdo_error)?;
            Ok(path.into())
        }

        async fn stop(&mut self, mode: &str) -> fdo::Result<OwnedObjectPath> {
            if mode != "fail" {
                return Err(to_zbus_fdo_error("Invalid mode"));
            }
            self.active = false;
            let path = ObjectPath::try_from("/stop/0").map_err(to_zbus_fdo_error)?;
            Ok(path.into())
        }
    }

    #[tokio::test]
    async fn test_fan_control() {
        let mut h = testing::start();
        let unit = MockUnit::default();
        let connection = h.new_dbus().await.expect("dbus");
        connection
            .request_name("org.freedesktop.systemd1")
            .await
            .expect("request_name");
        connection
            .object_server()
            .at(
                "/org/freedesktop/systemd1/unit/jupiter_2dfan_2dcontrol_2eservice",
                unit,
            )
            .await
            .expect("at");

        sleep(Duration::from_millis(10)).await;

        h.test.platform_config.replace(Some(PlatformConfig {
            factory_reset: None,
            update_bios: None,
            update_dock: None,
            storage: None,
            fan_control: Some(ServiceConfig::Systemd(String::from(
                "jupiter-fan-control.service",
            ))),
            tdp_limit: None,
            gpu_clocks: None,
            battery_charge_limit: None,
            performance_profile: None,
        }));

        let fan_control = FanControl::new(connection);
        assert_eq!(
            fan_control.get_state().await.unwrap(),
            FanControlState::Bios
        );
        assert!(fan_control.set_state(FanControlState::Os).await.is_ok());
        assert_eq!(fan_control.get_state().await.unwrap(), FanControlState::Os);
        assert!(fan_control.set_state(FanControlState::Bios).await.is_ok());
        assert_eq!(
            fan_control.get_state().await.unwrap(),
            FanControlState::Bios
        );
    }
}
