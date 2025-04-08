/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, Registry};
use zbus::connection::{Builder, Connection};

use crate::daemon::{channel, Daemon, DaemonCommand, DaemonContext};
use crate::ds_inhibit::Inhibitor;
use crate::manager::root::SteamOSManager;
use crate::path;
use crate::sls::ftrace::Ftrace;

#[derive(Copy, Clone, Default, Deserialize, Debug)]
#[serde(default)]
pub(crate) struct RootConfig {
    pub services: RootServicesConfig,
}

#[derive(Copy, Clone, Default, Deserialize, Debug)]
pub(crate) struct RootServicesConfig {}

#[derive(Copy, Clone, Default, Deserialize, Serialize, Debug)]
#[serde(default)]
pub(crate) struct RootState {
    pub services: RootServicesState,
}

#[derive(Copy, Clone, Default, Deserialize, Serialize, Debug)]
pub(crate) struct RootServicesState {
    pub ds_inhibit: DsInhibit,
}

#[derive(Debug)]
pub(crate) enum RootCommand {
    SetDsInhibit(bool),
    GetDsInhibit(oneshot::Sender<bool>),
}

#[derive(Copy, Clone, Deserialize, Serialize, Debug)]
pub(crate) struct DsInhibit {
    pub enabled: bool,
}

impl Default for DsInhibit {
    fn default() -> DsInhibit {
        DsInhibit { enabled: true }
    }
}

pub(crate) struct RootContext {
    state: RootState,
    channel: Sender<Command>,

    ds_inhibit: Option<CancellationToken>,
}

impl RootContext {
    pub(crate) fn new(channel: Sender<Command>) -> RootContext {
        RootContext {
            state: RootState::default(),
            channel,
            ds_inhibit: None,
        }
    }

    async fn reload_ds_inhibit(&mut self, daemon: &mut Daemon<RootContext>) -> Result<()> {
        match (
            self.state.services.ds_inhibit.enabled,
            self.ds_inhibit.as_ref(),
        ) {
            (false, Some(handle)) => {
                handle.cancel();
                self.ds_inhibit = None;
            }
            (true, None) => {
                let inhibitor = Inhibitor::init().await?;
                self.ds_inhibit = Some(daemon.add_service(inhibitor));
            }
            _ => (),
        }
        Ok(())
    }
}

impl DaemonContext for RootContext {
    type State = RootState;
    type Config = RootConfig;
    type Command = RootCommand;

    fn user_config_path(&self) -> Result<PathBuf> {
        Ok(path("/etc/steamos-manager"))
    }

    fn system_config_path(&self) -> Result<PathBuf> {
        Ok(path("/usr/share/steamos-manager/system.d"))
    }

    fn state(&self) -> RootState {
        self.state
    }

    async fn start(
        &mut self,
        state: RootState,
        _config: RootConfig,
        daemon: &mut Daemon<RootContext>,
    ) -> Result<()> {
        self.state = state;

        let connection = daemon.get_connection();
        let ftrace = Ftrace::init(&connection).await?;
        daemon.add_service(ftrace);

        self.reload_ds_inhibit(daemon).await?;

        Ok(())
    }

    async fn reload(
        &mut self,
        _config: RootConfig,
        _daemon: &mut Daemon<RootContext>,
    ) -> Result<()> {
        // Nothing to do yet
        Ok(())
    }

    async fn handle_command(
        &mut self,
        cmd: RootCommand,
        daemon: &mut Daemon<RootContext>,
    ) -> Result<()> {
        match cmd {
            RootCommand::SetDsInhibit(enable) => {
                self.state.services.ds_inhibit.enabled = enable;
                self.reload_ds_inhibit(daemon).await?;
                self.channel.send(DaemonCommand::WriteState).await?;
            }
            RootCommand::GetDsInhibit(sender) => {
                let _ = sender.send(self.ds_inhibit.is_some());
            }
        }
        Ok(())
    }
}

pub(crate) type Command = DaemonCommand<RootCommand>;

async fn create_connection(channel: Sender<Command>) -> Result<Connection> {
    let connection = Builder::system()?
        .name("com.steampowered.SteamOSManager1")?
        .build()
        .await?;
    let manager = SteamOSManager::new(connection.clone(), channel).await?;
    connection
        .object_server()
        .at("/com/steampowered/SteamOSManager1", manager)
        .await?;
    Ok(connection)
}

pub async fn daemon() -> Result<()> {
    // This daemon is responsible for creating a dbus api that steam client can use to do various OS
    // level things. It implements com.steampowered.SteamOSManager1.RootManager interface

    let stdout_log = fmt::layer();
    let subscriber = Registry::default().with(stdout_log);
    let (tx, rx) = channel::<RootContext>();

    let connection = match create_connection(tx.clone()).await {
        Ok(c) => c,
        Err(e) => {
            let _guard = tracing::subscriber::set_default(subscriber);
            error!("Error connecting to DBus: {}", e);
            bail!(e);
        }
    };

    let context = RootContext::new(tx);
    let mut daemon = Daemon::new(subscriber, connection.clone(), rx).await?;

    daemon.run(context).await
}
