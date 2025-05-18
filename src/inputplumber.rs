/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::Result;
use tokio::spawn;
use tokio_stream::StreamExt;
use tracing::{debug, error, info};
use zbus::fdo::{InterfacesAdded, ObjectManagerProxy};
use zbus::names::OwnedInterfaceName;
use zbus::proxy::CacheProperties;
use zbus::zvariant::ObjectPath;
use zbus::Connection;

use crate::hardware::{device_type, DeviceType};
use crate::Service;

#[zbus::proxy(
    interface = "org.shadowblip.Input.CompositeDevice",
    default_service = "org.shadowblip.InputPlumber"
)]
trait CompositeDevice {
    #[zbus(property)]
    fn target_devices(&self) -> Result<Vec<String>>;

    async fn set_target_devices(&self, devices: &[&str]) -> Result<()>;
}

#[zbus::proxy(
    interface = "org.shadowblip.Input.Target",
    default_service = "org.shadowblip.InputPlumber"
)]
trait Target {
    #[zbus(property)]
    fn device_type(&self) -> Result<String>;
}

#[derive(Clone, Debug)]
pub struct DeckService {
    connection: Connection,
    composite_device_iface_name: OwnedInterfaceName,
}

impl DeckService {
    pub fn init(connection: Connection) -> DeckService {
        DeckService {
            connection,
            composite_device_iface_name: OwnedInterfaceName::try_from(
                "org.shadowblip.Input.CompositeDevice",
            )
            .unwrap(),
        }
    }

    async fn check_devices(&self, object_manager: &ObjectManagerProxy<'_>) -> Result<()> {
        for (path, ifaces) in object_manager.get_managed_objects().await?.into_iter() {
            if ifaces.contains_key(&self.composite_device_iface_name) {
                self.make_deck(&path).await?;
            }
        }
        Ok(())
    }

    async fn make_deck_from_ifaces_added(&self, msg: InterfacesAdded) -> Result<()> {
        let args = msg.args()?;
        if !args
            .interfaces_and_properties
            .contains_key(&self.composite_device_iface_name.as_ref())
        {
            return Ok(());
        }
        debug!("New CompositeDevice found at {}", args.object_path());
        self.make_deck(args.object_path()).await
    }

    async fn is_deck(&self, device: &CompositeDeviceProxy<'_>) -> Result<bool> {
        let targets = device.target_devices().await?;
        if targets.len() != 1 {
            return Ok(false);
        }

        let target = TargetProxy::builder(&self.connection)
            .path(targets[0].as_str())?
            .build()
            .await?;
        Ok(target.device_type().await? == "deck-uhid")
    }

    async fn make_deck(&self, path: &ObjectPath<'_>) -> Result<()> {
        if !path
            .as_str()
            .starts_with("/org/shadowblip/InputPlumber/CompositeDevice")
        {
            return Ok(());
        }
        let proxy = CompositeDeviceProxy::builder(&self.connection)
            .cache_properties(CacheProperties::No)
            .path(path)?
            .build()
            .await?;

        let system_device_type = match device_type().await {
            Ok(dt) => dt,
            Err(e) => {
                error!(
                    "Failed to get system device type for {}: {}. Skipping inputplumber device configuration.",
                    path, e
                );
                return Ok(());
            }
        };

        match system_device_type {
            DeviceType::SteamDeck => {
                if !self.is_deck(&proxy).await? {
                    debug!(
                        "Changing CompositeDevice {} into `deck-uhid` type for SteamDeck",
                        path
                    );
                    proxy.set_target_devices(&["deck-uhid"]).await?;
                } else {
                    debug!(
                        "CompositeDevice {} is already `deck-uhid` type for SteamDeck",
                        path
                    );
                }
            }
            // The MSI Claw lineup needs a keyboard target to be configured in order for the volume
            // keys to be registered while InputPlumber is running. Creating both a deck-uhid and a
            // keyboard target works for this.
            DeviceType::Claw => {
                const EXPECTED_CLAW_TARGET_TYPES: &[&str] = &["deck-uhid", "keyboard"];
                let current_target_types = proxy.target_devices().await?;

                let expected_claw_types_vec: Vec<String> = EXPECTED_CLAW_TARGET_TYPES
                    .iter()
                    .map(|&s| s.to_string())
                    .collect();

                let is_correctly_set = current_target_types == expected_claw_types_vec;

                if !is_correctly_set {
                    debug!(
                        "Changing CompositeDevice {} into `{:?}` for {:?}",
                        path, EXPECTED_CLAW_TARGET_TYPES, system_device_type
                    );
                    proxy.set_target_devices(EXPECTED_CLAW_TARGET_TYPES).await?;
                } else {
                    debug!(
                        "CompositeDevice {} is already `{:?}` for {:?}",
                        path, EXPECTED_CLAW_TARGET_TYPES, system_device_type
                    );
                }
            }
            _ => {
                debug!(
                    "CompositeDevice {} (type {:?}). No specific target configuration change.",
                    path, system_device_type
                );
            }
        }
        Ok(())
    }
}

impl Service for DeckService {
    const NAME: &'static str = "inputplumber";

    async fn run(&mut self) -> Result<()> {
        let object_manager = ObjectManagerProxy::new(
            &self.connection,
            "org.shadowblip.InputPlumber",
            "/org/shadowblip/InputPlumber",
        )
        .await?;
        let mut iface_added = object_manager.receive_interfaces_added().await?;

        // This needs to be done in a separate task to prevent the
        // signal listener from filling up. We just clone `self`
        // for this since it doesn't hold any state.
        let ctx = self.clone();
        spawn(async move {
            if let Err(e) = ctx.check_devices(&object_manager).await {
                info!("Can't query initial InputPlumber devices: {e}");
            }
        });

        loop {
            tokio::select! {
                Some(iface) = iface_added.next() => {
                    let ctx = self.clone();
                    spawn(async move {
                        ctx.make_deck_from_ifaces_added(iface).await
                    });
                }
            }
        }
    }
}
