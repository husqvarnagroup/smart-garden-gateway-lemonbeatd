// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Everything that utilizes the lemonbeat firmware upload protocol.
//!
//! That includes:
//! - FOTA
//! - Data download
//! - LONA map upload

use crate::traits::CursorExt as _;
use crate::traits::ReadExt as _;
use crate::Error;
use anyhow::anyhow;
use anyhow::Context as _;
use crc::Crc;
use derive_more::Debug;
use lwm2m::FirmwareUpdateState;
use rand::Rng as _;
use sha2::Digest as _;
use std::convert::TryFrom as _;
use std::convert::TryInto as _;
use std::io::Read as _;
use std::io::Seek as _;

const CONTENTTAG_CRC: Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);

/// All the info we need from a firmware container.
#[derive(Debug)]
pub(super) struct FirmwareContainer {
    /// Firmware version as provided by the container.
    pub firmware_version: String,
    /// Images that are inside this container and need to be uploaded.
    images: Vec<FirmwareContainerImage>,
}

/// All image info needed during upload.
#[derive(Debug)]
struct FirmwareContainerImage {
    /// The slot to upload the image to.
    slot: u32,
    /// The range of bytes within the full container data that covers this image.
    ///
    /// We use this instead of a reference to prevent having to deal with lifetimes.
    data_range: core::ops::Range<usize>,
    /// The size of this image.
    ///
    /// In theory, `data_range` has that info as well but it's not that easy to
    /// obtain right now.
    len: usize,
    /// The CRC16 of the image data.
    ///
    /// This was calculated by us because the device expects us to provide one
    /// so it can verify the integrity of the uploaded data. The firmware
    /// container doesn't store that because we use a more reliable algorithm
    /// there.
    crc: u16,
}

impl FirmwareContainer {
    /// Parse firmware container inside `data`
    ///
    /// The resulting structs contain offsets into that data so make sure to
    /// not modify it between parsing and uploading.
    pub fn from_slice(data: &[u8]) -> Result<Self, Error> {
        let mut reader = std::io::Cursor::new(data);

        let mut magic = [0; 4];
        reader.read_exact(&mut magic)?;
        if magic != [0x4c, 0x42, 0x46, 0x57] {
            anyhow::bail!("unsupported magic: {:?}", magic);
        }

        let format_version = reader.read_u8()?;
        if format_version != 0x00 {
            anyhow::bail!("unsupported format_version: {}", format_version);
        }

        let firmware_version_length: usize = reader.read_u8()?.into();
        let firmware_version_position = reader.position_usize()?;
        let firmware_version = core::str::from_utf8(
            data.get(
                firmware_version_position
                    ..firmware_version_position
                        .checked_add(firmware_version_length)
                        .context("firmware_version_length is too big")?,
            )
            .context("short read for version")?,
        )
        .context("firmware version is not valid UTF8")?
        .to_string();

        reader.seek(std::io::SeekFrom::Current(
            firmware_version_length.try_into()?,
        ))?;

        let mut images = Vec::new();
        while reader.remaining()? > 64 {
            let slot = reader.read_u32_le()?;

            let data_length: usize = reader.read_u32_le()?.try_into()?;
            let data_start = reader.position_usize()?;
            let data_range = data_start
                ..data_start
                    .checked_add(data_length)
                    .context("data_length too big")?;

            let crc = crate::device::XMODEM.checksum(
                data.get(data_range.clone())
                    .context("short read for data")?,
            );

            reader.seek(std::io::SeekFrom::Current(data_length.try_into()?))?;

            images.push(FirmwareContainerImage {
                slot,
                data_range,
                len: data_length,
                crc,
            });
        }

        // NOTE: This does a reverse sort so image `1` will come last.
        // Initially we did this because we thought that uploading image `1`
        // after (optional) secondary images would remove the need to do
        // anything special during activation. Even though that turned out to
        // not be the case we want to keep this `just in case`.
        // PANIC: if my current understanding of the cmp implementations for
        //        primitives is correct, this can never happen. It'd definitely
        //        be weird though.
        images.sort_by(|a, b| b.slot.partial_cmp(&a.slot).unwrap());

        // Containers that only have secondary images were never tested and we
        // currently don't need or want to support them.
        if !matches!(images.last().map(|img| img.slot), Some(1)) {
            anyhow::bail!("the last slot doesn't have number `1`");
        }

        let checksummed_data = data
            .get(0..reader.position_usize()?)
            .context("short read for container")?;

        let mut checksum = [0; 64];
        reader.read_exact(&mut checksum)?;

        let checksum_calculated = sha2::Sha512::digest(checksummed_data);
        if checksum_calculated.as_slice() != checksum {
            anyhow::bail!(
                "invalid checksum. header={:?}, calculated={:?}",
                checksum,
                checksum_calculated
            );
        }

        let remaining = reader.remaining()?;
        if remaining != 0 {
            anyhow::bail!("got {} bytes of trailing garbage", remaining);
        }

        Ok(Self {
            firmware_version,
            images,
        })
    }
}

