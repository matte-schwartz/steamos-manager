/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::str::FromStr;
use strum::{Display, EnumString};
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1"
)]
trait SystemdUnit {
    #[zbus(property)]
    fn active_state(&self) -> Result<String>;
    #[zbus(property)]
    fn unit_file_state(&self) -> Result<String>;

    async fn restart(&self, mode: &str) -> Result<OwnedObjectPath>;
    async fn start(&self, mode: &str) -> Result<OwnedObjectPath>;
    async fn stop(&self, mode: &str) -> Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait SystemdManager {
    #[allow(clippy::type_complexity)]
    async fn enable_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
        force: bool,
    ) -> Result<(bool, Vec<(String, String, String)>)>;

    async fn disable_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
    ) -> Result<Vec<(String, String, String)>>;

    async fn mask_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
        force: bool,
    ) -> Result<Vec<(String, String, String)>>;

    async fn unmask_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
    ) -> Result<Vec<(String, String, String)>>;

    async fn reload(&self) -> Result<()>;
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone)]
#[strum(serialize_all = "lowercase")]
pub enum EnableState {
    Disabled,
    Enabled,
    Masked,
    Static,
}

pub struct SystemdUnit<'dbus> {
    connection: Connection,
    proxy: SystemdUnitProxy<'dbus>,
    name: String,
}

pub async fn daemon_reload(connection: &Connection) -> Result<()> {
    let proxy = SystemdManagerProxy::new(connection).await?;
    proxy.reload().await?;
    Ok(())
}

impl<'dbus> SystemdUnit<'dbus> {
    pub async fn new(connection: Connection, name: &str) -> Result<SystemdUnit<'dbus>> {
        let path = PathBuf::from("/org/freedesktop/systemd1/unit").join(escape(name));
        let path = String::from(path.to_str().ok_or(anyhow!("Unit name {name} invalid"))?);
        Ok(SystemdUnit {
            proxy: SystemdUnitProxy::builder(&connection)
                .cache_properties(CacheProperties::No)
                .path(path)?
                .build()
                .await?,
            connection,
            name: String::from(name),
        })
    }

    pub async fn restart(&self) -> Result<()> {
        self.proxy.restart("fail").await?;
        Ok(())
    }

    pub async fn start(&self) -> Result<()> {
        self.proxy.start("fail").await?;
        Ok(())
    }

    pub async fn stop(&self) -> Result<()> {
        self.proxy.stop("fail").await?;
        Ok(())
    }

    #[allow(unused)]
    pub async fn enable(&self) -> Result<bool> {
        let manager = SystemdManagerProxy::new(&self.connection).await?;
        let (_, res) = manager
            .enable_unit_files(&[self.name.as_str()], false, false)
            .await?;
        Ok(!res.is_empty())
    }

    #[allow(unused)]
    pub async fn disable(&self) -> Result<bool> {
        let manager = SystemdManagerProxy::new(&self.connection).await?;
        let res = manager
            .disable_unit_files(&[self.name.as_str()], false)
            .await?;
        Ok(!res.is_empty())
    }

    pub async fn mask(&self) -> Result<bool> {
        let manager = SystemdManagerProxy::new(&self.connection).await?;
        let res = manager
            .mask_unit_files(&[self.name.as_str()], false, false)
            .await?;
        Ok(!res.is_empty())
    }

    pub async fn unmask(&self) -> Result<bool> {
        let manager = SystemdManagerProxy::new(&self.connection).await?;
        let res = manager
            .unmask_unit_files(&[self.name.as_str()], false)
            .await?;
        Ok(!res.is_empty())
    }

    pub async fn active(&self) -> Result<bool> {
        Ok(self.proxy.active_state().await? == "active")
    }

    pub async fn enabled(&self) -> Result<EnableState> {
        Ok(EnableState::from_str(
            self.proxy.unit_file_state().await?.as_str(),
        )?)
    }
}

