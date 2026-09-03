// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! All the lemonbeat request we need to send
//!
//! This includes:
//! - (de-)serialisation from/into more usable types
//! - Request-specific ways to wait for an answer and verify success
//! - Messages(aka requests that expect no answer). This might break the naming
//!   scheme of this module, but maybe it doesn't because we build an API
//!   around them that looks like a request. Many messages have side-channels
//!   that we can wait on.
//!
//! Function naming conventions:
//! - `set_*`: Sends a request that changes state on the device.
//! - `request_*`: Sends a request that returns state from the device.
//!   We didn't use rusts convention of not using any prefix for such
//!   "getter"-functions to make it clear that those functions actually
//!   communicate with the device and they don't just return internal state.
//! - `send_*`: send a message that triggers a process on the device. There'll
//!   be no answer and we're not simply changing a single state value on the
//!   device.

use crate::storage;
use crate::Error;
use anyhow::anyhow;
use anyhow::Context as _;
use num_traits::cast::ToPrimitive as _;
use std::convert::TryInto as _;

/// Extract properties from [lsdl::xsd::device_description::deviceTypeInner::device_description_report].
///
/// This is done without copies by moving the internal [Vec] pointer.
pub fn device_description_to_properties(
    mut description: lsdl::xsd::device_description::networkType,
) -> Result<Vec<lsdl::Property>, Error> {
    let description =
        lsdl::get_response!(description, device_description, device_description_report)?;
    let mut properties = vec![];
    std::mem::swap(&mut description.inner, &mut properties);
    Ok(properties)
}