/// We need different state for different upload kinds. This has 'em all.
#[derive(Debug)]
pub(super) enum FirmwareUploadKind {
    Fota {
        /// The full firmware container image.
        #[debug(ignore)]
        data: Vec<u8>,
        /// Parsed information about the container in `data`.
        container: FirmwareContainer,
        /// The index of the image that's currently being uploaded.
        ///
        /// This refers to `container.images`.
        image: usize,
    },
    Data {
        /// Full DDL image.
        ///
        /// We don't know or care about it's contents. We simply upload it.
        #[debug(ignore)]
        data: Vec<u8>,
        /// Slot to upload `image` to.
        slot: u32,
    },
}

impl FirmwareUploadKind {
    /// Return the full image data that currently needs to be uploaded.
    ///
    /// This works for all upload kinds but will return an error when there's
    /// no images left to upload.
    pub fn data(&self) -> Result<&[u8], Error> {
        match self {
            FirmwareUploadKind::Fota {
                data,
                container,
                image,
            } => {
                let image = container
                    .images
                    .get(*image)
                    .with_context(|| format!("no image with index `{image}`"))?;

                data.get(image.data_range.clone())
                    .context("BUG: image data doesn't contain `data_range`")
            }
            FirmwareUploadKind::Data { data, .. } => Ok(data),
        }
    }
}

/// Generic upload state for all kinds.
#[derive(Debug)]
pub(super) struct FirmwareUploadState {
    /// Chunk size as reported by the firmware.
    ///
    /// If this is 0, the upload of the current image will be skipped and we'll
    /// immediately move to the next one.
    chunk_size: usize,
    /// Index of the current image to be uploaded.
    index: usize,
    /// Number of attempts per chunk.
    num_attempts: usize,
    /// Number of attempts left for the current chunk.
    attempts_left: usize,
    /// Number of times current chunk was blocked by the application.
    blocked_count: usize,
    /// Kind-specific upload state.
    pub(super) kind: FirmwareUploadKind,
    pub(super) next_attempt: Option<std::time::Instant>,

    pub(super) started: std::time::Instant,
}

/// A chunk that can be uploaded to the device.
pub(super) struct Chunk<'a> {
    /// Offset on the device that this will be uploaded to.
    pub offset: usize,
    /// The data to upload to `offset`.
    pub data: &'a [u8],
    /// [true] if this is the last chunk.
    pub is_last: bool,
}

impl FirmwareUploadState {
    pub fn new(chunk_size: usize, kind: FirmwareUploadKind) -> Self {
        Self {
            chunk_size,
            index: 0,
            num_attempts: 10,
            attempts_left: 10,
            blocked_count: 0,
            kind,
            next_attempt: None,
            started: std::time::Instant::now(),
        }
    }

    /// Return the next chunk that's supposed to be uploaded.
    ///
    /// Returns [None] if there's nothing left to upload.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk<'_>>, Error> {
        if self.attempts_left == 0 {
            anyhow::bail!("ran out of attempts");
        }

