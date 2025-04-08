/* SPDX-License-Identifier: BSD-2-Clause */
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use tokio::fs::{self, File};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::unix::pipe;
use tracing::{error, info};
use zbus::{zvariant, Connection};

use crate::{get_appid, path, read_comm, Service};

#[zbus::proxy(
    interface = "com.steampowered.SteamOSLogSubmitter.Trace",
    default_service = "com.steampowered.SteamOSLogSubmitter",
    default_path = "/com/steampowered/SteamOSLogSubmitter/helpers/Trace"
)]
trait TraceHelper {
    async fn log_event(
        &self,
        trace: &str,
        data: HashMap<&str, zvariant::Value<'_>>,
    ) -> zbus::Result<()>;
}

pub struct Ftrace
where
    Self: 'static,
{
    pipe: Option<BufReader<pipe::Receiver>>,
    proxy: TraceHelperProxy<'static>,
}

async fn setup_traces(base: &Path) -> Result<()> {
    fs::write(base.join("events/oom/mark_victim/enable"), "1").await?;

    let file = File::open(path("/proc/cpuinfo")).await?;
    let mut reader = BufReader::new(file);
    loop {
        let mut string = String::new();
        if reader.read_line(&mut string).await? == 0 {
            break;
        }
        if !string.starts_with("flags") {
            continue;
        }
        if let Some((_, rest)) = string.split_once(':') {
            let mut flags = rest.split_whitespace();
            if flags.any(|flag| flag == "split_lock_detect") {
                fs::write(base.join("set_ftrace_filter"), "split_lock_warn").await?;
                fs::write(base.join("current_tracer"), "function").await?;
                break;
            }
        }
    }
    Ok(())
}

impl Ftrace {
    pub async fn init(connection: &Connection) -> Result<Ftrace> {
        let path = Self::base();
        fs::create_dir_all(&path).await?;
        setup_traces(path.as_path()).await?;
        let file = pipe::OpenOptions::new()
            .unchecked(true) // Thanks tracefs for making trace_pipe a "regular" file
            .open_receiver(path.join("trace_pipe"))?;
        Ok(Ftrace {
            pipe: Some(BufReader::new(file)),
            proxy: TraceHelperProxy::new(connection).await?,
        })
    }

    fn base() -> PathBuf {
        path("/sys/kernel/tracing/instances/steamos-log-submitter")
    }

    async fn handle_pid(data: &mut HashMap<&str, zvariant::Value<'_>>, pid: u32) -> Result<()> {
        if let Ok(comm) = read_comm(pid) {
            info!("├─ comm: {}", comm);
            data.insert("comm", zvariant::Value::new(comm));
        } else {
            info!("├─ comm not found");
        }
        if let Ok(Some(appid)) = get_appid(pid) {
            info!("└─ appid: {}", appid);
            data.insert("appid", zvariant::Value::new(appid));
        } else {
            info!("└─ appid not found");
        }
        Ok(())
    }

    async fn handle_event(&mut self, line: &str) -> Result<()> {
        info!("Forwarding line {}", line);
        let mut data = HashMap::new();
        let mut split = line.rsplit(' ');
        if let Some(("pid", pid)) = split.next().and_then(|arg| arg.split_once('=')) {
            let pid = pid.parse()?;
            Ftrace::handle_pid(&mut data, pid).await?;
        }
        self.proxy.log_event(line, data).await?;
        Ok(())
    }
}

impl Service for Ftrace {
    const NAME: &'static str = "ftrace";

