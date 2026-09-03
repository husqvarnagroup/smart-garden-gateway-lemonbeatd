// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Load data from storage into [Device](super::Device) and vise-versa

use crate::crypto;
use crate::device::ValueDescriptionList as _;
use crate::storage;
use crate::Error;
use anyhow::Context as _;

/// Returns [true] if the filename of path `p` matched `re`
///
/// Returns [false] if any error occurs like a filename that can't be
/// represented by [str].
fn filename_matches_re<P: AsRef<std::path::Path>>(p: P, re: &regex::Regex) -> bool {
    p.as_ref()
        .file_name()
        .and_then(|filename| filename.to_str())
        .is_some_and(|filename| re.is_match(filename))
}

/// Load all value descriptions from the devices `workdir`
pub(super) fn load_value_descriptions<P: AsRef<std::path::Path>>(
    workdir: P,
) -> Result<Vec<storage::ValueDescription>, Error> {
    lazy_static::lazy_static! {
        static ref RE_VALUE_DESCRIPTION:regex::Regex = regex::Regex::new(r"^Value_description_\d+\.json$").unwrap();
    }
    let workdir = workdir.as_ref();
    let dir = std::fs::read_dir(workdir.join("Value_description"))
        .context("can't open value-description directory")?;
    let mut descs = Vec::new();

    for entry in dir {
        let entry = entry.context("can't get entry")?;

        if !filename_matches_re(entry.path(), &RE_VALUE_DESCRIPTION) {
            continue;
        }

        if entry.file_type().context("can't get filetype")?.is_file() {
            let path = entry.path();
            descs.push(
                storage::ValueDescription::load(&path)
                    .with_context(|| format!("can't load value-description from `{path:?}`"))?,
            );
        }
    }

    Ok(descs)
}

/// Load all values from the devices `workdir`
///
/// `desc` is used to validate the values against their specifications.
pub(super) fn load_values<P: AsRef<std::path::Path>>(
    workdir: P,
    descs: &[storage::ValueDescription],
) -> Result<Vec<storage::Value>, Error> {
    lazy_static::lazy_static! {
        static ref RE_VALUE:regex::Regex = regex::Regex::new(r"^Value_\d+r\.json$").unwrap();
    }
    let workdir = workdir.as_ref();
    let dir = std::fs::read_dir(workdir.join("Value")).context("can't open value directory")?;
    let mut vals = Vec::new();

    for entry in dir {
        let entry = entry.context("can't get entry")?;

        if !filename_matches_re(entry.path(), &RE_VALUE) {
            continue;
        }

        if entry.file_type().context("can't get filetype")?.is_file() {
            let path = entry.path();
            vals.push(
                storage::Value::load(&path, descs)
                    .with_context(|| format!("can't load value from `{path:?}`"))?,
            );
        }
    }

    Ok(vals)
}