        // this is a special case that we use to allow the init code to skip
        // uploading the whole file because the firmware reported that it knows
        // the contents already.
        if self.chunk_size == 0 {
            return Ok(None);
        }

        let offset = self.index * self.chunk_size;

        let mut chunks = self
            .kind
            .data()
            .context("can't get data")?
            .chunks(self.chunk_size);
        let num_chunks = chunks.len();

        Ok(match chunks.nth(self.index) {
            Some(chunk) => {
                tracing::debug!(
                    attempts_left = self.attempts_left,
                    "[{}/{}] sending `{:?}` chunk of size {} to offset {}",
                    self.index + 1,
                    num_chunks,
                    self.kind,
                    chunk.len(),
                    offset,
                );

                self.attempts_left = self
                    .attempts_left
                    .checked_sub(1)
                    .context("attempts underflowed")?;

                Some(Chunk {
                    offset,
                    data: chunk,
                    is_last: chunks.next().is_none(),
                })
            }
            None => None,
        })
    }

    /// Confirm that the chunk returned by `next_chunk` was uploaded successfully.
    ///
    /// This will move to the next image, if any.
    pub fn confirm_receival(&mut self) -> Result<(), Error> {
        self.index = self.index.checked_add(1).context("index overflowed")?;

        // we'll start a new chunk with a fresh number of attempts
        self.attempts_left = self.num_attempts;
        self.blocked_count = 0;

        Ok(())
    }

    /// Set the current image to upload.
    ///
    /// Usually this is not needed because it overrides the state we have.
    /// There are cases where the device tells us to do that though because we
    /// missed a confirmation and are trying to upload data which the device
    /// has received already.
    pub fn set_index(&mut self, index: usize) {
        self.index = index;

        // we don't know if it was successful or not. Fact is that we want to
        // jump to another index - whatever the reason may be.
        // So let's rust reset attempts to start fresh.
        self.attempts_left = self.num_attempts;
        self.blocked_count = 0;
    }

    pub fn kind(&self) -> &FirmwareUploadKind {
        &self.kind
    }

    pub fn wait_for(&mut self, duration: std::time::Duration) {
        self.next_attempt = Some(std::time::Instant::now().checked_add(duration).unwrap());
    }
}

/// Error that represents all possible upload failures.
///
/// Callers can use this to send different events depending on the cause to
/// provide more information to their caller.
#[derive(Debug, thiserror::Error)]
pub(super) enum FirmwareUploadError {
    #[error("offset too big")]
    OffsetTooBig,
    #[error("chunk too big")]
    ChunkTooBig,
    #[error("upload failed: {0:#?}")]
    UploadFailed(anyhow::Error),
    #[error("invalid response: {0:#?}")]
    InvalidResponse(anyhow::Error),
    #[error("firmware reported error: {0}")]
    Lsdl(u32),
    #[error("aborted")]
    Aborted,
    #[error("blocked by device")]
    BlockedByDevice,
    #[error("BUG: missing state")]
    MissingState,
    #[error("BUG: missing expected_offset")]
    MissingExpectedOffset,
}

/// Data download state that will be stored in [Device](super::Device).
///
/// This is mainly stored so we can provide it via IPC.
#[derive(Default)]
pub(super) struct DataDownload {
    pub status: lwm2m::TimedData<lwm2m::DataDownloadStatus>,
    pub slot: lwm2m::TimedData<i64>,
    pub checksum: lwm2m::TimedData<i64>,
    pub content_tag: lwm2m::TimedData<i64>,
}