    async fn run(&mut self) -> Result<()> {
        loop {
            let mut string = String::new();
            self.pipe
                .as_mut()
                .ok_or(anyhow!("BUG: trace_pipe missing"))?
                .read_line(&mut string)
                .await?;
            if let Err(e) = self.handle_event(string.trim_end()).await {
                error!("Encountered an error handling event: {}", e);
            }
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.pipe.take();
        fs::remove_dir(Self::base()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::testing;
    use nix::sys::stat::Mode;
    use nix::unistd;
    use tokio::fs::{create_dir_all, read_to_string, try_exists, write};
    use tokio::sync::mpsc::{error, unbounded_channel, UnboundedSender};
    use zbus::fdo;

    struct MockTrace {
        traces: UnboundedSender<(String, HashMap<String, zvariant::OwnedValue>)>,
    }

    #[zbus::interface(name = "com.steampowered.SteamOSLogSubmitter.Trace")]
    impl MockTrace {
        fn log_event(
            &mut self,
            trace: &str,
            data: HashMap<&str, zvariant::Value<'_>>,
        ) -> fdo::Result<()> {
            let _ = self.traces.send((
                String::from(trace),
                HashMap::from_iter(
                    data.iter()
                        .map(|(k, v)| (String::from(*k), v.try_to_owned().unwrap())),
                ),
            ));
            Ok(())
        }
    }

    #[tokio::test]
    async fn handle_pid() {
        let _h = testing::start();

        create_dir_all(path("/proc/1234"))
            .await
            .expect("create_dir_all");
        write(path("/proc/1234/comm"), "ftrace\n")
            .await
            .expect("write comm");
        write(path("/proc/1234/environ"), "SteamGameId=5678")
            .await
            .expect("write environ");

        create_dir_all(path("/proc/1235"))
            .await
            .expect("create_dir_all");
        write(path("/proc/1235/comm"), "ftrace\n")
            .await
            .expect("write comm");

        create_dir_all(path("/proc/1236"))
            .await
            .expect("create_dir_all");
        write(path("/proc/1236/environ"), "SteamGameId=5678")
            .await
            .expect("write environ");

        let mut map = HashMap::new();
        assert!(Ftrace::handle_pid(&mut map, 1234).await.is_ok());
        assert_eq!(
            *map.get("comm").expect("comm"),
            zvariant::Value::new("ftrace")
        );
        assert_eq!(
            *map.get("appid").expect("appid"),
            zvariant::Value::new(5678_u64)
        );

        let mut map = HashMap::new();
        assert!(Ftrace::handle_pid(&mut map, 1235).await.is_ok());
        assert_eq!(
            *map.get("comm").expect("comm"),
            zvariant::Value::new("ftrace")
        );
        assert!(map.get("appid").is_none());

        let mut map = HashMap::new();
        assert!(Ftrace::handle_pid(&mut map, 1236).await.is_ok());
        assert!(map.get("comm").is_none());
        assert_eq!(
            *map.get("appid").expect("appid"),
            zvariant::Value::new(5678_u64)
        );
    }

    #[tokio::test]
    async fn ftrace_init() {
        let mut h = testing::start();

        let tracefs = Ftrace::base();

        create_dir_all(path("proc")).await.expect("create_dir_all");
        write(path("proc/cpuinfo"), &[]).await.expect("write");
        create_dir_all(tracefs.join("events/oom/mark_victim"))
            .await
            .expect("create_dir_all");
        unistd::mkfifo(
            tracefs.join("trace_pipe").as_path(),
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .expect("trace_pipe");
        let dbus = h.new_dbus().await.expect("dbus");
        let _ftrace = Ftrace::init(&dbus).await.expect("ftrace");

        assert_eq!(
            read_to_string(tracefs.join("events/oom/mark_victim/enable"))
                .await
                .unwrap(),
            "1"
        );
        assert!(!try_exists(tracefs.join("set_ftrace_filter")).await.unwrap());
        assert!(!try_exists(tracefs.join("current_tracer")).await.unwrap());
    }

    #[tokio::test]
    async fn ftrace_init_split_lock() {
        let mut h = testing::start();

        let tracefs = Ftrace::base();

        create_dir_all(path("proc")).await.expect("create_dir_all");
        write(path("proc/cpuinfo"), "flags           : split_lock_detect")
            .await
            .expect("write");
        create_dir_all(tracefs.join("events/oom/mark_victim"))
            .await
            .expect("create_dir_all");
        unistd::mkfifo(
            tracefs.join("trace_pipe").as_path(),
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .expect("trace_pipe");
        let dbus = h.new_dbus().await.expect("dbus");
        let _ftrace = Ftrace::init(&dbus).await.expect("ftrace");

        assert_eq!(
            read_to_string(tracefs.join("events/oom/mark_victim/enable"))
                .await
                .unwrap(),
            "1"
        );
        assert_eq!(
            read_to_string(tracefs.join("set_ftrace_filter"))
                .await
                .unwrap(),
            "split_lock_warn"
        );
        assert_eq!(
            read_to_string(tracefs.join("current_tracer"))
                .await
                .unwrap(),
            "function"
        );
    }

    #[tokio::test]
    async fn ftrace_relay() {
        let _h = testing::start();

        let tracefs = Ftrace::base();

        create_dir_all(tracefs.join("events/oom/mark_victim"))
            .await
            .expect("create_dir_all");
        unistd::mkfifo(
            tracefs.join("trace_pipe").as_path(),
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .expect("trace_pipe");

        create_dir_all(path("proc")).await.expect("create_dir_all");
        write(path("proc/cpuinfo"), &[])
            .await
            .expect("write cpuinfo");
        create_dir_all(path("/proc/14351"))
            .await
            .expect("create_dir_all");
        write(path("/proc/14351/comm"), "ftrace\n")
            .await
            .expect("write comm");
        write(path("/proc/14351/environ"), "SteamGameId=5678")
            .await
            .expect("write environ");

        let (sender, mut receiver) = unbounded_channel();
        let trace = MockTrace { traces: sender };
        let dbus = zbus::connection::Builder::session()
            .unwrap()
            .name("com.steampowered.SteamOSLogSubmitter")
            .unwrap()
            .serve_at("/com/steampowered/SteamOSLogSubmitter/helpers/Trace", trace)
            .unwrap()
            .build()
            .await
            .expect("dbus");
        let mut ftrace = Ftrace::init(&dbus).await.expect("ftrace");

        assert!(match receiver.try_recv() {
            Err(error::TryRecvError::Empty) => true,
            _ => false,
        });
        ftrace
            .handle_event(
                " GamepadUI Input-4886    [003] .N.1. 23828.572941: mark_victim: pid=14351",
            )
            .await
            .expect("event");
        let (line, data) = match receiver.try_recv() {
            Ok((line, data)) => (line, data),
            _ => panic!("Test failed"),
        };
        assert_eq!(
            line,
            " GamepadUI Input-4886    [003] .N.1. 23828.572941: mark_victim: pid=14351"
        );
        assert_eq!(data.len(), 2);
        assert_eq!(
            data.get("appid").map(|v| v.downcast_ref()),
            Some(Ok(5678_u64))
        );
        assert_eq!(
            data.get("comm").map(|v| v.downcast_ref()),
            Some(Ok("ftrace"))
        );

        ftrace
            .handle_event(" GamepadUI Input-4886    [003] .N.1. 23828.572941: split_lock_warn <-")
            .await
            .expect("event");
        let (line, data) = match receiver.try_recv() {
            Ok((line, data)) => (line, data),
            _ => panic!("Test failed"),
        };
        assert_eq!(
            line,
            " GamepadUI Input-4886    [003] .N.1. 23828.572941: split_lock_warn <-"
        );
        assert_eq!(data.len(), 0);
    }
}
