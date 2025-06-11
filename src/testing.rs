use anyhow::{anyhow, bail, Result};
use libc::pid_t;
use nix::sys::signal;
use nix::unistd::Pid;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::iter::zip;
use std::path::Path;
use std::process::Stdio;
use std::rc::Rc;
use std::str::FromStr;
use std::time::Duration;
use tempfile::{tempdir, TempDir};
use tokio::fs::read;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::error;
use zbus::connection::{Builder, Connection};
use zbus::object_server::Interface;
use zbus::zvariant::ObjectPath;
use zbus::Address;
use zbus_xml::{Method, Node, Property, Signal};

use crate::platform::{DeviceConfig, PlatformConfig};

thread_local! {
    static TEST: RefCell<Option<Rc<Test>>> = const { RefCell::new(None) };
}

#[macro_export]
macro_rules! enum_roundtrip {
    ($enum:ident => $value:literal : str = $variant:ident) => {
        assert_eq!($enum::$variant.to_string(), $value);
        assert_eq!($enum::from_str($value).unwrap(), $enum::$variant);
    };
    ($enum:ident => $value:literal : $ty:ty = $variant:ident) => {
        assert_eq!($enum::$variant as $ty, $value);
        assert_eq!($enum::try_from($value).unwrap(), $enum::$variant);
    };

    ($enum:ident { $($value:literal : $ty:ident = $variant:ident,)+ }) => {
        $(enum_roundtrip!($enum => $value : $ty = $variant);)+
    };
}

#[macro_export]
macro_rules! enum_on_off {
    ($enum:ident => ($on:ident, $off:ident)) => {
        assert_eq!($enum::from_str("on").unwrap(), $enum::$on);
        assert_eq!($enum::from_str("On").unwrap(), $enum::$on);
        assert_eq!($enum::from_str("enable").unwrap(), $enum::$on);
        assert_eq!($enum::from_str("enabled").unwrap(), $enum::$on);
        assert_eq!($enum::from_str("1").unwrap(), $enum::$on);
        assert_eq!($enum::from_str("off").unwrap(), $enum::$off);
        assert_eq!($enum::from_str("Off").unwrap(), $enum::$off);
        assert_eq!($enum::from_str("disable").unwrap(), $enum::$off);
        assert_eq!($enum::from_str("disabled").unwrap(), $enum::$off);
        assert_eq!($enum::from_str("0").unwrap(), $enum::$off);
    };
}

pub fn start() -> TestHandle {
    TEST.with(|lock| {
        assert!(lock.borrow().as_ref().is_none());
        let test: Rc<Test> = Rc::new(Test {
            base: tempdir().expect("Couldn't create test directory"),
            process_cb: Cell::new(|_, _| Err(anyhow!("No current process_cb"))),
            mock_dbus: Cell::new(None),
            dbus_address: Mutex::new(None),
            platform_config: RefCell::new(None),
            device_config: RefCell::new(None),
        });
        *lock.borrow_mut() = Some(test.clone());
        TestHandle { test }
    })
}

pub fn stop() {
    TEST.with(|lock| {
        let test = (*lock.borrow_mut()).take();
        if let Some(test) = test {
            if let Some(mock_dbus) = test.mock_dbus.take() {
                let _ = mock_dbus.shutdown();
            }
        }
    });
}

pub fn current() -> Rc<Test> {
    TEST.with(|lock| lock.borrow().as_ref().unwrap().clone())
}

pub struct MockDBus {
    pub connection: Connection,
    address: Address,
    process: Child,
}

pub struct Test {
    base: TempDir,
    pub process_cb: Cell<fn(&OsStr, &[&OsStr]) -> Result<(i32, String)>>,
    pub mock_dbus: Cell<Option<MockDBus>>,
    pub dbus_address: Mutex<Option<Address>>,
    pub platform_config: RefCell<Option<PlatformConfig>>,
    pub device_config: RefCell<Option<DeviceConfig>>,
}

pub struct TestHandle {
    pub test: Rc<Test>,
}