pub fn escape(name: &str) -> String {
    let mut parts = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            parts.push(c);
        } else {
            let escaped = format!("_{:02x}", u32::from(c));
            parts.push_str(escaped.as_str());
        }
    }
    parts
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::error::to_zbus_fdo_error;
    use crate::{enum_roundtrip, testing};
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::time::sleep;
    use zbus::{fdo, ObjectServer};
    use zbus::zvariant::ObjectPath;

    #[test]
    fn enable_state_roundtrip() {
        enum_roundtrip!(EnableState {
            "disabled": str = Disabled,
            "enabled": str = Enabled,
            "masked": str = Masked,
            "static": str = Static,
        });
        assert!(EnableState::from_str("loaded").is_err());
    }

    #[test]
    fn test_escape() {
        assert_eq!(escape("systemd"), "systemd");
        assert_eq!(escape("system d"), "system_20d");
    }

    #[derive(Default)]
    pub struct MockUnit {
        pub active: String,
        pub unit_file: String,
        job: u32,
    }

    #[derive(Default)]
    pub struct MockManager {
        states: HashMap<String, EnableState>,
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Unit")]
    impl MockUnit {
        #[zbus(property)]
        fn active_state(&self) -> fdo::Result<String> {
            Ok(self.active.clone())
        }

        #[zbus(property)]
        fn unit_file_state(&self) -> fdo::Result<String> {
            Ok(self.unit_file.clone())
        }

        async fn restart(&mut self, mode: &str) -> fdo::Result<OwnedObjectPath> {
            if mode != "fail" {
                return Err(to_zbus_fdo_error("Invalid mode"));
            }
            let path = ObjectPath::try_from(format!("/restart/{mode}/{}", self.job))
                .map_err(to_zbus_fdo_error)?;
            self.job += 1;
            self.active = String::from("active");
            Ok(path.into())
        }

        async fn start(&mut self, mode: &str) -> fdo::Result<OwnedObjectPath> {
            if mode != "fail" {
                return Err(to_zbus_fdo_error("Invalid mode"));
            }
            let path = ObjectPath::try_from(format!("/start/{mode}/{}", self.job))
                .map_err(to_zbus_fdo_error)?;
            self.job += 1;
            self.active = String::from("active");
            Ok(path.into())
        }

        async fn stop(&mut self, mode: &str) -> fdo::Result<OwnedObjectPath> {
            if mode != "fail" {
                return Err(to_zbus_fdo_error("Invalid mode"));
            }
            let path = ObjectPath::try_from(format!("/stop/{mode}/{}", self.job))
                .map_err(to_zbus_fdo_error)?;
            self.job += 1;
            self.active = String::from("inactive");
            Ok(path.into())
        }
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
    impl MockManager {
        #[allow(clippy::type_complexity)]
        async fn enable_unit_files(
            &mut self,
            files: Vec<String>,
            _runtime: bool,
            _force: bool,
            #[zbus(object_server)] object_server: &ObjectServer,
        ) -> fdo::Result<(bool, Vec<(String, String, String)>)> {
            let mut res = Vec::new();
            for file in files {
                if let Some(state) = self.states.get(&file) {
                    if *state == EnableState::Disabled {
                        self.states.insert(file.to_string(), EnableState::Enabled);
                        res.push((String::default(), String::default(), file.to_string()));
                    }
                } else {
                    self.states.insert(file.to_string(), EnableState::Enabled);
                    res.push((String::default(), String::default(), file.to_string()));
                }
                let path = PathBuf::from("/org/freedesktop/systemd1/unit").join(escape(&file));
                let mock_unit = object_server
                    .interface::<_, MockUnit>(path.to_string_lossy())
                    .await;
                if let Ok(mock_unit) = mock_unit {
                    dbg!();
                    mock_unit.get_mut().await.unit_file = String::from("enabled");
                }
            }
            Ok((true, res))
        }

        async fn disable_unit_files(
            &mut self,
            files: Vec<String>,
            _runtime: bool,
            #[zbus(object_server)] object_server: &ObjectServer,
        ) -> fdo::Result<Vec<(String, String, String)>> {
            let mut res = Vec::new();
            for file in files {
                if let Some(state) = self.states.get(&file) {
                    if *state == EnableState::Enabled {
                        self.states.insert(file.to_string(), EnableState::Disabled);
                        res.push((String::default(), String::default(), file.to_string()));
                    }
                } else {
                    self.states.insert(file.to_string(), EnableState::Disabled);
                    res.push((String::default(), String::default(), file.to_string()));
                }
                let path = PathBuf::from("/org/freedesktop/systemd1/unit").join(escape(&file));
                let mock_unit = object_server
                    .interface::<_, MockUnit>(path.to_string_lossy())
                    .await;
                if let Ok(mock_unit) = mock_unit {
                    mock_unit.get_mut().await.unit_file = String::from("disabled");
                }
            }
            Ok(res)
        }

        async fn mask_unit_files(
            &mut self,
            files: Vec<String>,
            _runtime: bool,
            _force: bool,
        ) -> fdo::Result<Vec<(String, String, String)>> {
            let mut res = Vec::new();
            for file in files {
                if let Some(state) = self.states.get(&file) {
                    if *state != EnableState::Masked {
                        self.states.insert(file.to_string(), EnableState::Masked);
                        res.push((String::default(), String::default(), file.to_string()));
                    }
                } else {
                    self.states.insert(file.to_string(), EnableState::Masked);
                    res.push((String::default(), String::default(), file.to_string()));
                }
            }
            Ok(res)
        }

        async fn unmask_unit_files(
            &mut self,
            files: Vec<String>,
            _runtime: bool,
        ) -> fdo::Result<Vec<(String, String, String)>> {
            let mut res = Vec::new();
            for file in files {
                if let Some(state) = self.states.get(&file) {
                    if *state == EnableState::Masked {
                        self.states.remove(&file);
                        res.push((String::default(), String::default(), file.to_string()));
                    }
                }
            }
            Ok(res)
        }

        async fn reload(&self) -> fdo::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_unit() {
        let mut h = testing::start();
        let mut unit = MockUnit::default();
        unit.active = String::from("inactive");
        unit.unit_file = String::from("disabled");
        let connection = h.new_dbus().await.expect("dbus");
        connection
            .request_name("org.freedesktop.systemd1")
            .await
            .expect("request_name");
        let object_server = connection.object_server();
        object_server
            .at("/org/freedesktop/systemd1/unit/test_2eservice", unit)
            .await
            .expect("at");
        object_server
            .at("/org/freedesktop/systemd1", MockManager::default())
            .await
            .expect("at");
        let mock_unit = object_server
            .interface::<_, MockUnit>("/org/freedesktop/systemd1/unit/test_2eservice")
            .await
            .unwrap();

        sleep(Duration::from_millis(10)).await;

        let unit = SystemdUnit::new(connection.clone(), "test.service")
            .await
            .expect("unit");
        assert_eq!(unit.active().await.unwrap(), false);

        assert!(unit.start().await.is_ok());
        assert_eq!(mock_unit.get().await.active, "active");
        assert_eq!(unit.active().await.unwrap(), true);

        assert!(unit.restart().await.is_ok());
        assert_eq!(mock_unit.get().await.active, "active");
        assert_eq!(unit.active().await.unwrap(), true);

        assert!(unit.stop().await.is_ok());
        assert_eq!(mock_unit.get().await.active, "inactive");
        assert_eq!(unit.active().await.unwrap(), false);

        assert_eq!(mock_unit.get().await.unit_file, "disabled");
        assert_eq!(unit.enabled().await.unwrap(), EnableState::Disabled);

        assert!(unit.enable().await.unwrap());
        assert_eq!(mock_unit.get().await.unit_file, "enabled");
        assert_eq!(unit.enabled().await.unwrap(), EnableState::Enabled);

        assert!(unit.disable().await.unwrap());
        assert_eq!(mock_unit.get().await.unit_file, "disabled");
        assert_eq!(unit.enabled().await.unwrap(), EnableState::Disabled);
    }

    #[tokio::test]
    async fn test_manager() {
        let mut h = testing::start();
        let mut unit = MockUnit::default();
        unit.active = String::from("active");
        unit.unit_file = String::from("enabled");
        let connection = h.new_dbus().await.expect("dbus");
        connection
            .request_name("org.freedesktop.systemd1")
            .await
            .expect("request_name");
        let object_server = connection.object_server();
        object_server
            .at("/org/freedesktop/systemd1/unit/test_2eservice", unit)
            .await
            .expect("at");
        object_server
            .at("/org/freedesktop/systemd1", MockManager::default())
            .await
            .expect("at");

        sleep(Duration::from_millis(10)).await;

        let unit = SystemdUnit::new(connection.clone(), "test.service")
            .await
            .expect("unit");
        assert!(unit.enable().await.unwrap());
        assert!(!unit.enable().await.unwrap());
        assert!(unit.disable().await.unwrap());
        assert!(!unit.disable().await.unwrap());
        assert!(unit.mask().await.unwrap());
        assert!(!unit.mask().await.unwrap());
        assert!(unit.unmask().await.unwrap());
        assert!(!unit.unmask().await.unwrap());
    }
}
