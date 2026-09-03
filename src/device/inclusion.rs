// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Everything related to inclusion and exclusion.
//!
//! There's no code for exclusion though because that' just a simple command
//! and thus implemented by [lemonbeat_requests](super::lemonbeat_requests).
//! Publishing anything via IPC is **NOT** the job of this module.

use crate::device::ValueDescriptionList as _;
use crate::traits::RangeEx as _;
use crate::Error;
use anyhow::anyhow;
use anyhow::Context as _;
use std::convert::TryFrom as _;
use std::convert::TryInto as _;

impl crate::device::Device {
    /// Runs inclusion and post-inclusion
    ///
    /// On errors (and exceeded retry-limits) it will just return.
    /// It is the responsibility of the caller to announce and handle that
    /// failure properly.
    /// The code was separated this way because it makes it easier for the
    /// caller to do said error checking.
    pub(crate) async fn include_inner(&mut self) -> Result<(), Error> {
        tracing::info!("Start inclusion");

        let mac_address = self
            .description
            .mac_address()
            .context("failed to get mac address")?;

        // Ensure radio module will clear the devices MAC counter.
        // Devices with stack 1.2.0 require this to happen as they clear their
        // own counter at each factory reset. So a re-inclusion would fail as
        // the gateway radio module would start dropping all incoming encrypted
        // packets.
        self.radiomodule
            .reset_device_nonce(&mac_address)
            .await
            .context("failed to reset device nonce")?;

        let inclusion_message = self.network.inclusion_message()?;

        self.description = self
            .send_inclusion_message(&inclusion_message, Some(crate::device::GOTO_SLEEP_DURATION))
            .await
            .context("can't send inclusion message")?;

        if !self.description.included {
            anyhow::bail!("supposedly successful inclusion, but description.included=false");
        }

        // Reset all of our state so we can start fresh.
        self.persistent =
            crate::storage::PersistentState::new(crate::storage::ConnectionStatus::Online);

        tracing::debug!("Device reports inclusion=1");

        let res = self.post_inclusion().await.context("post-inclusion failed");

        if let Err(e) = res {
            // not necessary, but let's clear some state
            self.value_descriptions.clear();
            self.values.clear();

            let res = self.send_exclusion_message().await;
            tracing::warn!(
                metric_name = "post_inclusion",
                metric_value = false,
                "Post-inclusion failed, exclusion result: {:?}",
                res
            );

            // Even though we don't know if this is true we'll just assume
            // it is because it makes error handling easier for our callers
            // Additionally, most of our code uses this field to check if
            // inclusion+post_inclusion are done to decide if the device is
            // ready to be used.
            self.description.included = false;

            return Err(e);
        }

        self.sleep_utctask.abort();

        tracing::info!(
            remote = true,
            metric_name = "post_inclusion",
            metric_value = true,
            "Post-inclusion succeeded"
        );

        Ok(())
    }

