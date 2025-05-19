/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{bail, Result};
use async_trait::async_trait;
use config::builder::AsyncState;
use config::{AsyncSource, ConfigBuilder, ConfigError, FileFormat, Format, Map, Value};
use std::fmt::Debug;
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use tokio::fs::{read_dir, read_to_string, File};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

mod ds_inhibit;
mod error;
mod inputplumber;
mod job;
mod manager;
mod platform;
mod process;
mod screenreader;
mod sls;
mod systemd;
mod udev;

pub mod cec;
pub mod daemon;
pub mod hardware;
pub mod power;
pub mod proxy;
pub mod wifi;

#[cfg(test)]
mod testing;

const API_VERSION: u32 = 9;

pub trait Service
where
    Self: Sized + Send,
{
    const NAME: &'static str;

    fn run(&mut self) -> impl Future<Output = Result<()>> + Send;

    fn shutdown(&mut self) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    fn start(mut self, token: CancellationToken) -> impl Future<Output = Result<()>> + Send {
        async move {
            info!("Starting {}", Self::NAME);
            let res = tokio::select! {
                r = self.run() => r,
                () = token.cancelled() => Ok(()),
            };
            if res.is_err() {
                warn!(
                    "{} encountered an error: {}",
                    Self::NAME,
                    res.as_ref().unwrap_err()
                );
                token.cancel();
            }
            info!("Shutting down {}", Self::NAME);
            self.shutdown().await.and(res)
        }
    }
}

#[derive(Debug)]
struct AsyncFileSource<F: Format, P: AsRef<Path> + Sized + Send + Sync> {
    path: P,
    format: F,
}

impl<F: Format, P: AsRef<Path> + Sized + Send + Sync + Debug> AsyncFileSource<F, P> {
    fn from(path: P, format: F) -> AsyncFileSource<F, P> {
        AsyncFileSource { path, format }
    }
}

#[async_trait]
impl<F: Format + Send + Sync + Debug, P: AsRef<Path> + Sized + Send + Sync + Debug> AsyncSource
    for AsyncFileSource<F, P>
{
    async fn collect(&self) -> Result<Map<String, Value>, ConfigError> {
        let path = self.path.as_ref();
        let text = match read_to_string(&path).await {
            Ok(text) => text,
            Err(e) => {
                if e.kind() == ErrorKind::NotFound {
                    info!("No config file {} found", path.to_string_lossy());
                    return Ok(Map::new());
                }
                return Err(ConfigError::Foreign(Box::new(e)));
            }
        };
        let path = path.to_string_lossy().to_string();
        self.format
            .parse(Some(&path), &text)
            .map_err(ConfigError::Foreign)
    }
}

#[cfg(not(test))]
pub fn path<S: AsRef<str>>(path: S) -> PathBuf {
    PathBuf::from(path.as_ref())
}

#[cfg(test)]
pub fn path<S: AsRef<str>>(path: S) -> PathBuf {
    let current_test = crate::testing::current();
    let test_path = current_test.path();
    PathBuf::from(test_path.as_os_str().to_str().unwrap())
        .join(path.as_ref().trim_start_matches('/'))
}

pub(crate) async fn write_synced<P: AsRef<Path>>(path: P, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path.as_ref()).await?;
    file.write_all(bytes).await?;
    Ok(file.sync_data().await?)
}

pub(crate) fn read_comm(pid: u32) -> Result<String> {
    let comm = std::fs::read_to_string(path(format!("/proc/{pid}/comm")))?;
    Ok(comm.trim_end().to_string())
}

pub(crate) fn get_appid(pid: u32) -> Result<Option<u64>> {
    let environ = std::fs::read_to_string(path(format!("/proc/{pid}/environ")))?;
    for env_var in environ.split('\0') {
        let Some((key, value)) = env_var.split_once('=') else {
            continue;
        };
        if key != "SteamGameId" {
            continue;
        }
        if let Ok(appid) = value.parse() {
            return Ok(Some(appid));
        }
        break;
    }

    let stat = std::fs::read_to_string(path(format!("/proc/{pid}/stat")))?;
    let ppid: u32 = if let Some((_, stat)) = stat.rsplit_once(") ") {
        if let Some(ppid) = stat.split(' ').nth(1) {
            ppid.parse()?
        } else {
            bail!("stat data invalid");
        }
    } else {
        return Ok(None);
    };
    if ppid > 1 {
        get_appid(ppid)
    } else {
        Ok(None)
    }
}

pub(crate) async fn read_config_directory<P: AsRef<Path> + Sync + Send>(
    builder: ConfigBuilder<AsyncState>,
    path: P,
    extensions: &[&str],
    format: FileFormat,
) -> Result<ConfigBuilder<AsyncState>> {
    let mut dir = match read_dir(&path).await {
        Ok(dir) => dir,
        Err(e) => {
            if e.kind() == ErrorKind::NotFound {
                debug!(
                    "No config fragment directory {} found",
                    path.as_ref().to_string_lossy()
                );
                return Ok(builder);
            }
            error!(
                "Error reading config fragment directory {}: {e}",
                path.as_ref().to_string_lossy()
            );
            return Err(e.into());
        }
    };
    let mut entries = Vec::new();
    while let Some(entry) = dir.next_entry().await? {
        let path = entry.path();
        if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
            if extensions.contains(&ext) {
                entries.push(path);
            }
        }
    }
    entries.sort();
    Ok(entries.into_iter().fold(builder, |builder, path| {
        builder.add_async_source(AsyncFileSource::from(path, format))
    }))
}

#[cfg(test)]
mod test {
    use crate::testing;
    use std::fs;

    #[test]
    fn read_comm() {
        let h = testing::start();
        let path = h.test.path();
        fs::create_dir_all(path.join("proc/123456")).expect("create_dir_all");
        fs::write(path.join("proc/123456/comm"), "test\n").expect("write comm");

        assert_eq!(crate::read_comm(123456).expect("read_comm"), "test");
        assert!(crate::read_comm(123457).is_err());
    }

    #[test]
    fn appid_environ() {
        let h = testing::start();
        let path = h.test.path();
        fs::create_dir_all(path.join("proc/123456")).expect("create_dir_all");
        fs::write(
            path.join("proc/123456/environ"),
            "A=B\0SteamGameId=98765\0C=D",
        )
        .expect("write environ");

        assert_eq!(crate::get_appid(123456).expect("get_appid"), Some(98765));
        assert!(crate::get_appid(123457).is_err());
    }

    #[test]
    fn appid_parent_environ() {
        let h = testing::start();
        let path = h.test.path();
        fs::create_dir_all(path.join("proc/123456")).expect("create_dir_all");
        fs::write(
            path.join("proc/123456/environ"),
            "A=B\0SteamGameId=98765\0C=D",
        )
        .expect("write environ");
        fs::create_dir_all(path.join("proc/123457")).expect("create_dir_all");
        fs::write(path.join("proc/123457/environ"), "A=B\0C=D").expect("write environ");
        fs::write(path.join("proc/123457/stat"), "0 (comm) S 123456 ...").expect("write stat");

        assert_eq!(crate::get_appid(123457).expect("get_appid"), Some(98765));
    }

    #[test]
    fn appid_missing() {
        let h = testing::start();
        let path = h.test.path();
        fs::create_dir_all(path.join("proc/123457")).expect("create_dir_all");
        fs::write(path.join("proc/123457/environ"), "A=B\0C=D").expect("write environ");
        fs::write(path.join("proc/123457/stat"), "0 (comm) S 1 ...").expect("write stat");

        assert_eq!(crate::get_appid(123457).expect("get_appid"), None);
    }
}
