/*
 * Copyright © 2025 Collabora Ltd.
 * Copyright © 2025 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{anyhow, bail, ensure, Result};
use gio::{prelude::SettingsExt, Settings};
#[cfg(test)]
use input_linux::InputEvent;
#[cfg(not(test))]
use input_linux::{EventKind, InputId, UInputHandle};
use input_linux::{EventTime, Key, KeyEvent, KeyState, SynchronizeEvent};
use lazy_static::lazy_static;
#[cfg(not(test))]
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use num_enum::TryFromPrimitive;
use serde_json::{Map, Value};
use std::collections::HashMap;
#[cfg(test)]
use std::collections::VecDeque;
#[cfg(not(test))]
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::ops::RangeInclusive;
#[cfg(not(test))]
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::SystemTime;
use strum::{Display, EnumString};
use tokio::fs::{read_to_string, write};
use tracing::{debug, error, info, trace, warn};
#[cfg(not(test))]
use xdg::BaseDirectories;
use zbus::Connection;

#[cfg(test)]
use crate::path;
use crate::systemd::SystemdUnit;

#[cfg(test)]
const TEST_ORCA_SETTINGS: &str = "data/test-orca-settings.conf";
#[cfg(test)]
const ORCA_SETTINGS: &str = "orca-settings.conf";

#[cfg(not(test))]
const ORCA_SETTINGS: &str = "orca/user-settings.conf";
const PITCH_SETTING: &str = "average-pitch";
const RATE_SETTING: &str = "rate";
const VOLUME_SETTING: &str = "gain";
const ENABLE_SETTING: &str = "enableSpeech";

const A11Y_SETTING: &str = "org.gnome.desktop.a11y.applications";
const SCREEN_READER_SETTING: &str = "screen-reader-enabled";
const KEYBOARD_NAME: &str = "steamos-manager";

const PITCH_DEFAULT: f64 = 5.0;
const RATE_DEFAULT: f64 = 50.0;
const VOLUME_DEFAULT: f64 = 10.0;

lazy_static! {
    static ref VALID_SETTINGS: HashMap<&'static str, RangeInclusive<f64>> = HashMap::from_iter([
        (PITCH_SETTING, (0.0..=10.0)),
        (RATE_SETTING, (0.0..=100.0)),
        (VOLUME_SETTING, (0.0..=10.0)),
    ]);
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
#[repr(u32)]
pub enum ScreenReaderMode {
    Browse = 0,
    Focus = 1,
}

pub(crate) struct UInputDevice {
    #[cfg(not(test))]
    handle: UInputHandle<OwnedFd>,
    #[cfg(test)]
    queue: VecDeque<InputEvent>,
    name: String,
    open: bool,
}

impl UInputDevice {
    #[cfg(not(test))]
    pub(crate) fn new() -> Result<UInputDevice> {
        let fd = OpenOptions::new()
            .write(true)
            .create(false)
            .open("/dev/uinput")?
            .into();

        let mut flags = OFlag::from_bits_retain(fcntl(&fd, FcntlArg::F_GETFL)?);
        flags.set(OFlag::O_NONBLOCK, true);
        fcntl(&fd, FcntlArg::F_SETFL(flags))?;

        Ok(UInputDevice {
            handle: UInputHandle::new(fd),
            name: String::new(),
            open: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn new() -> Result<UInputDevice> {
        Ok(UInputDevice {
            queue: VecDeque::new(),
            name: String::new(),
            open: false,
        })
    }

    pub(crate) fn set_name(&mut self, name: String) -> Result<()> {
        ensure!(!self.open, "Cannot change name after opening");
        self.name = name;
        Ok(())
    }

    #[cfg(not(test))]
    pub(crate) fn open(&mut self) -> Result<()> {
        ensure!(!self.open, "Cannot reopen uinput handle");

        self.handle.set_evbit(EventKind::Key)?;
        self.handle.set_keybit(Key::Insert)?;
        self.handle.set_keybit(Key::A)?;

        let input_id = InputId {
            bustype: input_linux::sys::BUS_VIRTUAL,
            vendor: 0x28DE,
            product: 0,
            version: 0,
        };
        self.handle
            .create(&input_id, self.name.as_bytes(), 0, &[])?;
        self.open = true;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn open(&mut self) -> Result<()> {
        ensure!(!self.open, "Cannot reopen uinput handle");
        self.open = true;
        Ok(())
    }

    fn system_time() -> Result<EventTime> {
        let duration = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
        Ok(EventTime::new(
            duration.as_secs().try_into()?,
            duration.subsec_micros().into(),
        ))
    }

    fn send_key_event(&mut self, key: Key, value: KeyState) -> Result<()> {
        let tv = UInputDevice::system_time().unwrap_or_else(|err| {
            warn!("System time error: {err}");
            EventTime::default()
        });

        let ev = KeyEvent::new(tv, key, value);
        let syn = SynchronizeEvent::report(tv);
        #[cfg(not(test))]
        self.handle.write(&[*ev.as_ref(), *syn.as_ref()])?;
        #[cfg(test)]
        self.queue.extend(&[*ev.as_ref(), *syn.as_ref()]);
        Ok(())
    }

    pub(crate) fn key_down(&mut self, key: Key) -> Result<()> {
        self.send_key_event(key, KeyState::PRESSED)
    }

    pub(crate) fn key_up(&mut self, key: Key) -> Result<()> {
        self.send_key_event(key, KeyState::RELEASED)
    }
}

pub(crate) struct OrcaManager<'dbus> {
    orca_unit: SystemdUnit<'dbus>,
    rate: f64,
    pitch: f64,
    volume: f64,
    enabled: bool,
    mode: ScreenReaderMode,
    keyboard: UInputDevice,
}

impl<'dbus> OrcaManager<'dbus> {
    pub async fn new(connection: &Connection) -> Result<OrcaManager<'dbus>> {
        let mut manager = OrcaManager {
            orca_unit: SystemdUnit::new(connection.clone(), "orca.service").await?,
            rate: RATE_DEFAULT,
            pitch: PITCH_DEFAULT,
            volume: VOLUME_DEFAULT,
            enabled: true,
            // Always start in browse mode for now, since we have no storage to remember this property
            mode: ScreenReaderMode::Browse,
            keyboard: UInputDevice::new()?,
        };
        let _ = manager
            .load_values()
            .await
            .inspect_err(|e| warn!("Failed to load orca configuration: {e}"));
        let a11ysettings = Settings::new(A11Y_SETTING);
        manager.enabled = a11ysettings.boolean(SCREEN_READER_SETTING);
        manager.keyboard.set_name(KEYBOARD_NAME.to_string())?;
        manager.keyboard.open()?;

        Ok(manager)
    }

    #[cfg(not(test))]
    fn settings_path(&self) -> Result<PathBuf> {
        let xdg_base = BaseDirectories::new();
        Ok(xdg_base
            .get_data_home()
            .ok_or(anyhow!("No XDG_DATA_HOME found"))?
            .join(ORCA_SETTINGS))
    }

    #[cfg(test)]
    fn settings_path(&self) -> Result<PathBuf> {
        Ok(path(ORCA_SETTINGS))
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub async fn set_enabled(&mut self, enable: bool) -> Result<()> {
        if self.enabled == enable {
            return Ok(());
        }

        #[cfg(not(test))]
        {
            let a11ysettings = Settings::new(A11Y_SETTING);
            a11ysettings
                .set_boolean(SCREEN_READER_SETTING, enable)
                .map_err(|e| anyhow!("Unable to set screen reader enabled gsetting, {e}"))?;
        }
        if let Err(e) = self.set_orca_enabled(enable).await {
            match e.downcast_ref::<std::io::Error>() {
                Some(e) if e.kind() == ErrorKind::NotFound => (),
                _ => return Err(e),
            }
        }
        if enable {
            self.restart_orca().await?;
        } else {
            self.stop_orca().await?;
        }
        self.enabled = enable;
        Ok(())
    }

    pub fn pitch(&self) -> f64 {
        self.pitch
    }

    pub async fn set_pitch(&mut self, pitch: f64) -> Result<()> {
        trace!("set_pitch called with {pitch}");

        self.set_orca_option(PITCH_SETTING, pitch).await?;
        self.pitch = pitch;
        Ok(())
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    pub async fn set_rate(&mut self, rate: f64) -> Result<()> {
        trace!("set_rate called with {rate}");

        self.set_orca_option(RATE_SETTING, rate).await?;
        self.rate = rate;
        Ok(())
    }

    pub fn volume(&self) -> f64 {
        self.volume
    }

    pub async fn set_volume(&mut self, volume: f64) -> Result<()> {
        trace!("set_volume called with {volume}");

        self.set_orca_option(VOLUME_SETTING, volume).await?;
        self.volume = volume;
        Ok(())
    }

    pub fn mode(&self) -> ScreenReaderMode {
        self.mode
    }

    pub async fn set_mode(&mut self, mode: ScreenReaderMode) -> Result<()> {
        // Use insert+A twice to switch to focus mode sticky
        // Use insert+A three times to switch to browse mode sticky
        match mode {
            ScreenReaderMode::Focus => {
                self.keyboard.key_down(Key::Insert)?;
                self.keyboard.key_down(Key::A)?;
                self.keyboard.key_up(Key::A)?;
                self.keyboard.key_down(Key::A)?;
                self.keyboard.key_up(Key::A)?;
                self.keyboard.key_up(Key::Insert)?;
            }
            ScreenReaderMode::Browse => {
                self.keyboard.key_down(Key::Insert)?;
                self.keyboard.key_down(Key::A)?;
                self.keyboard.key_up(Key::A)?;
                self.keyboard.key_down(Key::A)?;
                self.keyboard.key_up(Key::A)?;
                self.keyboard.key_down(Key::A)?;
                self.keyboard.key_up(Key::A)?;
                self.keyboard.key_up(Key::Insert)?;
            }
        }
        self.mode = mode;

        Ok(())
    }

    async fn set_orca_enabled(&mut self, enabled: bool) -> Result<()> {
        // Change json file
        let data = read_to_string(self.settings_path()?).await?;
        let mut json: Value = serde_json::from_str(&data)?;

        let general = json
            .as_object_mut()
            .ok_or(anyhow!("orca user-settings.conf json is not an object"))?
            .entry("general")
            .or_insert(Value::Object(Map::new()));
        general
            .as_object_mut()
            .ok_or(anyhow!("orca user-settings.conf general is not an object"))?
            .insert(ENABLE_SETTING.to_string(), Value::Bool(enabled));

        let data = serde_json::to_string_pretty(&json)?;
        Ok(write(self.settings_path()?, data.as_bytes()).await?)
    }

    async fn load_values(&mut self) -> Result<()> {
        debug!("Loading orca values from user-settings.conf");
        let data = read_to_string(self.settings_path()?).await?;
        let json: Value = serde_json::from_str(&data)?;

        let Some(default_voice) = json
            .get("profiles")
            .and_then(|profiles| profiles.get("default"))
            .and_then(|default_profile| default_profile.get("voices"))
            .and_then(|voices| voices.get("default"))
        else {
            warn!("Orca user-settings.conf missing default voice");
            self.pitch = PITCH_DEFAULT;
            self.rate = RATE_DEFAULT;
            self.volume = VOLUME_DEFAULT;
            return Ok(());
        };
        if let Some(pitch) = default_voice.get(PITCH_SETTING) {
            self.pitch = pitch.as_f64().unwrap_or_else(|| {
                error!("Unable to convert orca pitch setting to float value");
                PITCH_DEFAULT
            });
        } else {
            warn!("Unable to load default pitch from orca user-settings.conf");
            self.pitch = PITCH_DEFAULT;
        }

        if let Some(rate) = default_voice.get(RATE_SETTING) {
            self.rate = rate.as_f64().unwrap_or_else(|| {
                error!("Unable to convert orca rate setting to float value");
                RATE_DEFAULT
            });
        } else {
            warn!("Unable to load default voice rate from orca user-settings.conf");
        }

        if let Some(volume) = default_voice.get(VOLUME_SETTING) {
            self.volume = volume.as_f64().unwrap_or_else(|| {
                error!("Unable to convert orca volume value to float value");
                VOLUME_DEFAULT
            });
        } else {
            warn!("Unable to load default voice volume from orca user-settings.conf");
        }
        info!(
            "Done loading orca user-settings.conf, values: Rate: {}, Pitch: {}, Volume: {}",
            self.rate, self.pitch, self.volume
        );

        Ok(())
    }

    async fn set_orca_option(&self, option: &str, value: f64) -> Result<()> {
        if let Some(range) = VALID_SETTINGS.get(option) {
            ensure!(
                range.contains(&value),
                "orca option {option} value {value} out of range"
            );
        } else {
            bail!("Invalid orca option {option}");
        }
        let data = read_to_string(self.settings_path()?).await?;
        let mut json: Value = serde_json::from_str(&data)?;

        let profiles = json
            .as_object_mut()
            .ok_or(anyhow!("orca user-settings.conf json is not an object"))?
            .entry("profiles")
            .or_insert(Value::Object(Map::new()));
        let default_profile = profiles
            .as_object_mut()
            .ok_or(anyhow!("orca user-settings.conf profiles is not an object"))?
            .entry("default")
            .or_insert(Value::Object(Map::new()));
        let voices = default_profile
            .as_object_mut()
            .ok_or(anyhow!(
                "orca user-settings.conf default profile is not an object"
            ))?
            .entry("voices")
            .or_insert(Value::Object(Map::new()));
        let default_voice = voices
            .as_object_mut()
            .ok_or(anyhow!("orca user-settings.conf voices is not an object"))?
            .entry("default")
            .or_insert(Value::Object(Map::new()));
        default_voice
            .as_object_mut()
            .ok_or(anyhow!(
                "orca user-settings.conf default voice is not an object"
            ))?
            .insert(option.to_string(), value.into());

        let data = serde_json::to_string_pretty(&json)?;
        Ok(write(self.settings_path()?, data.as_bytes()).await?)
    }

    async fn restart_orca(&self) -> Result<()> {
        trace!("Restarting orca...");
        self.orca_unit.enable().await?;
        self.orca_unit.restart().await
    }

    async fn stop_orca(&self) -> Result<()> {
        trace!("Stopping orca...");
        self.orca_unit.disable().await?;
        self.orca_unit.stop().await
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::systemd::test::{MockManager, MockUnit};
    use crate::systemd::EnableState;
    use crate::testing;
    use std::time::Duration;
    use tokio::fs::{copy, remove_file};
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_enable_disable() {
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
            .at("/org/freedesktop/systemd1/unit/orca_2eservice", unit)
            .await
            .expect("at");
        object_server
            .at("/org/freedesktop/systemd1", MockManager::default())
            .await
            .expect("at");

        sleep(Duration::from_millis(10)).await;

        let unit = SystemdUnit::new(connection.clone(), "orca.service")
            .await
            .expect("unit");

        let mut manager = OrcaManager::new(&connection)
            .await
            .expect("OrcaManager::new");
        manager.set_enabled(true).await.unwrap();
        assert_eq!(manager.enabled(), true);
        assert_eq!(unit.active().await.unwrap(), true);
        assert_eq!(unit.enabled().await.unwrap(), EnableState::Enabled);

        manager.set_enabled(false).await.unwrap();
        assert_eq!(manager.enabled(), false);
        assert_eq!(unit.active().await.unwrap(), false);
        assert_eq!(unit.enabled().await.unwrap(), EnableState::Disabled);

        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        manager.load_values().await.unwrap();

        manager.set_enabled(true).await.unwrap();
        assert_eq!(manager.enabled(), true);
        assert_eq!(unit.active().await.unwrap(), true);
        assert_eq!(unit.enabled().await.unwrap(), EnableState::Enabled);

        manager.set_enabled(false).await.unwrap();
        assert_eq!(manager.enabled(), false);
        assert_eq!(unit.active().await.unwrap(), false);
        assert_eq!(unit.enabled().await.unwrap(), EnableState::Disabled);
    }

    #[tokio::test]
    async fn test_pitch() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        let set_result = manager.set_pitch(5.0).await;
        assert!(set_result.is_ok());
        assert_eq!(manager.pitch(), 5.0);

        let too_low_result = manager.set_pitch(-1.0).await;
        assert!(too_low_result.is_err());
        assert_eq!(manager.pitch(), 5.0);

        let too_high_result = manager.set_pitch(12.0).await;
        assert!(too_high_result.is_err());
        assert_eq!(manager.pitch(), 5.0);

        remove_file(h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let nofile_result = manager.set_pitch(7.0).await;
        assert_eq!(manager.pitch(), 5.0);
        assert!(nofile_result.is_err());
    }

    #[tokio::test]
    async fn test_rate() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        let set_result = manager.set_rate(5.0).await;
        assert!(set_result.is_ok());
        assert_eq!(manager.rate(), 5.0);

        let too_low_result = manager.set_rate(-1.0).await;
        assert!(too_low_result.is_err());
        assert_eq!(manager.rate(), 5.0);

        let too_high_result = manager.set_rate(101.0).await;
        assert!(too_high_result.is_err());
        assert_eq!(manager.rate(), 5.0);

        remove_file(h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let nofile_result = manager.set_rate(7.0).await;
        assert_eq!(manager.rate(), 5.0);
        assert!(nofile_result.is_err());
    }

    #[tokio::test]
    async fn test_volume() {
        let mut h = testing::start();
        copy(TEST_ORCA_SETTINGS, h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let mut manager = OrcaManager::new(&h.new_dbus().await.expect("new_dbus"))
            .await
            .expect("OrcaManager::new");
        let set_result = manager.set_volume(5.0).await;
        assert!(set_result.is_ok());
        assert_eq!(manager.volume(), 5.0);

        let too_low_result = manager.set_volume(-1.0).await;
        assert!(too_low_result.is_err());
        assert_eq!(manager.volume(), 5.0);

        let too_high_result = manager.set_volume(12.0).await;
        assert!(too_high_result.is_err());
        assert_eq!(manager.volume(), 5.0);

        remove_file(h.test.path().join(ORCA_SETTINGS))
            .await
            .unwrap();
        let nofile_result = manager.set_volume(7.0).await;
        assert_eq!(manager.volume(), 5.0);
        assert!(nofile_result.is_err());
    }
}
