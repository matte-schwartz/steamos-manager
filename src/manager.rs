/*
 * Copyright © 2023 Collabora Ltd.
 *
 * SPDX-License-Identifier: MIT
 *
 * Permission is hereby granted, free of charge, to any person obtaining
 * a copy of this software and associated documentation files (the
 * "Software"), to deal in the Software without restriction, including
 * without limitation the rights to use, copy, modify, merge, publish,
 * distribute, sublicense, and/or sell copies of the Software, and to
 * permit persons to whom the Software is furnished to do so, subject to
 * the following conditions:
 *
 * The above copyright notice and this permission notice shall be included
 * in all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
 * EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
 * MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
 * IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
 * CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
 * TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
 * SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
 */

use std::{ffi::OsStr};
use tokio::process::Command;
use zbus_macros::dbus_interface;
pub struct SMManager {
}

async fn script_exit_code(executable: &str, args: &[impl AsRef<OsStr>]) -> Result<bool, Box<dyn std::error::Error>> {
    // Run given script and return true on success
    let mut child = Command::new(executable)
        .args(args)
        .spawn()
        .expect("Failed to spawn {executable}");
    let status = child.wait().await?;
    Ok(status.success())
}

async fn run_script(name: &str, executable: &str, args: &[impl AsRef<OsStr>]) -> bool {
    // Run given script to get exit code and return true on success.
    // Return false on failure, but also print an error if needed
    match script_exit_code(executable, args).await {
        Ok(value) => value,
        Err(err) => { println!("Error running {} {}", name, err); false}
    }
}

async fn script_output(executable: &str, args: &[impl AsRef<OsStr>]) -> Result<String, Box<dyn std::error::Error>> {
    // Run given command and return the output given
    let output = Command::new(executable)
        .args(args).output();

    let output = output.await?;

    let s = match std::str::from_utf8(&output.stdout) {
        Ok(v) => v,
        Err(e) => panic!("Invalid UTF-8 sequence: {}", e),
    };
    Ok(s.to_string())
}

#[dbus_interface(name = "com.steampowered.SteamOSManager1")]
impl SMManager {
    const API_VERSION: u32 = 1;


    async fn say_hello(&self, name: &str) -> String {
        format!("Hello {}!", name)
    }
    
    async fn factory_reset(&self) -> bool {
        // Run steamos factory reset script and return true on success
        run_script("factory reset", "steamos-factory-reset-config", &[""]).await
    }

    async fn disable_wifi_power_management(&self) -> bool {
        // Run polkit helper script and return true on success
        run_script("disable wifi power management", "/usr/bin/steamos-polkit-helpers/steamos-disable-wireless-power-management", &[""]).await
    }
    
    async fn enable_fan_control(&self, enable: bool) -> bool {
        // Run what steamos-polkit-helpers/jupiter-fan-control does
        if enable {
            run_script("enable fan control", "systemcltl", &["start", "jupiter-fan-control-service"]).await
        } else {
            run_script("disable fan control", "systemctl", &["stop", "jupiter-fan-control.service"]).await
        }
    }

    async fn hardware_check_support(&self) -> bool {
        // Run jupiter-check-support note this script does exit 1 for "Support: No" case
        // so no need to parse output, etc.
        run_script("check hardware support", "jupiter-check-support", &[""]).await
    }

    async fn read_als_calibration(&self) -> f32 {
        // Run script to get calibration value
        let result = script_output("/usr/bin/steamos-polkit-helpers/jupiter-get-als-gain", &[""]).await;
        let mut value: f32 = -1.0;
        match result {
            Ok(as_string) => value = as_string.trim().parse().unwrap(),
            Err(message) => println!("Unable to run als calibration script : {}", message),
        }
        
        value
    }

    async fn update_bios(&self) -> bool {
        // Update the bios as needed
        // Return true if the script was successful (though that might mean no update was needed), false otherwise
        run_script("update bios", "/usr/bin/steamos-potlkit-helpers/jupiter-biosupdate", &["--auto"]).await
    }

    async fn update_dock(&self) -> bool {
        // Update the dock firmware as needed
        // Retur true if successful, false otherwise
        run_script("update dock firmware", "/usr/bin/steamos-polkit-helpers/jupiter-dock-updater", &[""]).await
    }

    async fn trim_devices(&self) -> bool {
        // Run steamos-trim-devices script
        // return true on success, false otherwise
        run_script("trim devices", "/usr/bin/steamos-polkit-helpers/steamos-trim-devices", &[""]).await
    }

    async fn format_sdcard(&self) -> bool {
        // Run steamos-format-sdcard script
        // return true on success, false otherwise
        run_script("format sdcard", "/usr/bin/steamos-polkit-helpers/steamos-format-sdcard", &[""]).await
    }
    
    
    /// A version property.
    #[dbus_interface(property)]
    async fn version(&self) -> u32 {
        SMManager::API_VERSION
    }
}