impl crate::device::Device {
    /// Upload next chunk no matter the upload kind.
    pub(super) async fn upload_next_chunk(&mut self) -> Result<bool, FirmwareUploadError> {
        let chunk = match self
            .upload_state
            .as_mut()
            .ok_or(FirmwareUploadError::MissingState)?
            .next_chunk()
            .map_err(FirmwareUploadError::UploadFailed)?
        {
            Some(chunk) => chunk,
            None => {
                // apparently, there's no chunks left
                return Ok(true);
            }
        };

        let offset: u32 = chunk
            .offset
            .try_into()
            .map_err(|_| FirmwareUploadError::OffsetTooBig)?;
        let chunk_len: u32 = chunk
            .data
            .len()
            .try_into()
            .map_err(|_| FirmwareUploadError::ChunkTooBig)?;
        let chunk_data = hex::encode(chunk.data);
        let is_last = chunk.is_last;

        let gotosleep = Some(crate::current_config().gotosleep_duration_fota);
        let mut response = match self.send_firmware_data(offset, chunk_data, gotosleep).await {
            Err(e) => {
                tracing::debug!("Chunk upload failed: `{:?}`", e);

                let upload_state = self
                    .upload_state
                    .as_mut()
                    .ok_or(FirmwareUploadError::MissingState)?;

                // this throttles the upload speed to prevent interference
                let wait = crate::current_config().fota_min_wait.saturating_add(
                    std::time::Duration::from_millis(rand::thread_rng().gen_range(0..1000)),
                );
                upload_state.wait_for(wait);

                // we ran out of attempt and the FOTA will fail during
                // `next_chunk` on the next call to this function. We set the
                // online state to offline even though there might be race
                // conditions where this is not true because it improves
                // the user-experience with the way the app works right now.
                if upload_state.attempts_left == 0 {
                    self.set_online(false).await;
                }

                // if we run out of attempts, the `next_chunk` function will
                // have failed already
                return Ok(false);
            }
            Ok(v) => {
                // this throttles the upload speed to prevent interference
                let wait = crate::current_config().fota_min_wait.saturating_add(
                    std::time::Duration::from_millis(rand::thread_rng().gen_range(0..1000)),
                );
                self.upload_state
                    .as_mut()
                    .ok_or(FirmwareUploadError::MissingState)?
                    .wait_for(wait);
                v
            }
        };
        let report = lsdl::get_response!(response, firmware_update, firmware_report)
            .map_err(FirmwareUploadError::InvalidResponse)?;

        match report.status {
            lsdl::FIRMWARE_UPDATE_STATUS_OK => {
                self.upload_state
                    .as_mut()
                    .ok_or(FirmwareUploadError::MissingState)?
                    .confirm_receival()
                    .map_err(FirmwareUploadError::UploadFailed)?;

                Ok(is_last)
            }

            lsdl::FIRMWARE_UPDATE_STATUS_NOT_OK => {
                let expected_offset = report
                    .expected_offset
                    .ok_or(FirmwareUploadError::MissingExpectedOffset)?;

                let upload_finished = (offset + chunk_len) == expected_offset;

                if is_last && upload_finished {
                    self.upload_state
                        .as_mut()
                        .ok_or(FirmwareUploadError::MissingState)?
                        .confirm_receival()
                        .map_err(FirmwareUploadError::UploadFailed)?;

                    Ok(true)
                } else {
                    Err(FirmwareUploadError::Lsdl(report.status))
                }
            }

            // This happens when we received the ACK to the outgoing message
            // but we didn't see the response to the request.
            // We will have retried as part of our usual request logic and all
            // of these retries will return this wrong offset error because the
            // firmware expects the next chunk already.
            // In theory we could just assume that we just missed a previous
            // confirmation and increment the index by 1. To be more resilient
            // against unknown bugs or firmware features we allow jumping to
            // any offset though. Unlike `upload.py` we only allow jumping to
            // chunk boundaries though because that makes our current code
            // easier.
            lsdl::FIRMWARE_UPDATE_WRONG_OFFSET => {
                let upload_state = self
                    .upload_state
                    .as_mut()
                    .ok_or(FirmwareUploadError::MissingState)?;

                let offset = report
                    .expected_offset
                    .context("firmware reported `wrong offset` but didn't provide any offset")
                    .map_err(FirmwareUploadError::InvalidResponse)?;
                let offset: usize = offset
                    .try_into()
                    .with_context(|| format!("offset `{offset}` doesn't fit into usize"))
                    .map_err(FirmwareUploadError::InvalidResponse)?;

                // The upload jumped to the end of the file. This can happen if
                // we missed the confirmation for the last chunk.
                if upload_state
                    .kind
                    .data()
                    .map(|data| data.len() == offset)
                    .unwrap_or(false)
                {
                    return Ok(true);
                }

                if offset.checked_rem(upload_state.chunk_size) != Some(0) {
                    return Err(FirmwareUploadError::InvalidResponse(anyhow!(
                        "firmware reported offset `{}` which is not a multiple of the chunk size",
                        offset
                    )));
                }
                let index = offset
                    .checked_div(upload_state.chunk_size)
                    .with_context(|| format!("BUG: offset `{offset}` is 0?"))
                    .map_err(FirmwareUploadError::InvalidResponse)?;

                tracing::info!("Upload index jumped to `{}`", index);
                upload_state.set_index(index);

                Ok(false)
            }

            lsdl::FIRMWARE_UPDATE_BLOCKED_BY_APPLICATION => {
                let upload_state = self
                    .upload_state
                    .as_mut()
                    .ok_or(FirmwareUploadError::MissingState)?;

                upload_state.blocked_count += 1;
                if upload_state.blocked_count >= 3 {
                    // stop fota once a chunk was declined 3 times
                    Err(FirmwareUploadError::BlockedByDevice)
                } else {
                    // we didn't confirm yet so we'll retry
                    Ok(false)
                }
            }

            _ => Err(FirmwareUploadError::Lsdl(report.status)),
        }
    }