impl MockDBus {
    pub async fn new() -> Result<MockDBus> {
        let mut process = Command::new("/usr/bin/dbus-daemon")
            .args([
                "--nofork",
                "--print-address",
                "--config-file=data/test-dbus.conf",
            ])
            .stdout(Stdio::piped())
            .spawn()?;

        let stdout = BufReader::new(
            process
                .stdout
                .take()
                .ok_or(anyhow!("Couldn't capture stdout"))?,
        );

        let address = stdout
            .lines()
            .next_line()
            .await?
            .ok_or(anyhow!("Failed to read address"))?;

        let address = Address::from_str(address.trim_end())?;
        let connection = Builder::address(address.clone())?.build().await?;

        Ok(MockDBus {
            connection,
            address,
            process,
        })
    }

    pub fn shutdown(mut self) -> Result<()> {
        let pid = match self.process.id() {
            Some(id) => id,
            None => return Ok(()),
        };
        let pid: pid_t = match pid.try_into() {
            Ok(pid) => pid,
            Err(message) => bail!("Unable to get pid_t from command {message}"),
        };
        signal::kill(Pid::from_raw(pid), signal::Signal::SIGINT)?;
        for _ in [0..10] {
            // Wait for the process to exit synchronously, but not for too long
            if self.process.try_wait()?.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_micros(100));
        }
        Ok(())
    }
}

impl Test {
    pub fn path(&self) -> &Path {
        self.base.path()
    }
}

impl TestHandle {
    pub async fn new_dbus(&mut self) -> Result<Connection> {
        let dbus = MockDBus::new().await?;
        let connection = dbus.connection.clone();
        *self.test.dbus_address.lock().await = Some(dbus.address.clone());
        self.test.mock_dbus.set(Some(dbus));
        Ok(connection)
    }

    pub async fn dbus_address(&self) -> Option<Address> {
        (*self.test.dbus_address.lock().await).clone()
    }
}

impl Drop for TestHandle {
    fn drop(&mut self) {
        stop();
    }
}

pub struct InterfaceIntrospection<'a> {
    interface: zbus_xml::Interface<'a>,
}

impl<'a> InterfaceIntrospection<'a> {
    pub async fn from_remote<'p, I, P>(connection: &Connection, path: P) -> Result<Self>
    where
        I: Interface,
        P: TryInto<ObjectPath<'p>>,
        P::Error: Into<zbus::Error>,
    {
        let iface_ref = connection.object_server().interface::<_, I>(path).await?;
        let iface = iface_ref.get().await;
        let mut remote_interface_string = String::from(
            "<node name=\"/\" xmlns:doc=\"http://www.freedesktop.org/dbus/1.0/doc.dtd\">",
        );
        iface.introspect_to_writer(&mut remote_interface_string, 0);
        remote_interface_string.push_str("</node>");
        Self::from_xml(remote_interface_string.as_bytes(), I::name().to_string())
    }

