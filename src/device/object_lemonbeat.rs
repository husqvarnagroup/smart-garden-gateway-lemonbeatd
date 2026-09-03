// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::device::Lwm2mIdentifier as _;
use crate::device::ValueDescriptionList as _;
use crate::device::ValueList as _;
use crate::storage;
use crate::traits::ResultEx as _;
use crate::Error;
use anyhow::anyhow;
use anyhow::Context as _;
use async_trait::async_trait;
use itertools::Itertools as _;
use lsdl::Network as _;

pub struct LemonbeatHandler<'a> {
    device: &'a mut crate::device::Device,
}

impl<'a> LemonbeatHandler<'a> {
    pub(super) fn new(device: &'a mut crate::device::Device) -> Self {
        Self { device }
    }
}

#[async_trait]
impl lwm2m::Object for LemonbeatHandler<'_> {
    fn urn(&self) -> &'static str {
        "urn:oma:lwm2m:x:31000"
    }

    async fn read_resource(
        &self,
        object_instance: usize,
        resource_id: usize,
        resource_instance: usize,
    ) -> Result<lwm2m::Value, lwm2m::Error> {
        if object_instance != 0 {
            return Err(anyhow!("non-zero object instance").into());
        }
        if resource_instance != 0 {
            return Err(anyhow!("non-zero resource instance").into());
        }

        let value_description = self.device.value_descriptions.by_resource_id(resource_id)?;
        let value = self.device.values.by_resource_id(resource_id)?;

        let data: lwm2m::ValueData = match (&value_description.format, value.data()) {
            (storage::ValueFormat::Number { step, .. }, storage::ValueData::Number(v)) => {
                if approx::abs_diff_eq!(*step, 1.0) {
                    if v.is_nan() {
                        lwm2m::ValueData::Integer(None)
                    } else if approx::abs_diff_ne!(v - v.trunc(), 0.0) {
                        return Err(anyhow!("float `{}` has non-zero fraction", v).into());
                    }
                    // XXX: It would sound reasonable to base the decision for
                    //      the type signedness on the range given by the
                    //      value-description since that never changes.
                    //      However, lwm2m doesn't support unsigned values and
                    //      we're only doing that internally for various
                    //      reasons.
                    //      Thus, it doesn't really make any difference and we
                    //      can just use whatever type can hold that number.
                    else if v.is_sign_negative() {
                        if *v < i64::MIN as f64 || *v > i64::MAX as f64 {
                            return Err(anyhow!("float `{}` doesn't fit into i64", v).into());
                        }

                        (*v as i64).into()
                    } else {
                        if *v < u64::MIN as f64 || *v > u64::MAX as f64 {
                            return Err(anyhow!("float `{}` doesn't fit into i64", v).into());
                        }

                        (*v as u64).into()
                    }
                } else {
                    (*v).into()
                }
            }
            (storage::ValueFormat::String { .. }, storage::ValueData::String(v)) => {
                v.to_string().into()
            }
            (storage::ValueFormat::Binary { .. }, storage::ValueData::Binary(v)) => {
                v.clone().into()
            }
            _ => return Err(anyhow!("BUG: value and value-description types don't agree").into()),
        };

        let timestamp = std::time::UNIX_EPOCH + std::time::Duration::from_millis(value.timestamp);

        Ok((data, Some(timestamp)).into())
    }

    // XXX: This implementation is very ineffecient and kinda backwards, but it
    //      allows us to reduce copy&paste a lot. It's a little sad because
    //      this variant is called more often than `handle_partial_write`.
    async fn write_resource(
        &mut self,
        object_instance: usize,
        resource_id: usize,
        resource_instance: usize,
        value: lwm2m::Value,
    ) -> Result<(), lwm2m::Error> {
        if resource_instance != 0 {
            return Err(anyhow!("non-zero resource instance").into());
        }

        let resource_name = self.get_resource_name(resource_id)?;

        let mut values = std::collections::HashMap::with_capacity(1);
        values.insert(resource_name.to_string(), value);

        self.handle_partial_write(object_instance, values).await
    }

    #[tracing::instrument(name = "state", skip_all)]
    async fn handle_partial_write(
        &mut self,
        object_instance: usize,
        values: std::collections::HashMap<String, lwm2m::Value>,
    ) -> Result<(), lwm2m::Error> {
        if object_instance != 0 {
            return Err(anyhow!("non-zero object instance").into());
        }

        let mut inners = Vec::with_capacity(values.len());

        let all_value_names_str = values.keys().join(",");

        for (resource_name, value) in &values {
            let value_description = self.device.value_descriptions.by_name(resource_name)?;

            let device_type_inner = match &value.data {
                lwm2m::ValueData::String(Some(v)) => value_description.makesetmsg_string(v),
                lwm2m::ValueData::Float(Some(v)) => value_description.makesetmsg_number(*v),
                lwm2m::ValueData::Integer(Some(lwm2m::Integer::Signed(v))) => {
                    value_description.makesetmsg_number(*v as f64)
                }
                lwm2m::ValueData::Integer(Some(lwm2m::Integer::Unsigned(v))) => {
                    value_description.makesetmsg_number(*v as f64)
                }
                lwm2m::ValueData::Opaque(v) => value_description.makesetmsg_binary(v),
                _ => return Err(anyhow!("unsupported value type").into()),
            }
            .context("can't make message")?;

            inners.push(device_type_inner);
        }

        let mut network = lsdl::device_message!(value, None, None, inners)?;

        let wor = self.device.description.radio_mode == lsdl::RadioMode::WakeOnRadio;

        if wor {
            network
                .set_go_to_sleep(Some(crate::device::GOTO_SLEEP_IMMEDIATELY))
                .context("can't set go_to_sleep")?;
        }

        /// There's no specified confirmation message.
        /// To speed up the happy path without removing the possibility to wait
        /// for ICMP we just wait for *ANY* value update pretending that to be
        /// a confirmation - which it will be in most cases.
        async fn wait_for_answer(
            device: &mut crate::device::Device,
            now: &mut std::time::Instant,
        ) -> Result<(), Error> {
            device
                .handle_one_update_values_request(|instant, _args| instant >= *now)
                .await
                .ok_or_else(|| anyhow!("queue error"))?;

            Ok(())
        }

        let online = self.device.online();
        let metric_base = format!(
            "write_{}_{}",
            if wor { "wor" } else { "mains" },
            if online { "online" } else { "offline" }
        );

        let mut now = std::time::Instant::now();
        let answer = self
            .device
            .send_message(true, &network, wait_for_answer, false, &mut now)
            .await
            .s_inspect_err(|_| {
                tracing::warn!(
                    metric_name = format!("{metric_base}_without_failure"),
                    metric_value = false,
                    values = all_value_names_str,
                    "Updating values failed",
                )
            })
            .context("can't send value")?;

        tracing::info!(
            remote = true,
            metric_name = format!("{metric_base}_without_failure"),
            metric_value = true,
            values = all_value_names_str,
            "Updating values succeeded",
        );

        if answer.is_some() {
            tracing::info!(
                remote = true,
                metric_name = format!("{metric_base}_without_timeout"),
                metric_value = true,
                values = all_value_names_str,
                "Updating values succeeded (no timeout)",
            );

            for (value_name, value) in values {
                if value.is_big() {
                    tracing::info!("Updated value {} with big hex value", value_name);
                } else {
                    tracing::info!("Updated value {} to {}", value_name, value.data);
                }
            }
        } else {
            tracing::warn!(
                metric_name = format!("{metric_base}_without_timeout"),
                metric_value = false,
                values = all_value_names_str,
                "Updating values succeeded (with timeout)",
            );
        }

        self.device.last_write = Some(tokio::time::Instant::now());

        Ok(())
    }

    async fn exec(
        &mut self,
        _object_instance: usize,
        _resource_id: usize,
        _resource_instance: usize,
        _args: Option<Vec<String>>,
    ) -> Result<(), lwm2m::Error> {
        Err(anyhow!("NOT IMPLEMENTED").into())
    }

    fn parse_resource_name(&self, name: &str) -> Result<usize, lwm2m::Error> {
        Ok(self
            .device
            .value_descriptions
            .by_name(name)
            .context("no value description with that name")?
            .resource_id()?)
    }

    fn get_resource_name(&self, resource_id: usize) -> Result<&str, lwm2m::Error> {
        Ok(self
            .device
            .value_descriptions
            .by_resource_id(resource_id)?
            .name
            .as_deref()
            .ok_or_else(|| anyhow!("value description has no name"))?)
    }

    fn supported_resource_operations(&self, resource_id: usize) -> Result<usize, lwm2m::Error> {
        Ok(
            match &self
                .device
                .value_descriptions
                .by_resource_id(resource_id)?
                .permission
            {
                lsdl::Permission::ReadOnly => lwm2m_types::OP_READ,
                lsdl::Permission::ReadWrite => lwm2m_types::OP_READ | lwm2m_types::OP_WRITE,
                lsdl::Permission::WriteOnly => lwm2m_types::OP_WRITE,
            },
        )
    }

    fn is_array_resource(&self, _resource_id: usize) -> Result<bool, lwm2m::Error> {
        Ok(false)
    }
}