    /// Called when the current upload has finished.
    ///
    /// This may cause another upload to be started if there was more than one
    /// image in the container.
    #[tracing::instrument(name = "fota", skip_all)]
    async fn on_fota_upload_finished(
        &mut self,
        container: FirmwareContainer,
        data: Vec<u8>,
        image: usize,
        result: Result<(), FirmwareUploadError>,
        elapsed: std::time::Duration,
    ) -> Result<(), Error> {
        match &result {
            Ok(_) => {
                tracing::info!(
                    remote = true,
                    metric_name = "firmware_upload",
                    metric_value = true,
                    duration = elapsed.as_secs_f64(),
                    "[{}/{}] Firmware upload succeeded",
                    image + 1,
                    container.images.len(),
                );
            }
            Err(FirmwareUploadError::BlockedByDevice) => {
                tracing::info!(
                    remote = true,
                    duration = elapsed.as_secs_f64(),
                    "[{}/{}] Firmware upload blocked by device",
                    image + 1,
                    container.images.len(),
                );
            }
            Err(e) => {
                tracing::warn!(
                    metric_name = "firmware_upload",
                    metric_value = false,
                    duration = elapsed.as_secs_f64(),
                    "[{}/{}] Firmware upload failed error={:?}",
                    image + 1,
                    container.images.len(),
                    e
                );
            }
        }

        let (state, result) = if let Err(e) = result {
            let result = match e {
                FirmwareUploadError::Aborted
                | FirmwareUploadError::BlockedByDevice
                | FirmwareUploadError::OffsetTooBig
                | FirmwareUploadError::ChunkTooBig
                | FirmwareUploadError::InvalidResponse(_)
                | FirmwareUploadError::MissingState
                | FirmwareUploadError::MissingExpectedOffset
                | FirmwareUploadError::UploadFailed(_) => {
                    // single allowed result code that comes close to our error state
                    lwm2m::FirmwareUpdateResult::ConnectionLost
                }

                // match LB states to LWM2M where possible
                FirmwareUploadError::Lsdl(status) => match status {
                    lsdl::FIRMWARE_UPDATE_TOO_BIG => lwm2m::FirmwareUpdateResult::NoFlash,
                    lsdl::FIRMWARE_UPDATE_CHECKSUM_ERR => {
                        lwm2m::FirmwareUpdateResult::IntegrityCheckFail
                    }

                    _ => lwm2m::FirmwareUpdateResult::ConnectionLost,
                },
            };

            (lwm2m::FirmwareUpdateState::Idle, result)
        } else {
            // SG-20459: should we already switch to DownloadComplete here?
            let mut status = (
                lwm2m::FirmwareUpdateState::DownloadComplete,
                lwm2m::FirmwareUpdateResult::Initial,
            );

            if container.images.len() > image + 1 {
                match self.init_firmware_image(data, container, image + 1).await {
                    Err(e) => {
                        tracing::error!("Failed to init image `{}`: {:#?}", image + 1, e);
                        status = (
                            lwm2m::FirmwareUpdateState::Idle,
                            lwm2m::FirmwareUpdateResult::IntegrityCheckFail,
                        );
                    }
                    Ok(()) => return Ok(()),
                }
            }

            tracing::info!("Firmware upload completed successfully");
            status
        };

        self.set_and_publish_firmware_update_status(state, result)
            .await
            .context("failed to signal firmware update status change")
    }