impl crate::device::Device {
    pub(super) async fn send_inclusion_message(
        &mut self,
        inclusion_message: &[u8],
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<Box<storage::DeviceDescription>, Error> {
        let go_to_sleep = self.filter_go_to_sleep(go_to_sleep);

        let network = lsdl::device_message!(
            network_management,
            None,
            go_to_sleep,
            vec![
                lsdl::xsd::network_management::deviceTypeInner::network_include(
                    lsdl::xsd::network_management::network_include {
                        address_size: None,
                        inclusion_count: None,
                        value: hex::encode(inclusion_message),
                    },
                )
            ]
        )?;

        // Network-inclusions don't send an answer to the sender. Instead,
        // they send a device-description with included=true to the gateway.
        async fn wait_for_answer(
            device: &mut crate::device::Device,
            now: &mut std::time::Instant,
        ) -> Result<Box<storage::DeviceDescription>, Error> {
            Ok(device
                .receiver
                .remove_one_received_device_description_request(|instant, description| {
                    instant >= *now && description.included
                })
                .await
                .ok_or_else(|| anyhow!("queue error"))?
                .0)
        }

        let mut now = std::time::Instant::now();
        let result = self
            .send_message(false, &network, wait_for_answer, true, &mut now)
            .await;

        match result {
            Ok(Some(v)) => v,
            Ok(None) => {
                log::info!("Inclusion timeout, try to recover reading device description");

                let gotosleep = Some(crate::current_config().gotosleep_duration);
                self.request_device_description(gotosleep)
                    .await
                    .context("failed to request device description")
            }
            Err(e) => {
                // No matter what the error - we give the inclusion process one last chance by
                // actively requesting device description to see if it got included. Maybe the
                // ACK got lost and triggered an ICMP.

                log::info!(
                    "Inclusion error, try to recover reading device description: {:?}",
                    e
                );

                let gotosleep = Some(crate::current_config().gotosleep_duration);
                self.request_device_description(gotosleep)
                    .await
                    .context("failed to request device description")
            }
        }
    }

    /// Sends the actual inclusion message and waits for the confirmation.
    pub(super) async fn send_exclusion_message(&mut self) -> Result<(), Error> {
        // XXX: even though it might make sense to use 0s, testing has shown
        //      that the exclusion of a water control doesn't work with that.
        let go_to_sleep = self.filter_go_to_sleep(Some(crate::current_config().gotosleep_duration));
        let network = lsdl::device_message!(
            device_description,
            None,
            go_to_sleep,
            vec![
                lsdl::xsd::device_description::deviceTypeInner::device_description_set(
                    lsdl::xsd::device_description::deviceDescriptionType {
                        inner: vec![lsdl::Property::new_number(lsdl::PropertyId::Included, 0)?,]
                    },
                )
            ]
        )?;

        /// wait for status message
        ///
        /// Older firmwares don't support this but that's okay for the following reason:
        /// - Due to us waiting for and retrying on ICMP would make sure the
        ///   device gets excluded anyway.
        /// - We'll only support somewhat recent firmware versions
        /// - Our main concern is that we'll still be able to include old
        ///   firmwares, so we can update them. Once that's done everything
        ///   behaves as expected again.
        async fn wait_for_answer(
            device: &mut crate::device::Device,
            now: &mut std::time::Instant,
        ) -> Option<(lsdl::RawStatus,)> {
            device
                .receiver
                .remove_one_handle_status_request(|instant, status| {
                    instant >= *now
                        && matches!(
                            status.code(),
                            Ok(lsdl::StatusCode::System(
                                lsdl::StatusCodeSystem::FactoryResetPending
                            ))
                        )
                })
                .await
        }

        // even though `device_description_set` is a request, testing has shown
        // that we don't receive an answer.
        let mut now = std::time::Instant::now();
        self.send_message(true, &network, wait_for_answer, true, &mut now)
            .await
            .context("can't send exclude")?
            .map_or_else(
                || Err(anyhow!("timed out waiting for answer to exclusion")),
                |opt| {
                    opt.map_or_else(
                        || Err(anyhow!("queue error")),
                        |args| {
                            tracing::debug!("Got exclusion confirmation: {:#?}", args);
                            Ok(())
                        },
                    )
                },
            )
    }

    /// Upload firmware data to the device.
    ///
    /// Using a heap-allocated [String] instead of a [u8]-slice makes this
    /// RPC call more efficient and doesn't require us to bother with lifetimes.
    ///
    /// We return the raw response because the caller (the firmware update task)
    /// needs to send different results via IPC depending on the kind of error.
    /// Summarizing them as an anyhow Error wouldn't allow doing that.
    pub(super) async fn send_firmware_data(
        &mut self,
        offset: u32,
        chunk: String,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<lsdl::xsd::firmware_update::networkType, Error> {
        let go_to_sleep = self.filter_go_to_sleep(go_to_sleep);

        let network = lsdl::device_message!(
            firmware_update,
            None,
            go_to_sleep,
            vec![lsdl::xsd::firmware_update::deviceTypeInner::firmware_data(
                lsdl::xsd::firmware_update::firmwareDataType {
                    offset,
                    inner: vec![chunk],
                }
            )]
        )?;

        // For a 340KB firmware, the last chunk will take ~3 seconds to confirm.
        // It's not known why that is the case but there's a few probable
        // reasons like writing the image to flash or transferring it to the
        // secondary MCU via UART.
        // If we retry while the chunk is still being processed we'll receive
        // status=10 (FOTA blocked by app). We decided to use a timeout of 10s
        // for all chunks to be on the safe side and because we don't think it
        // can cause any issues for us. It might even help to be able to do
        // FOTA in very noisy environments.
        self.send_request_ex(true, network, Some(std::time::Duration::from_secs(10)), 1)
            .await
    }

    /// Retrieve config status.
    ///
    /// We offer the raw variant without parsing the return value because this
    /// is used as a ping-message. For those we want to accept any answer,
    /// no matter if we can parse it or not.
    pub(super) async fn request_config_status_raw(
        &mut self,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<lsdl::xsd::configuration::networkType, Error> {
        let go_to_sleep = self.filter_go_to_sleep(go_to_sleep);

        let network = lsdl::device_message!(
            configuration,
            None,
            go_to_sleep,
            vec![
                lsdl::xsd::configuration::deviceTypeInner::config_status_get(
                    lsdl::xsd::configuration::configStatusGetType {}
                )
            ]
        )?;
        self.send_request(true, network).await
    }

    pub(super) async fn request_config_status(
        &mut self,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<u32, Error> {
        let mut response = self.request_config_status_raw(go_to_sleep).await?;

        lsdl::get_response!(response, configuration, config_status_report)
            .context("invalid configuration response")
            .map(|r| r.status)
    }

    pub(super) async fn request_memory_information(
        &mut self,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<Vec<storage::MemoryInformation>, Error> {
        let go_to_sleep = self.filter_go_to_sleep(go_to_sleep);

        let network = lsdl::device_message!(
            memory_information,
            None,
            go_to_sleep,
            vec![
                lsdl::xsd::memory_information::deviceTypeInner::memory_information_get(
                    lsdl::xsd::memory_information::memoryInformationGetType {},
                )
            ]
        )?;
        let mut response = self
            .send_request(true, network)
            .await
            .context("can't send memory-information request")?;
        let report = lsdl::get_response!(response, memory_information, memory_information_report)
            .context("invalid memory-information response")?;
        Ok(report
            .inner
            .iter()
            .map(storage::MemoryInformation::new)
            .collect())
    }

    pub(super) async fn request_value_descriptions(
        &mut self,
        range: std::ops::Range<u32>,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<Vec<storage::ValueDescription>, Error> {
        let go_to_sleep = self.filter_go_to_sleep(go_to_sleep);

        let network = lsdl::device_message!(
            value_description,
            None,
            go_to_sleep,
            range
                .map(
                    |id| lsdl::xsd::value_description::deviceTypeInner::value_description_get(
                        lsdl::xsd::value_description::valueDescriptionGetType {
                            value_description_id: Some(id),
                        },
                    )
                )
                .collect()
        )?;
        let mut response = self
            .send_request(true, network)
            .await
            .context("can't send value-descriptions request")?;
        let report = lsdl::get_response!(response, value_description, value_description_report)
            .context("can't parse value-descriptions response")?;

        let mut descriptions = Vec::with_capacity(report.inner.len());
        for description in &report.inner {
            descriptions.push(
                storage::ValueDescription::new(description).with_context(|| {
                    format!(
                        "can't parse value description with id `{}`",
                        description.value_id
                    )
                })?,
            );
        }

        Ok(descriptions)
    }

    pub(super) async fn request_all_values(
        &mut self,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<Vec<storage::Value>, Error> {
        let go_to_sleep = self.filter_go_to_sleep(go_to_sleep);

        let network = lsdl::device_message!(
            value,
            None,
            go_to_sleep,
            vec![lsdl::xsd::value::deviceTypeInner::value_get(
                lsdl::xsd::value::valueGetType { value_id: None },
            )]
        )?;
        let mut response = self
            .send_request(true, network)
            .await
            .context("can't send values-request")?;
        let responses =
            lsdl::get_responses!(response, value).context("can't parse values response")?;

        let mut values = Vec::with_capacity(responses.len());
        for response in responses {
            match response {
                lsdl::xsd::value::deviceTypeInner::value_report(report) => {
                    values.push(storage::Value::new(report).context("can't parse value")?)
                }
                _ => anyhow::bail!("got unsupported variant as values response"),
            }
        }

        Ok(values)
    }

    pub(super) async fn request_calendar_timezone(
        &mut self,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<i32, Error> {
        let go_to_sleep = self.filter_go_to_sleep(go_to_sleep);

        let network = lsdl::device_message!(
            calendar,
            None,
            go_to_sleep,
            vec![lsdl::xsd::calendar::deviceTypeInner::calendar_get_timezone(
                lsdl::xsd::calendar::calendarTimezoneGetType {},
            )]
        )?;
        let mut response = self
            .send_request(true, network)
            .await
            .context("can't send calendar-timezone request")?;
        let report = lsdl::get_response!(response, calendar, calendar_report_timezone)
            .context("invalid calendar-timezone response")?;
        Ok(report.offset)
    }

    pub(super) async fn request_device_description(
        &mut self,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<Box<storage::DeviceDescription>, Error> {
        let go_to_sleep = self.filter_go_to_sleep(go_to_sleep);

        let network = lsdl::device_message!(
            device_description,
            None,
            go_to_sleep,
            vec![
                lsdl::xsd::device_description::deviceTypeInner::device_description_get(
                    lsdl::xsd::device_description::deviceDescriptionGetType {},
                )
            ]
        )?;

        let response = self.send_request(true, network).await?;
        let properties = device_description_to_properties(response)
            .context("failed to parse device description")?;

        Ok(Box::new(
            storage::DeviceDescription::new(self.description.address, properties)
                .context("failed to parse radio module device description")?,
        ))
    }

    pub(super) async fn request_firmware_init(
        &mut self,
        size: usize,
        crc: u16,
        firmware_id: u32,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<lsdl::xsd::firmware_update::firmwareReportType, Error> {
        let network = lsdl::device_message!(
            firmware_update,
            None,
            go_to_sleep,
            vec![lsdl::xsd::firmware_update::deviceTypeInner::firmware_init(
                lsdl::xsd::firmware_update::firmwareInitType {
                    size: u32::try_from(size).context("failed to convert firmware size to u32")?,
                    firmware_id,
                    checksum: hex::encode(crc.to_be_bytes())
                }
            )]
        )?;

        if self.description.version_stack_parsed() > crate::utils::create_version(1, 5, 0) {
            // device sends response only after firmware init - request needs a longer timeout
            // (SGSE-843 / SGSE-946)
            let mut response = self
                .send_request_ex(true, network, Some(std::time::Duration::from_secs(10)), 3)
                .await?;

            let report = lsdl::get_response!(response, firmware_update, firmware_report)
                .context("failed to get response to 'init' request")?;

            Ok(report.clone())
        } else {
            let mut response = self.send_request(true, network).await?;

            let report = lsdl::get_response!(response, firmware_update, firmware_report)
                .context("failed to get response to 'init' request")?;

            // device sent response immediately - give it some time to complete firmware init
            tokio::time::sleep(std::time::Duration::from_secs(6)).await;

            Ok(report.clone())
        }
    }

    pub(super) async fn request_firmware_info(
        &mut self,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<lsdl::xsd::firmware_update::firmwareInformationReportType, Error> {
        let go_to_sleep = self.filter_go_to_sleep(go_to_sleep);

        let network = lsdl::device_message!(
            firmware_update,
            None,
            go_to_sleep,
            vec![
                lsdl::xsd::firmware_update::deviceTypeInner::firmware_information_get(
                    lsdl::xsd::firmware_update::firmwareInformationGetType {}
                )
            ]
        )?;
        let mut response = self.send_request(true, network).await?;
        let report = lsdl::get_response!(response, firmware_update, firmware_information_report)?;
        Ok(lsdl::xsd::firmware_update::firmwareInformationReportType {
            size: report.size,
            firmware_id: report.firmware_id,
            chunk_size: report.chunk_size,
            received_size: report.received_size,
        })
    }

    /// Send the lemonbeat `firmware_update_start` command.
    ///
    /// `long_timeout`:
    /// If true uses longer request timeout (and single retry): older stacks
    /// have a 10s timer before update gets confirmed. Should they
    /// receive another request within those 10s the update gets delayed by
    /// another 10s. Issue was discovered with stack 1.2.5 - unclear when it
    /// got fixed. 1.5.2 and later don't need it.
    pub(super) async fn firmware_update_start(
        &mut self,
        long_timeout: bool,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<u32, Error> {
        let go_to_sleep = self.filter_go_to_sleep(go_to_sleep);

        let network = lsdl::device_message!(
            firmware_update,
            None,
            go_to_sleep,
            vec![
                lsdl::xsd::firmware_update::deviceTypeInner::firmware_update_start(
                    lsdl::xsd::firmware_update::firmwareUpdateStartType {}
                )
            ]
        )?;

        tracing::info!(remote = true, "Sending firmware update start");

        let result = if long_timeout {
            let timeout = Some(std::time::Duration::from_secs(15));
            self.send_request_ex(true, network, timeout, 1).await
        } else {
            self.send_request(true, network).await
        };
        let mut response = result.context("failed to send 'flash' request")?;

        let report = lsdl::get_response!(response, firmware_update, firmware_report)
            .context("failed to get response to 'flash' request")?;

        Ok(report.status)
    }

    /// Set a new timezone offset.
    ///
    /// returns an error on any internal issues
    /// returns `Ok(true)` on timeout
    /// returns `Ok(false)` if the request was confirmed by a status message.
    ///
    /// The reason for returning a timeout that way is so the user can handle
    /// this without us having to introduce another thiserror type.
    /// Usually, users should ignore a timeout because there'll be no status
    /// message if the new timezone equals the old one.
    pub(super) async fn set_calendar_timezone(
        &mut self,
        offset: i32,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<bool, Error> {
        let go_to_sleep = self.filter_go_to_sleep(go_to_sleep);

        let network = lsdl::device_message!(
            calendar,
            None,
            go_to_sleep,
            vec![lsdl::xsd::calendar::deviceTypeInner::calendar_set_timezone(
                lsdl::xsd::calendar::calendarTimezoneSetType { offset },
            )]
        )?;

        async fn wait_for_answer(
            device: &mut crate::device::Device,
            now: &mut std::time::Instant,
        ) -> Option<(lsdl::RawStatus,)> {
            device
                .receiver
                .remove_one_handle_status_request(|instant, status| {
                    instant >= *now
                        && matches!(
                            status.code(),
                            Ok(lsdl::StatusCode::Configuration(
                                lsdl::StatusCodeConfiguration::Started
                            ))
                        )
                })
                .await
        }

        // Usually a device will respond with "config started". However, if it missed the previous
        // configuration mode "save" command it remains forever in the started state. So there
        // is no guarantee that we will receive an answer - thus answer_required has to be set to
        // false (SG-19684).
        let mut now = std::time::Instant::now();
        self.send_message(true, &network, wait_for_answer, false, &mut now)
            .await
            .context("can't set calendar timezone")?
            .map_or_else(
                || {
                    tracing::debug!("Timed out waiting for answer to set-timezone");
                    Ok(true)
                },
                |opt| {
                    opt.map_or_else(
                        || Err(anyhow!("queue error")),
                        |args| {
                            tracing::debug!("Got timezone configuration confirmation: {:#?}", args);
                            Ok(false)
                        },
                    )
                },
            )
    }

    pub(super) async fn set_config_mode(
        &mut self,
        mode: lsdl::ConfigurationMode,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<(), Error> {
        let go_to_sleep = self.filter_go_to_sleep(go_to_sleep);

        let mode = mode
            .to_u32()
            .ok_or_else(|| anyhow!("can't convert mode to u32"))?;
        let network = lsdl::device_message!(
            configuration,
            None,
            go_to_sleep,
            vec![lsdl::xsd::configuration::deviceTypeInner::config_mode_set(
                lsdl::xsd::configuration::configModeSetType { mode },
            )]
        )?;

        // This will trigger a timeout on the happy path because we don't
        // expect an answer. This slows down things by a few seconds but this
        // should be okay since we currently don't use this request that often.
        // On ICMP, it'll still retry.
        async fn wait_for_answer(_device: &mut crate::device::Device, _ctx: &mut ()) {
            futures::future::pending::<()>().await;
            unreachable!()
        }

        self.send_message(true, &network, wait_for_answer, false, &mut ())
            .await
            .context("can't set configuration mode")?
            .map_or_else(
                || Ok(()),
                |opt| Err(anyhow!("BUG: receive an answer: {:?}", opt)),
            )
    }

    pub(super) async fn set_wakeup_channel(
        &mut self,
        channel: u64,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<(), Error> {
        let go_to_sleep = self.filter_go_to_sleep(go_to_sleep);
        let network = lsdl::device_message!(
            device_description,
            None,
            go_to_sleep,
            vec![
                lsdl::xsd::device_description::deviceTypeInner::device_description_set(
                    lsdl::xsd::device_description::deviceDescriptionType {
                        inner: vec![lsdl::Property::new_number(
                            lsdl::PropertyId::WakeupChannel,
                            channel
                        )?,]
                    },
                )
            ]
        )?;

        /// we don't receive an answer but still want to get notified about
        /// ICMP. So let's just wait for the timeout.
        /// Within `send_message`, a timeout will not cause a retry.
        async fn wait_for_answer(_device: &mut crate::device::Device, _ctx: &mut ()) {
            futures::future::pending::<()>().await;
        }

        self.send_message(true, &network, wait_for_answer, false, &mut ())
            .await
            .context("can't send exclude")?;

        Ok(())
    }
}
