/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::mpsc::{unbounded_channel, Sender};
use tracing::error;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, Registry};
#[cfg(not(test))]
use xdg::BaseDirectories;
use zbus::connection::{Builder, Connection};

use crate::daemon::{channel, Daemon, DaemonCommand, DaemonContext};
use crate::job::{JobManager, JobManagerService};
use crate::manager::user::create_interfaces;
use crate::path;
use crate::power::TdpManagerService;
use crate::udev::UdevMonitor;

#[derive(Copy, Clone, Default, Deserialize, Debug)]
#[serde(default)]
pub(crate) struct UserConfig {
    pub services: UserServicesConfig,
}

#[derive(Copy, Clone, Default, Deserialize, Debug)]
pub(crate) struct UserServicesConfig {}

#[derive(Copy, Clone, Default, Deserialize, Serialize, Debug)]
#[serde(default)]
pub(crate) struct UserState {
    pub services: UserServicesState,
}

#[derive(Copy, Clone, Default, Deserialize, Serialize, Debug)]
pub(crate) struct UserServicesState {}

pub(crate) struct UserContext {
    session: Connection,
}

impl DaemonContext for UserContext {
    type State = UserState;
    type Config = UserConfig;
    type Command = ();

    #[cfg(not(test))]
    fn user_config_path(&self) -> Result<PathBuf> {
        let xdg_base = BaseDirectories::new()?;
        Ok(xdg_base.get_config_file("steamos-manager"))
    }

    #[cfg(test)]
    fn user_config_path(&self) -> Result<PathBuf> {
        Ok(path("steamos-manager"))
    }

    fn system_config_path(&self) -> Result<PathBuf> {
        Ok(path("/usr/share/steamos-manager/user.d"))
    }

    fn state(&self) -> UserState {
        UserState::default()
    }

    async fn start(
        &mut self,
        _state: UserState,
        _config: UserConfig,
        daemon: &mut Daemon<UserContext>,
    ) -> Result<()> {
        let udev = UdevMonitor::init(&self.session).await?;
        daemon.add_service(udev);

        Ok(())
    }

    async fn reload(
        &mut self,
        _config: UserConfig,
        _daemon: &mut Daemon<UserContext>,
    ) -> Result<()> {
        // Nothing to do yet
        Ok(())
    }

    async fn handle_command(
        &mut self,
        _cmd: Self::Command,
        _daemon: &mut Daemon<UserContext>,
    ) -> Result<()> {
        // Nothing to do yet
        Ok(())
    }
}

pub(crate) type Command = DaemonCommand<()>;

async fn create_connections(
    channel: Sender<Command>,
) -> Result<(Connection, Connection, JobManagerService, TdpManagerService)> {
    let system = Connection::system().await?;
    let connection = Builder::session()?
        .name("com.steampowered.SteamOSManager1")?
        .build()
        .await?;

    let (jm_tx, rx) = unbounded_channel();
    let job_manager = JobManager::new(connection.clone()).await?;
    let jm_service = JobManagerService::new(job_manager, rx, system.clone());

    let (tdp_tx, rx) = unbounded_channel();
    let tdp_service = TdpManagerService::new(rx, &system).await?;

    create_interfaces(connection.clone(), system.clone(), channel, jm_tx, tdp_tx).await?;

    Ok((connection, system, jm_service, tdp_service))
}

pub async fn daemon() -> Result<()> {
    // This daemon is responsible for creating a dbus api that steam client can use to do various OS
    // level things. It implements com.steampowered.SteamOSManager1.Manager interface

    let stdout_log = fmt::layer();
    let subscriber = Registry::default().with(stdout_log);
    let (tx, rx) = channel::<UserContext>();

    let (session, system, mirror_service, tdp_service) = match create_connections(tx).await {
        Ok(c) => c,
        Err(e) => {
            let _guard = tracing::subscriber::set_default(subscriber);
            error!("Error connecting to DBus: {}", e);
            bail!(e);
        }
    };

    let context = UserContext { session };
    let mut daemon = Daemon::new(subscriber, system, rx).await?;

    daemon.add_service(mirror_service);
    daemon.add_service(tdp_service);

    daemon.run(context).await
}