    /// The Data download/upload has finished.
    ///
    /// This will simply publish the result via IPC.
    #[tracing::instrument(name = "ddl", skip_all)]
    pub(super) async fn on_data_upload_finished(
        &mut self,
        result: Result<(), FirmwareUploadError>,
        slot: u32,
        elapsed: std::time::Duration,
    ) -> Result<(), Error> {
        if let Err(e) = &result {
            tracing::warn!(
                metric_name = "data_upload",
                metric_value = false,
                duration = elapsed.as_secs_f64(),
                slot,
                "Data upload failed error={:?}",
                e
            );
        } else {
            tracing::info!(
                remote = true,
                metric_name = "data_upload",
                metric_value = true,
                duration = elapsed.as_secs_f64(),
                slot,
                "Data upload succeeded",
            );
        }

        if result.is_err() {
            // the generic code logged it already
            self.set_and_publish_data_download_status(lwm2m::DataDownloadStatus::UploadFailed)
                .await?;
            return Ok(());
        }

        self.set_and_publish_data_download_status(lwm2m::DataDownloadStatus::Activating)
            .await?;

        // data upload runs on firmware with stack 1.5.3 or higher - no need for long timeout
        match self
            .firmware_update_start(false, Some(crate::device::GOTO_SLEEP_IMMEDIATELY))
            .await
        {
            Err(e) => {
                tracing::error!("Failed to send firmware-update-start command: {:?}", e);
                self.set_and_publish_data_download_status(
                    lwm2m::DataDownloadStatus::ActivationFailed,
                )
                .await?;
            }
            Ok(lsdl::FIRMWARE_UPDATE_STATUS_OK) => (),
            Ok(result) => {
                tracing::error!(result, "Firmware-update-start failed");
                self.set_and_publish_data_download_status(
                    lwm2m::DataDownloadStatus::ActivationFailed,
                )
                .await?;
            }
        }

        tracing::info!(slot, "Data activated");

        self.set_and_publish_data_download_status(lwm2m::DataDownloadStatus::Activated)
            .await?;

        Ok(())
    }

    /// An upload has failed or succeeded.
    pub(super) async fn on_upload_finished(
        &mut self,
        kind: FirmwareUploadKind,
        result: Result<(), FirmwareUploadError>,
        elapsed: std::time::Duration,
    ) {
        let res = match kind {
            FirmwareUploadKind::Fota {
                container,
                data,
                image,
            } => {
                self.on_fota_upload_finished(container, data, image, result, elapsed)
                    .await
            }
            FirmwareUploadKind::Data { slot, .. } => {
                self.on_data_upload_finished(result, slot, elapsed).await
            }
        };

        if let Err(e) = res {
            tracing::error!("Upload finished callback failed: {:?}", e);
        }
    }