/// Load a device from `devdir`
///
/// If everything is successful, this spawns the task and returns a
/// [DeviceHandle](super::DeviceHandle)
pub fn load_spawn<P: AsRef<std::path::Path>>(
    interface: std::net::SocketAddr,
    devdir: P,
    lwm2m_pub_service: lwm2m::PubService,
    radiomodule: crate::radiomodule::RadioModuleHandle,
    description: Box<crate::storage::DeviceDescription>,
    radio_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    network: std::sync::Arc<crypto::Network>,
) -> Result<crate::device::DeviceHandle, Error> {
    let persistent = match crate::storage::PersistentState::load(devdir.as_ref()) {
        Err(e) => {
            // This will happen if the device was created by shadoway.
            // As soon as we've loaded the device and written the persistent
            // state this should not happen anymore. If it does anyway
            // we don't expect losing it's data to be THAT bad.
            // Distinguishing between those cases wouldn't even have any
            // benefit since we can't or shouldn't react differently to it
            // expect for using a different log level.
            tracing::info!(
                "failed to load persistent state (normal after migration): {:?}",
                e
            );

            // NOTE: We assume that the device is offline because that's our
            //       best guess at this point. It's also not that important
            //       since it should only happen once per device.
            crate::storage::PersistentState::new(crate::storage::ConnectionStatus::Offline)
        }
        Ok(v) => v,
    };

    let (mut device, handle) = crate::device::Device::new(
        interface,
        devdir.as_ref().to_path_buf(),
        lwm2m_pub_service,
        radiomodule,
        description,
        network,
        Some(persistent),
        crate::device::NewDeviceSource::Storage,
    );

    // shadoway used to persist memory information. We no longer need those files,
    // so let's delete the directory if it exists. Could be removed in 2024 to save a few clock
    // cycles at each device load.
    let memory_path = devdir.as_ref().join("Memory_information");
    if memory_path.exists() {
        if let Err(e) = std::fs::remove_dir_all(memory_path) {
            tracing::warn!("Failed to remove Memory_information directory: {:?}", e);
        }
    }

    device.value_descriptions =
        crate::device::load_value_descriptions(&devdir).context("can't load value descriptions")?;

    device.values = crate::device::load_values(&devdir, &device.value_descriptions)
        .context("can't load values")?;

    device
        .value_descriptions
        .validate_values(&device.values)
        .context("failed to validate values")?;

    tracing::debug!("Loaded device: {:#?}", device.description);

    // We don't want to publish loaded devices because we only do that on
    // startup and we expect users to react to that by fetching the whole
    // device list.
    // It's important to pretend to have done so though because we don't
    // send events unless we're ready.
    // NOTE: technically we're always included since we don't load
    //       unincluded devices but this function can't really know that
    //       and shouldn't make that assumption.
    if device.description.included {
        device.publication_state = crate::device::PublicationState::Ready;
    }

    device.spawn(handle.clone_uncounted(), radio_lock);
    Ok(handle)
}

impl crate::device::Device {
    /// Write this devices description to disk (maybe)
    ///
    /// Does nothing if [storage::DeviceDescription::loadsave_enabled] returns
    /// `false`.
    pub(super) async fn save_device_description(&mut self) -> Result<(), Error> {
        if !self.description.loadsave_enabled() {
            return Ok(());
        }

        self.description
            .save(&self.devdir)
            .await
            .context("can't save device description")?;

        Ok(())
    }

    /// Write this devices persistent state to disk (maybe)
    ///
    /// Does nothing if [storage::DeviceDescription::loadsave_enabled] returns
    /// `false`.
    pub(super) async fn save_persistent_state(&mut self) -> Result<(), Error> {
        if !self.description.loadsave_enabled() {
            return Ok(());
        }

        self.persistent
            .save(&self.devdir)
            .await
            .context("can't save persistent state")?;

        Ok(())
    }

    /// Write all value descriptions to disk (maybe)
    ///
    /// Does nothing if [storage::DeviceDescription::loadsave_enabled] returns
    /// `false`.
    pub(super) async fn save_value_descriptions(&mut self) -> Result<(), Error> {
        if !self.description.loadsave_enabled() {
            return Ok(());
        }

        let dir = self.devdir.join("Value_description");

        if tokio::fs::metadata(&dir).await.is_ok() {
            tokio::fs::remove_dir_all(&dir)
                .await
                .context("can't delete value-descriptions directory")?;
        }
        tokio::fs::create_dir_all(&dir)
            .await
            .context("can't create value-description directory")?;

        for value_description in &self.value_descriptions {
            value_description
                .save(&dir)
                .await
                .context("can't write value-description")?;
        }

        Ok(())
    }

    /// returns the path to the directory which holds all values for this device
    pub(super) fn values_path(&self) -> std::path::PathBuf {
        self.devdir.join("Value")
    }

    /// write all value descriptions to disk (maybe)
    ///
    /// does nothing if [storage::DeviceDescription::loadsave_enabled] returns
    /// `false`.
    pub(super) async fn save_values(&mut self) -> Result<(), Error> {
        if !self.description.loadsave_enabled() {
            return Ok(());
        }

        let dir = self.values_path();

        // We risk leaving behind trash to prevent losing the MSC due to a
        // reset after deleting the directory and before storing the new values.
        if tokio::fs::metadata(&dir).await.is_ok() {
            tokio::fs::remove_dir_all(&dir)
                .await
                .context("can't delete values directory")?;
        }
        tokio::fs::create_dir_all(&dir)
            .await
            .context("can't create values directory")?;

        for value in &self.values {
            value.save(&dir).await.context("can't write value")?;
        }

        Ok(())
    }
}
