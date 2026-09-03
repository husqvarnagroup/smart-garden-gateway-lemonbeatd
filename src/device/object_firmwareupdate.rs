// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::utils::create_version;
use anyhow::anyhow;
use anyhow::Context as _;
use async_trait::async_trait;
use num_traits::cast::ToPrimitive as _;

#[async_trait]
impl lwm2m::objects::FirmwareUpdate for crate::device::Device {
    #[tracing::instrument(name = "fota", skip_all)]
    async fn set_package(
        &mut self,
        _object: usize,
        _resource: usize,
        value: Vec<u8>,
    ) -> Result<(), lwm2m::Error> {
        if matches!(
            self.persistent.firmware_update_state,
            lwm2m::FirmwareUpdateState::DownloadComplete
        ) && value == [0]
        {
            // LWM2M spec: writing an empty string to Package URI Resource or setting the Package
            // Resource to NULL ("\0"), resets the Firmware Update State Machine: the State
            // Resource value is set to Idle and the Update Result Resource value is set to 0.

            tracing::info!("Flush uploaded firmware, ready for new upload");
            self.set_and_publish_firmware_update_status(
                lwm2m::FirmwareUpdateState::Idle,
                lwm2m::FirmwareUpdateResult::Initial,
            )
            .await?;
            return Ok(());
        }

        match self.upload_state.as_ref().map(|state| state.kind()) {
            None => (),
            Some(crate::device::FirmwareUploadKind::Fota { .. }) => {
                tracing::info!("Another firmware upload is running, abort");
                self.abort_upload().await;
            }
            Some(crate::device::FirmwareUploadKind::Data { slot, .. }) => {
                return Err(
                    anyhow!("another Data upload to slot {} is currently running", slot).into(),
                );
            }
        }

        if matches!(
            self.persistent.firmware_update_state,
            lwm2m::FirmwareUpdateState::Downloading | lwm2m::FirmwareUpdateState::Updating
        ) {
            return Err(anyhow!("firmware upload or update already in progress").into());
        }

        self.set_and_publish_firmware_update_status(
            lwm2m::FirmwareUpdateState::Idle,
            lwm2m::FirmwareUpdateResult::Initial,
        )
        .await?;

        let container = crate::device::FirmwareContainer::from_slice(&value)
            .context("invalid firmware format")?;

        // SG-20458: send event?
        self.persistent
            .firmware_update_pkg_version
            .clone_from(&container.firmware_version);
        self.save_persistent_state()
            .await
            .context("can't save persistent state")?;

        if let Err(e) = self.init_firmware_image(value, container, 0).await {
            tracing::error!("Failed to init image `{}`: {:#?}", 0, e);

            self.set_and_publish_firmware_update_status(
                lwm2m::FirmwareUpdateState::Idle,
                lwm2m::FirmwareUpdateResult::IntegrityCheckFail,
            )
            .await?;

            return Err(anyhow!("failed to init first firmware image").into());
        }

        self.set_and_publish_firmware_update_status(
            lwm2m::FirmwareUpdateState::Downloading,
            lwm2m::FirmwareUpdateResult::Success,
        )
        .await?;

        Ok(())
    }