    pub(crate) async fn fetch_values_and_descriptions(
        &mut self,
    ) -> Result<
        (
            Vec<crate::storage::ValueDescription>,
            Vec<crate::storage::Value>,
        ),
        Error,
    > {
        let memory_information = self
            .request_memory_information(Some(crate::device::GOTO_SLEEP_DURATION))
            .await
            .context("can't request memory information")?;
        tracing::debug!("Memory information: {:#?}", memory_information);

        let meminfo_value = memory_information
            .iter()
            .find(|info| matches!(info.id(), Some(lsdl::MemoryId::Value)))
            .ok_or_else(|| anyhow!("can't find `value` memory info"))?;
        let num = meminfo_value.num_allocated();
        let num_usize =
            usize::try_from(num).map_err(|_| anyhow!("can't convert `{}` to usize", num))?;

        let mut value_descriptions = Vec::with_capacity(
            num.try_into()
                .context("can't convert number of values to usize")?,
        );
        // Before BNW the approach was different: request all value descriptors (or values) and
        // check response if all got retrieved. If not, switch to requesting the missing ones.
        // This resulted in much fewer (and smaller) requests, but lead to very big response
        // packets. Also not clear, if truncated responses could be deserialized in all cases.
        for range in (1..num + 1).chunks(10) {
            value_descriptions.append(
                &mut self
                    .request_value_descriptions(range, Some(crate::device::GOTO_SLEEP_DURATION))
                    .await
                    .context("can't request value descriptions")?,
            );
        }
        tracing::debug!("Value descriptions: {:#?}", value_descriptions);

        if value_descriptions.len() != num_usize {
            anyhow::bail!(
                "got {} value-descriptions, expected {}",
                value_descriptions.len(),
                num_usize
            );
        }

        // Requesting all values at once (and not chunking our requests like above): this
        // leads to single small request and the answer of all our current devices fit into
        // a single MAC frame (IC24 being the most demanding with 709 bytes on the wire).
        let mut values = Vec::with_capacity(value_descriptions.len());
        values.append(
            &mut self
                .request_all_values(Some(crate::device::GOTO_SLEEP_DURATION))
                .await
                .context("can't request values")?,
        );

        // set all value timestamps to the current time
        // as documented in the function `update_value`, we can't rely on those
        // timestamps meaning we have to initialize them here as well.
        let gateway_timestamp = crate::utils::gateway_timestamp();
        for value in &mut values {
            value.timestamp = gateway_timestamp;
        }

        value_descriptions
            .validate_values(&values)
            .context("failed to validate values")?;

        Ok((value_descriptions, values))
    }

    /// Run post inclusion steps like reading the value-description
    ///
    /// NOTE: This will not publish includable-device updates as it's only
    /// intended to be called through the public API for the radio module'
    /// post-inclusion.
    pub(super) async fn post_inclusion(&mut self) -> Result<(), Error> {
        tracing::info!("Start post-inclusion");

        if matches!(self.description.radio_mode, lsdl::RadioMode::WakeOnRadio) {
            let wakeup_channel =
                crate::comm::WakeupChannel::allocate(&self.description.channel_map())
                    .context("can't allocate wakeup channel")?;

            self.set_wakeup_channel(
                wakeup_channel.channel(),
                Some(crate::device::GOTO_SLEEP_DURATION),
            )
            .await
            .context("can't change wakeup channel")?;

            let description = self
                .request_device_description(Some(crate::device::GOTO_SLEEP_DURATION))
                .await
                .context("can't request device description")?;
            if description.wakeup_channel.channel() != wakeup_channel.channel() {
                anyhow::bail!(
                    "device reported wakeup channel {}, expected {}",
                    description.wakeup_channel.channel(),
                    wakeup_channel.channel()
                );
            }

            self.description.wakeup_channel = wakeup_channel;
        }

        let (value_descriptions, values) = self.fetch_values_and_descriptions().await?;

        // Dropping all value updates received until now. This can cause issues for value updates
        // that were sent our right after the response to request_all_values and that were queued
        // before remove_pending_data.
        self.remove_pending_data();

        tracing::debug!("Values: {:#?}", values);

        // XXX: we just sent the last request (in case this is not the radio module)
        //      but we didn't make the device go to sleep immediately.
        //      This shouldn't matter though because:
        //      - device inclusion is a rare thing to happen
        //      - the end of the inclusion should trigger setting the UTC
        //        offset which will put the device to sleep at the end of that
        //        process

        self.value_descriptions = value_descriptions;
        self.values = values;

        self.save_persistent_state()
            .await
            .context("can't save persistent state")?;
        self.save_device_description()
            .await
            .context("can't save device description")?;
        self.save_value_descriptions()
            .await
            .context("can't save value descriptions")?;
        self.save_values().await.context("can't save values")?;

        Ok(())
    }

    fn remove_pending_data(&mut self) {
        let num_removed = self.receiver.rx.remove_matching(|(_instant, request)| {
            let remove = matches!(
                request,
                crate::device::DeviceRequest::update_values { .. }
                    | crate::device::DeviceRequest::received_device_description { .. }
            );

            if remove {
                tracing::debug!("Remove update: {:#?}", request);
            }

            remove
        });
        tracing::info!(
            "Removed {} value updates / device descriptions",
            num_removed
        );
    }
}