    pub(super) async fn abort_upload(&mut self) {
        if let Some(state) = self.upload_state.take() {
            self.on_upload_finished(
                state.kind,
                Err(FirmwareUploadError::Aborted),
                state.started.elapsed(),
            )
            .await;
        }
    }

    /// Prepares `container` to be uploaded to the device.
    ///
    /// This will both talk to the device and start the upload internally so
    /// the first chunk will be uploaded on the next call to
    /// [try_continue_fota](Self::try_continue_fota). That call is done by a
    /// separate task.
    pub(super) async fn init_firmware_image(
        &mut self,
        data: Vec<u8>,
        container: FirmwareContainer,
        index: usize,
    ) -> Result<(), Error> {
        let image = container
            .images
            .get(index)
            .with_context(|| format!("image `{index}` doesn't exist"))?;

        if image.slot > 1 {
            // NOTE: The cloud doesn't give us a content-tag, yet we have to
            //       set one. In addition to that it might be useful to know
            //       which image the upload corresponds to. Since we have 32
            //       bits of content-tag we can simply use a crc32 of the image
            //       data.
            let crc32 = CONTENTTAG_CRC.checksum(
                data.get(image.data_range.clone())
                    .context("short read for data")?,
            );

            self.set_data_download_int(image.slot, crc32).await?;
        }

        let go_to_sleep = self.filter_go_to_sleep(Some(crate::current_config().gotosleep_duration));
        let report = self
            .request_firmware_init(image.len, image.crc, image.slot, go_to_sleep)
            .await
            .context("firmware init failed")?;
        if report.status != lsdl::FIRMWARE_UPDATE_STATUS_OK {
            match report.status {
                lsdl::FIRMWARE_UPDATE_CHECKSUM_ERR => {
                    tracing::warn!("Checksum error at firmware_init: likely a defective flash");
                }
                lsdl::FIRMWARE_UPDATE_BLOCKED_BY_APPLICATION => {
                    tracing::info!("Device blocks firmware_init: likely caused by low battery");
                }
                _ => {
                    tracing::error!("Unexpected error at firmware_init: {}", report.status);
                }
            }

            anyhow::bail!(
                "firmware update initialization failed, device responded with status code {}",
                report.status
            );
        }

        let imagelen_u32 = image
            .len
            .try_into()
            .context("image len doesn't fit into u32")?;
        if report.expected_offset == Some(imagelen_u32) {
            tracing::info!("Firmware was uploaded already, skip");

            self.set_upload_state(FirmwareUploadState::new(
                0,
                FirmwareUploadKind::Fota {
                    data,
                    container,
                    image: index,
                },
            ));

            return Ok(());
        }

        tracing::info!(
            "Starting firmware upload with image size {} bytes and checksum {:x?}",
            image.len,
            image.crc
        );

        // request info to obtain expected image and chunk size
        let gotosleep = Some(crate::current_config().gotosleep_duration);
        let firmware_info = self.request_firmware_info(gotosleep).await?;
        if image.len
            != usize::try_from(firmware_info.size)
                .context("failed to convert firmware size to 'usize'")?
        {
            anyhow::bail!("expected image size not equal to actual image size");
        }

        let chunk_size = usize::try_from(firmware_info.chunk_size)
            .context("failed to convert chunk_size to 'usize'")?;

        self.set_upload_state(FirmwareUploadState::new(
            chunk_size,
            FirmwareUploadKind::Fota {
                data,
                container,
                image: index,
            },
        ));

        Ok(())
    }