    #[tracing::instrument(name = "fota", skip_all)]
    async fn update(
        &mut self,
        _object: usize,
        _resource: usize,
        _args: Option<Vec<String>>,
    ) -> Result<(), lwm2m::Error> {
        if self.persistent.firmware_update_state != lwm2m::FirmwareUpdateState::DownloadComplete {
            return Err(
                anyhow!("firmware update can only be triggered in downloaded state").into(),
            );
        }
        let firmware_info = self
            .request_firmware_info(Some(crate::device::GOTO_SLEEP_DURATION))
            .await?;
        if firmware_info.size != firmware_info.received_size {
            // a download was started but never finished
            self.set_and_publish_firmware_update_status(
                lwm2m::FirmwareUpdateState::Idle,
                lwm2m::FirmwareUpdateResult::NoFlash,
            )
            .await?;
            return Err(anyhow!(
                "received size {} not equal to actual image size {}",
                firmware_info.received_size,
                firmware_info.size
            )
            .into());
        }

        let reset_after_upload_supported_version = create_version(1, 5, 3);
        let current_version = self.description.version_stack_parsed();
        if firmware_info.size == 0 && current_version < reset_after_upload_supported_version {
            self.set_and_publish_firmware_update_status(
                lwm2m::FirmwareUpdateState::Idle,
                lwm2m::FirmwareUpdateResult::UpdateFailed,
            )
            .await?;
            return Err(anyhow!("device reset after firmware upload not supported").into());
        }

        self.set_and_publish_firmware_update_status(
            lwm2m::FirmwareUpdateState::Updating,
            lwm2m::FirmwareUpdateResult::Initial,
        )
        .await?;

        // NOTE: what we actually want to do is reboot the radiomodule if we
        //       uploaded secondary firmware. But to simplify our code for now:
        //       - we only do this for CBTL since it's the only lemonbeat
        //         device that supports secondary uploads
        //       - we reboot it in all cases so we don't have to remember if we
        //         uploaded secondary firmware or not.
        if self.description.is_cbtl() {
            let now = std::time::Instant::now();
            // reboot the devices radiomodule
            self.set_command(31).await?;

            // use a received device description as an indication that the
            // radio module is back up
            // NOTE: we have to remove it because otherwise, the FOTA logic in
            //       the device description handler sees a device description
            //       with `included=true` and fails the update
            // NOTE: should we maybe delete all pending data from the queue
            //       before triggering to update to fix above issue?
            // NOTE: this also assumes that the device description doesn't
            //       change in a way that needs to be handled the usual way.
            let fut = self
                .receiver
                .remove_one_received_device_description_request(|instant, _description| {
                    instant >= now
                });
            let fut = tokio::time::timeout(std::time::Duration::from_secs(5), fut);

            fut.await
                .context("timed out waiting for device description")?
                .ok_or_else(|| anyhow!("queue error"))?;
        }

        // activate long timeout work around for devices with stack older than 1.5.2
        let timeout_on_update_start_version = create_version(1, 5, 2);
        let current_version = self.description.version_stack_parsed();
        let long_timeout = current_version < timeout_on_update_start_version;

        let result = self
            .firmware_update_start(long_timeout, Some(crate::device::GOTO_SLEEP_IMMEDIATELY))
            .await?;
        // map LB status to lwm2m where possible
        match result {
            lsdl::FIRMWARE_UPDATE_STATUS_OK => {
                self.set_and_publish_firmware_update_status(
                    lwm2m::FirmwareUpdateState::Updating,
                    lwm2m::FirmwareUpdateResult::Success,
                )
                .await?;
            }
            lsdl::FIRMWARE_UPDATE_CHECKSUM_ERR => {
                self.set_and_publish_firmware_update_status(
                    lwm2m::FirmwareUpdateState::Idle,
                    lwm2m::FirmwareUpdateResult::IntegrityCheckFail,
                )
                .await?;
                return Err(anyhow!(
                    "'flash' request returned with status code {:?}",
                    lwm2m::FirmwareUpdateResult::IntegrityCheckFail
                )
                .into());
            }
            _ => {
                self.set_and_publish_firmware_update_status(
                    lwm2m::FirmwareUpdateState::Idle,
                    lwm2m::FirmwareUpdateResult::UpdateFailed,
                )
                .await?;
                return Err(anyhow!(
                    "'flash' request returned with unknown status code: {}",
                    result
                )
                .into());
            }
        }

        Ok(())
    }

    async fn set_package_uri(
        &mut self,
        _object: usize,
        _resource: usize,
        _value: String,
    ) -> Result<(), lwm2m::Error> {
        Err(lwm2m::Error::UnsupportedOptionalResource)
    }

    async fn package_uri(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<String>, lwm2m::Error> {
        Ok(("".to_string(), None))
    }

    async fn state(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<i64>, lwm2m::Error> {
        if self.upload_state.is_some()
            && self.persistent.firmware_update_state != lwm2m::FirmwareUpdateState::Downloading
        {
            tracing::warn!(upload_state=?self.upload_state, firmware_update_state=?self.persistent.firmware_update_state, "inconsistent firmware_update_state");

            // this is an internal issue and doesn't affect the ability of the
            // API user to start another update or abort the current one.
            // So let's simply fix the visible state.
            return Ok((
                lwm2m::FirmwareUpdateState::Downloading
                    .to_i64()
                    .context("FirmwareUpdateState doesn't fit i64")?,
                None,
            ));
        }

        Ok((
            self.persistent
                .firmware_update_state
                .to_i64()
                .ok_or_else(|| anyhow!("failed to retrieve firmware update state"))?,
            None,
        ))
    }

    async fn update_result(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<i64>, lwm2m::Error> {
        Ok((
            self.persistent
                .firmware_update_result
                .to_i64()
                .ok_or_else(|| anyhow!("failed to retrieve firmware update result"))?,
            None,
        ))
    }

    async fn pkg_version(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<String>, lwm2m::Error> {
        Ok((self.persistent.firmware_update_pkg_version.clone(), None))
    }

    async fn firmware_update_delivery_method(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<i64>, lwm2m::Error> {
        Ok((1, None))
    }
}
