// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

use anyhow::anyhow;
use anyhow::Context as _;
use async_trait::async_trait;
use num_traits::cast::ToPrimitive as _;
use std::convert::TryFrom as _;
use std::convert::TryInto as _;

#[async_trait]
impl lwm2m::objects::DataDownload for crate::device::Device {
    async fn set_data(
        &mut self,
        _object: usize,
        _resource: usize,
        _value: Vec<u8>,
    ) -> Result<(), lwm2m::Error> {
        Err(anyhow!("unsupported").into())
    }

    async fn slot(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<i64>, lwm2m::Error> {
        Ok(self.data_download.slot)
    }

    async fn set_slot(
        &mut self,
        _object: usize,
        _resource: usize,
        _value: i64,
    ) -> Result<(), lwm2m::Error> {
        Err(anyhow!("unsupported").into())
    }

    async fn checksum(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<i64>, lwm2m::Error> {
        Ok(self.data_download.checksum)
    }

    async fn set_checksum(
        &mut self,
        _object: usize,
        _resource: usize,
        _value: i64,
    ) -> Result<(), lwm2m::Error> {
        Err(anyhow!("unsupported").into())
    }

    async fn content_tag(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<i64>, lwm2m::Error> {
        Ok(self.data_download.content_tag)
    }

    async fn set_content_tag(
        &mut self,
        _object: usize,
        _resource: usize,
        _value: i64,
    ) -> Result<(), lwm2m::Error> {
        Err(anyhow!("unsupported").into())
    }

    async fn status(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<i64>, lwm2m::Error> {
        Ok((
            self.data_download
                .status
                .0
                .to_i64()
                .ok_or_else(|| anyhow!("data-download-status doesn't fit into i64"))?,
            self.data_download.status.1,
        ))
    }

    #[tracing::instrument(name = "ddl", skip_all)]
    async fn handle_partial_write(
        &mut self,
        _object_instance: usize,
        mut values: std::collections::HashMap<String, lwm2m::Value>,
    ) -> Result<(), lwm2m::Error> {
        if values.len() != 4 {
            return Err(anyhow!("invalid number of values: {}", values.len()).into());
        }

        let slot: u64 = (&values.get("slot").context("`slot` not found")?.data)
            .try_into()
            .context("invalid type for `slot`")?;
        let slot: u32 = slot.try_into().context("`slot` doesn't fit into u32")?;

        let checksum: u64 = (&values.get("checksum").context("`checksum` not found")?.data)
            .try_into()
            .context("invalid type for `checksum`")?;
        let checksum: u16 = checksum
            .try_into()
            .context("`checksum` doesn't fit into u16")?;

        let content_tag: u64 = (&values
            .get("content_tag")
            .context("`content_tag` not found")?
            .data)
            .try_into()
            .context("invalid type for `content_tag`")?;
        let content_tag: u32 = content_tag
            .try_into()
            .context("`content_tag` doesn't fit into u32")?;

        let data: Vec<u8> = values
            .remove("data")
            .context("`data` not found")?
            .data
            .try_into()
            .context("invalid type for `data`")?;

        let checksum_calc = crate::device::XMODEM.checksum(&data);
        if checksum != checksum_calc {
            return Err(anyhow!(
                "invalid checksum. received={:04X} calculated={:04X}",
                checksum,
                checksum_calc
            )
            .into());
        }

        match self.upload_state.as_ref().map(|state| state.kind()) {
            None => (),
            Some(crate::device::FirmwareUploadKind::Fota { .. }) => {
                tracing::info!("Another firmware upload is running, abort");
                self.abort_upload().await;
            }
            Some(crate::device::FirmwareUploadKind::Data {
                slot: slot_current, ..
            }) => {
                if slot != *slot_current {
                    return Err(anyhow!(
                        "another data upload to slot {} is currently running",
                        slot_current
                    )
                    .into());
                }

                tracing::info!(slot = slot_current, "Another data upload is running, abort",);
                self.abort_upload().await;
            }
        }

        self.set_data_download_int(slot, content_tag).await?;

        let go_to_sleep = self.filter_go_to_sleep(Some(crate::device::GOTO_SLEEP_DURATION));
        let report = self
            .request_firmware_init(data.len(), checksum, slot, go_to_sleep)
            .await
            .context("firmware init failed")?;
        if report.status != lsdl::FIRMWARE_UPDATE_STATUS_OK {
            return Err(anyhow!(
                "data download initialization failed, device responded with status code {}",
                report.status
            )
            .into());
        }
        // According to `upload.py`, the offset immediately jumps to the end in
        // case the file was uploaded already.
        // In that case we're supposed to activate it immediately.
        let datalen_u32 = data
            .len()
            .try_into()
            .context("data len doesn't fit into u32")?;
        if report.expected_offset == Some(datalen_u32) {
            self.on_data_upload_finished(Ok(()), slot, std::time::Duration::ZERO)
                .await
                .context("upload finished callback failed")?;
            return Ok(());
        }

        tracing::info!(
            slot,
            "Data upload length={} checksum={:04X} content_tag={:08X} started",
            data.len(),
            checksum,
            content_tag
        );

        // request info to obtain expected image and chunk size
        let firmware_info = self
            .request_firmware_info(Some(crate::device::GOTO_SLEEP_DURATION))
            .await?;
        if data.len()
            != usize::try_from(firmware_info.size)
                .context("failed to convert firmware size to 'usize'")?
        {
            return Err(anyhow!("expected image size not equal to actual image size").into());
        }

        let chunk_size = usize::try_from(firmware_info.chunk_size)
            .context("failed to convert chunk_size to 'usize'")?;

        let now = std::time::SystemTime::now();
        self.data_download = crate::device::DataDownload {
            status: (lwm2m::DataDownloadStatus::Uploading, Some(now)),
            slot: (slot.into(), Some(now)),
            checksum: (checksum.into(), Some(now)),
            content_tag: (content_tag.into(), Some(now)),
        };

        self.publish_resource_instances(
            lwm2m::ObjectType::DataDownload,
            0,
            &[
                (lwm2m::objects::DATA_DOWNLOAD_STATUS, 0),
                (lwm2m::objects::DATA_DOWNLOAD_SLOT, 0),
                (lwm2m::objects::DATA_DOWNLOAD_CHECKSUM, 0),
                (lwm2m::objects::DATA_DOWNLOAD_CONTENT_TAG, 0),
            ],
        )
        .await?;

        self.set_upload_state(crate::device::FirmwareUploadState::new(
            chunk_size,
            crate::device::FirmwareUploadKind::Data { data, slot },
        ));

        Ok(())
    }
}