    pub(super) async fn set_and_publish_firmware_update_status(
        &mut self,
        state: lwm2m::FirmwareUpdateState,
        result: lwm2m::FirmwareUpdateResult,
    ) -> Result<(), Error> {
        if !self.description.included {
            anyhow::bail!("device not included");
        }

        if state == self.persistent.firmware_update_state
            && result == self.persistent.firmware_update_result
        {
            return Ok(());
        }

        if self.persistent.firmware_update_state != state
            && matches!(
                state,
                FirmwareUpdateState::Idle | FirmwareUpdateState::Updating
            )
        {
            // The pkg_version doesn't make any sense in those states since
            // the data is probably lost anyway.
            // NOTE: we're not using `clear()` in the hope that the
            //       assignment will reduce dynamic memory usage.
            if !self.persistent.firmware_update_pkg_version.is_empty() {
                self.persistent.firmware_update_pkg_version = "".to_string();
            }
        }

        self.persistent.firmware_update_state = state;
        self.persistent.firmware_update_result = result;
        self.save_persistent_state().await?;

        self.publish_resource_instances(
            lwm2m::ObjectType::FirmwareUpdate,
            0,
            &[
                (lwm2m::objects::FIRMWARE_UPDATE_STATE, 0),
                (lwm2m::objects::FIRMWARE_UPDATE_UPDATE_RESULT, 0),
                (lwm2m::objects::FIRMWARE_UPDATE_PKG_VERSION, 0),
            ],
        )
        .await?;

        Ok(())
    }

    async fn set_and_publish_data_download_status(
        &mut self,
        status: lwm2m::DataDownloadStatus,
    ) -> Result<(), Error> {
        if status == self.data_download.status.0 {
            return Ok(());
        }

        self.data_download.status = (status, Some(std::time::SystemTime::now()));

        self.publish_resource_instance(
            lwm2m::ObjectType::DataDownload,
            0,
            lwm2m::objects::DATA_DOWNLOAD_STATUS,
            0,
        )
        .await?;

        Ok(())
    }

    /// Sets data_download_int value on this device.
    ///
    /// We can use this to tell the device that the upload that follows is not
    /// a main firmware image.
    pub(super) async fn set_data_download_int(
        &mut self,
        slot: u32,
        content_tag: u32,
    ) -> Result<(), Error> {
        let mut data_download_int = Vec::with_capacity(8);
        data_download_int.extend_from_slice(&slot.to_be_bytes());
        data_download_int.extend_from_slice(&content_tag.to_be_bytes());

        self.set_lemonbeat_value(
            "data_download_int",
            lwm2m::Value::new(lwm2m::ValueData::Opaque(data_download_int), None),
        )
        .await
        .context("failed to set data_download_int")
    }

    /// Includes device after FOTA-reboot and/or sends out IPC events for it.
    pub(super) async fn fota_on_device_description(&mut self) -> Result<(), Error> {
        // a firmware update failed and the device just rebooted
        if self.description.included {
            tracing::info!("Device booted after update as included, report update as failed");

            // The bootloader rejected the update and booted with the old
            // firmware. All of our state is probably still correct and we
            // just updated the device description.
            // Meaning there's no need to clean anything up and we can
            // simply report failure.

            self.set_and_publish_firmware_update_status(
                lwm2m::FirmwareUpdateState::Idle,
                lwm2m::FirmwareUpdateResult::UpdateFailed,
            )
            .await?;
        } else {
            tracing::info!("Device booted after update as un-included, re-include");

            match self.include_inner().await {
                Ok(()) => {
                    tracing::info!(
                        remote = true,
                        metric_name = "re_inclusion",
                        metric_value = true,
                        "Re-inclusion succeeded"
                    );

                    self.set_and_publish_firmware_update_status(
                        lwm2m::FirmwareUpdateState::Idle,
                        lwm2m::FirmwareUpdateResult::Success,
                    )
                    .await?;

                    if let Err(e) = self.publish_endpoint().await {
                        tracing::error!("Failed to publish endpoint: {:?}", e);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        metric_name = "re_inclusion",
                        metric_value = false,
                        "Re-inclusion failed, removing device, cause: {:?}",
                        e
                    );

                    self.forget("failed re-inclusion").await;
                }
            }
        }

        Ok(())
    }

    pub fn set_upload_state(&mut self, state: FirmwareUploadState) {
        self.upload_state = Some(state);
        self.sleep_fotatask.abort();
    }
}
