/*
 * Copyright © 2025 Collabora Ltd.
 * Copyright © 2025 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{anyhow, Result};
use std::fs::File;
use std::io::Read;
use std::io::Write;
//use std::process::{Child, Command};
use serde_json::{json, Value};
use std::process::Command;
use tracing::{info, warn};
use zbus::Connection;

use crate::systemd::SystemdUnit;

const ORCA_SETTINGS: &str = "/home/deck/.local/share/orca/user-settings.conf";
const PITCH_SETTING: &str = "average-pitch";
const RATE_SETTING: &str = "rate";
const VOLUME_SETTING: &str = "gain";

pub(crate) struct OrcaManager<'dbus> {
    orca_unit: SystemdUnit<'dbus>,
    rate: f64,
    pitch: f64,
    volume: f64,
    enabled: bool,
}

impl<'dbus> OrcaManager<'dbus> {
    pub async fn new(connection: &Connection) -> Result<OrcaManager<'dbus>> {
        let mut manager = OrcaManager {
            orca_unit: SystemdUnit::new(connection.clone(), "orca.service").await?,
            rate: 1.0,
            pitch: 1.0,
            volume: 1.0,
            enabled: true,
        };
        manager.load_values()?;
        Ok(manager)
    }

    pub async fn enabled(&self) -> Result<bool> {
        // Check if screen reader is enabled
        Ok(self.enabled)
    }

    pub async fn set_enabled(&mut self, enable: bool) -> std::io::Result<()> {
        // Set screen reader enabled based on value of enable
        if enable {
            // Enable screen reader gsettings
            let _ = Command::new("gsettings")
                .args([
                    "set",
                    "org.gnome.desktop.a11y.applications",
                    "screen-reader-enabled",
                    "true",
                ])
                .spawn();
            // Set orca enabled also
            let _setting_result = self.set_orca_enabled(true);
            let _result = self.restart_orca().await;
        } else {
            // Disable screen reader gsettings
            let _ = Command::new("gsettings")
                .args([
                    "set",
                    "org.gnome.desktop.a11y.applications",
                    "screen-reader-enabled",
                    "false",
                ])
                .spawn();
            // Set orca disabled also
            let _setting_result = self.set_orca_enabled(false);
            // Stop orca
            let _result = self.stop_orca().await;
        }
        self.enabled = enable;
        Ok(())
    }

    pub async fn pitch(&self) -> Result<f64> {
        Ok(self.pitch)
    }

    pub async fn set_pitch(&mut self, pitch: f64) -> Result<()> {
        info!("set_pitch called with {:?}", pitch);

        let result = self.set_orca_option(PITCH_SETTING.to_owned(), pitch);
        match result {
            Ok(_) => {
                self.pitch = pitch;
                Ok(())
            }
            Err(_) => Err(anyhow!("Unable to set orca pitch value")),
        }
    }

    pub async fn rate(&self) -> Result<f64> {
        Ok(self.rate)
    }

    pub async fn set_rate(&mut self, rate: f64) -> Result<()> {
        info!("set_rate called with {:?}", rate);

        let result = self.set_orca_option(RATE_SETTING.to_owned(), rate);
        match result {
            Ok(_) => {
                self.rate = rate;
                Ok(())
            }
            Err(_) => Err(anyhow!("Unable to set orca rate")),
        }
    }

    pub async fn volume(&self) -> Result<f64> {
        Ok(self.volume)
    }

    pub async fn set_volume(&mut self, volume: f64) -> Result<()> {
        info!("set_volume called with {:?}", volume);

        let result = self.set_orca_option(VOLUME_SETTING.to_owned(), volume);
        match result {
            Ok(_) => {
                self.volume = volume;
                Ok(())
            }
            Err(_) => Err(anyhow!("Unable to set orca volume")),
        }
    }

    fn set_orca_enabled(&mut self, enabled: bool) -> Result<()> {
        // Change json file
        let mut file = File::open(ORCA_SETTINGS)?;
        let mut data = String::new();
        file.read_to_string(&mut data)?;

        let mut json: Value = serde_json::from_str(&data)?;

        if let Some(general) = json.get_mut("general") {
            if let Some(enable_speech) = general.get_mut("enableSpeech") {
                *enable_speech = json!(&enabled);
            } else {
                warn!("No enabledSpeech value in general in orca settings");
            }
        } else {
            warn!("No general section in orca settings");
        }

        data = serde_json::to_string_pretty(&json)?;

        let mut out_file = File::create(ORCA_SETTINGS)?;
        match out_file.write_all(&data.into_bytes()) {
            Ok(_) => {
                self.enabled = enabled;
                Ok(())
            }
            Err(_) => Err(anyhow!("Unable to write orca settings file")),
        }
    }

    fn load_values(&mut self) -> Result<()> {
        info!("Loading orca values from user-settings.conf");
        let mut file = File::open(ORCA_SETTINGS)?;
        let mut data = String::new();
        file.read_to_string(&mut data)?;

        let json: Value = serde_json::from_str(&data)?;

        if let Some(profiles) = json.get("profiles") {
            if let Some(default_profile) = profiles.get("default") {
                if let Some(voices) = default_profile.get("voices") {
                    if let Some(default_voice) = voices.get("default") {
                        if let Some(pitch) = default_voice.get(PITCH_SETTING.to_owned()) {
                            self.pitch = pitch
                                .as_f64()
                                .expect("Unable to convert orca pitch setting to float value");
                        } else {
                            warn!("Unable to load default pitch from orca user-settings.conf");
                        }

                        if let Some(rate) = default_voice.get(RATE_SETTING.to_owned()) {
                            self.rate = rate
                                .as_f64()
                                .expect("Unable to convert orca rate setting to float value");
                        } else {
                            warn!("Unable to load default voice rate from orca user-settings.conf");
                        }

                        if let Some(volume) = default_voice.get(VOLUME_SETTING.to_owned()) {
                            self.volume = volume
                                .as_f64()
                                .expect("Unable to convert orca volume value to float value");
                        } else {
                            warn!(
                                "Unable to load default voice volume from orca user-settings.conf"
                            );
                        }
                    } else {
                        warn!("Orca user-settings.conf missing default voice");
                    }
                } else {
                    warn!("Orca user-settings.conf missing voices list");
                }
            } else {
                warn!("Orca user-settings.conf missing default profile");
            }
        } else {
            warn!("Orca user-settings.conf missing profiles");
        }
        info!(
            "Done loading orca user-settings.conf, values: Rate: {:?}, Pitch: {:?}, Volume: {:?}",
            self.rate, self.pitch, self.volume
        );

        Ok(())
    }

    fn set_orca_option(&self, option: String, value: f64) -> Result<()> {
        // Verify option is one we know about
        // Verify value is in range
        // Change json file
        let mut file = File::open(ORCA_SETTINGS)?;
        let mut data = String::new();
        file.read_to_string(&mut data)?;

        let mut json: Value = serde_json::from_str(&data)?;

        if let Some(profiles) = json.get_mut("profiles") {
            if let Some(default_profile) = profiles.get_mut("default") {
                if let Some(voices) = default_profile.get_mut("voices") {
                    if let Some(default_voice) = voices.get_mut("default") {
                        if let Some(mut_option) = default_voice.get_mut(&option) {
                            *mut_option = json!(value);
                        } else {
                            let object = default_voice.as_object_mut().ok_or(anyhow!(
                                "Unable to generate mutable object for adding {:?} with value {:?}",
                                &option,
                                value
                            ))?;
                            object.insert(option, json!(value));
                        }
                    } else {
                        warn!(
                            "No default voice in voices list to set {:?} to {:?} in",
                            &option, value
                        );
                    }
                } else {
                    warn!(
                        "No voices in default profile to set {:?} to {:?} in",
                        &option, value
                    );
                }
            } else {
                warn!("No default profile to set {:?} to {:?} in", &option, value);
            }
        } else {
            warn!("No profiles in orca user-settings.conf to modify");
        }

        data = serde_json::to_string_pretty(&json)?;

        let mut out_file = File::create(ORCA_SETTINGS)?;
        match out_file.write_all(&data.into_bytes()) {
            Ok(_) => Ok(()),
            Err(_) => Err(anyhow!("Unable to write orca settings file")),
        }
    }

    async fn restart_orca(&self) -> Result<()> {
        info!("Restarting orca...");
        let _result = self.orca_unit.restart().await;
        info!("Done restarting orca...");
        Ok(())
    }

    async fn stop_orca(&self) -> Result<()> {
        // Stop orca user unit
        info!("Stopping orca...");
        let _result = self.orca_unit.stop().await;
        info!("Done stopping orca...");
        Ok(())
    }
}
