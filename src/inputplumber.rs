/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::Result;
use tokio_stream::StreamExt;
use tracing::{debug, info};
use zbus::fdo::{InterfacesAdded, ObjectManagerProxy};
use zbus::names::OwnedInterfaceName;
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
        if device_type().await.unwrap_or(DeviceType::Unknown) == DeviceType::LegionGoS {
            // There is a bug on the Legion Go S where querying this information
            // messes up the mapping for the `deck` target device. It's not clear
            // exactly what's causing this, so we just skip on the Legion Go S
            // since it makes a `deck` target device by default.
            return Ok(());
        }
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
        Ok(target.device_type().await? == "deck")
    }

    async fn make_deck(&self, path: &ObjectPath<'_>) -> Result<()> {
        if !path
            .as_str()
            .starts_with("/org/shadowblip/InputPlumber/CompositeDevice")
        {
            return Ok(());
        }
        let proxy = CompositeDeviceProxy::builder(&self.connection)
            .path(path)?
            .build()
            .await?;
        if !self.is_deck(&proxy).await? {
            debug!("Changing CompositeDevice {} into `deck` type", path);
            proxy.set_target_devices(&["deck"]).await
        } else {
            debug!("CompositeDevice {} is already `deck` type", path);
            Ok(())
        }
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

        if let Err(e) = self.check_devices(&object_manager).await {
            info!("Can't query initial InputPlumber devices: {e}");
        }

        loop {
            tokio::select! {
                Some(iface) = iface_added.next() => self.make_deck_from_ifaces_added(iface).await?,
            }
        }
    }
}