    pub async fn from_local<'p, P: AsRef<Path>, S: AsRef<str>>(
        path: P,
        interface: S,
    ) -> Result<Self> {
        let local_interface_string = read(path.as_ref()).await?;
        Self::from_xml(local_interface_string.as_ref(), interface)
    }

    fn from_xml<S: AsRef<str>>(xml: &[u8], iface_name: S) -> Result<Self> {
        let node = Node::from_reader(xml)?;
        let interfaces = node.interfaces();
        let mut interface = None;
        for iface in interfaces {
            if iface.name() == iface_name.as_ref() {
                interface = Some(iface.clone());
                break;
            }
        }
        Ok(if let Some(interface) = interface {
            InterfaceIntrospection { interface }
        } else {
            bail!("No interface found");
        })
    }

    fn collect_methods(&self) -> HashMap<String, &Method<'_>> {
        let mut map = HashMap::new();
        for method in self.interface.methods() {
            map.insert(method.name().to_string(), method);
        }
        map
    }

    fn collect_properties(&self) -> HashMap<String, &Property<'_>> {
        let mut map = HashMap::new();
        for prop in self.interface.properties() {
            map.insert(prop.name().to_string(), prop);
        }
        map
    }

    fn collect_signals(&self) -> HashMap<String, &Signal<'_>> {
        let mut map = HashMap::new();
        for signal in self.interface.signals() {
            map.insert(signal.name().to_string(), signal);
        }
        map
    }

    fn compare_methods(&self, other: &InterfaceIntrospection<'_>) -> u32 {
        let local_methods = self.collect_methods();
        let local_method_names: HashSet<&String> = local_methods.keys().collect();
        let other_methods = other.collect_methods();
        let other_method_names: HashSet<&String> = other_methods.keys().collect();

        let mut issues = 0;

        for key in local_method_names.union(&other_method_names) {
            let Some(local_method) = local_methods.get(*key) else {
                error!("Method {key} missing on self");
                issues += 1;
                continue;
            };

            let Some(other_method) = other_methods.get(*key) else {
                error!("Method {key} missing on other");
                issues += 1;
                continue;
            };

            if local_method.args().len() != other_method.args().len() {
                error!("Different arguments between {local_method:?} and {other_method:?}");
                issues += 1;
                continue;
            }

            for (local_arg, other_arg) in
                zip(local_method.args().iter(), other_method.args().iter())
            {
                if local_arg.direction() != other_arg.direction() {
                    error!("Arguments {local_arg:?} and {other_arg:?} differ in direction");
                    issues += 1;
                    continue;
                }
                if local_arg.ty() != other_arg.ty() {
                    error!("Arguments {local_arg:?} and {other_arg:?} differ in type");
                    issues += 1;
                    continue;
                }
            }
        }

        issues
    }

    fn compare_properties(&self, other: &InterfaceIntrospection<'_>) -> u32 {
        let local_properties = self.collect_properties();
        let local_property_names: HashSet<&String> = local_properties.keys().collect();

        let other_properties = other.collect_properties();
        let other_property_names: HashSet<&String> = other_properties.keys().collect();

        let mut issues = 0;

        for key in local_property_names.union(&other_property_names) {
            let Some(local_property) = local_properties.get(*key) else {
                error!("Property {key} missing on self");
                issues += 1;
                continue;
            };

            let Some(other_property) = other_properties.get(*key) else {
                error!("Property {key} missing on other");
                issues += 1;
                continue;
            };

            if local_property.ty() != other_property.ty() {
                error!("Properties {local_property:?} and {other_property:?} differ in type");
                issues += 1;
                continue;
            }

            if local_property.access() != other_property.access() {
                error!("Properties {local_property:?} and {other_property:?} differ in access");
                issues += 1;
                continue;
            }
        }

        issues
    }

    fn compare_signals(&self, other: &InterfaceIntrospection<'_>) -> u32 {
        let local_signals = self.collect_signals();
        let local_signal_names: HashSet<&String> = local_signals.keys().collect();

        let other_signals = other.collect_signals();
        let other_signal_names: HashSet<&String> = other_signals.keys().collect();

        let mut issues = 0;

        for key in local_signal_names.union(&other_signal_names) {
            let Some(local_signal) = local_signals.get(*key) else {
                error!("Signal {key} missing on self");
                issues += 1;
                continue;
            };

            let Some(other_signal) = other_signals.get(*key) else {
                error!("Signal {key} missing on other");
                issues += 1;
                continue;
            };

            for (local_arg, other_arg) in
                zip(local_signal.args().iter(), other_signal.args().iter())
            {
                if local_arg.ty() != other_arg.ty() {
                    error!("Arguments {local_arg:?} and {other_arg:?} differ in type");
                    issues += 1;
                    continue;
                }
            }
        }

        issues
    }

    pub fn compare(&self, other: &InterfaceIntrospection<'_>) -> bool {
        let mut issues = 0;
        issues += self.compare_methods(other);
        issues += self.compare_properties(other);
        issues += self.compare_signals(other);

        issues == 0
    }
}
