// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::traits::Sgtin96 as _;
use async_trait::async_trait;
use chrono::FixedOffset;
use lwm2m::TimedData;

#[async_trait]
impl lwm2m::objects::Device for crate::device::Device {
    async fn manufacturer(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<String>, lwm2m::Error> {
        Ok((
            format!("{:?}", self.description.manufacturer),
            Some(self.description.modified),
        ))
    }

    async fn model_number(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<String>, lwm2m::Error> {
        Ok((
            format!("{}", self.description.sgtin().item_reference()),
            Some(self.description.modified),
        ))
    }

    async fn serial_number(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<String>, lwm2m::Error> {
        Ok((
            format!("{:08}", self.description.sgtin().serial()),
            Some(self.description.modified),
        ))
    }

    async fn firmware_version(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<String>, lwm2m::Error> {
        Ok((
            format!(
                "{}-{}",
                self.description.version_boot, self.description.version_stack
            ),
            Some(self.description.modified),
        ))
    }

    async fn reboot(
        &mut self,
        _object: usize,
        _resource: usize,
        _args: Option<Vec<String>>,
    ) -> Result<(), lwm2m::Error> {
        self.set_lemonbeat_value(
            "command",
            lwm2m::Value::new(
                lwm2m::ValueData::Integer(Some(lwm2m::Integer::Unsigned(31))),
                None,
            ),
        )
        .await?;

        Ok(())
    }

    async fn factory_reset(
        &mut self,
        _object: usize,
        _resource: usize,
        _args: Option<Vec<String>>,
    ) -> Result<(), lwm2m::Error> {
        self.forget("factory reset cloud").await;
        Ok(())
    }

    async fn error_code(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<i64>, lwm2m::Error> {
        Ok((0, Some(self.description.modified)))
    }

    async fn supported_binding_and_modes(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<String>, lwm2m::Error> {
        // UDP, see LWM2M core standard, 'Behaviour with Current Transport Binding and Modes'
        Ok(("U".to_string(), Some(self.description.modified)))
    }

    async fn device_type(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<String>, lwm2m::Error> {
        Ok((
            self.description.device_type().to_string(),
            Some(self.description.modified),
        ))
    }

    async fn hardware_version(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<String>, lwm2m::Error> {
        Ok((
            self.description.version_hw.clone(),
            Some(self.description.modified),
        ))
    }

    async fn software_version(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<String>, lwm2m::Error> {
        Ok((
            self.description.version_app.clone(),
            Some(self.description.modified),
        ))
    }

    async fn utc_offset(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<TimedData<String>, lwm2m::Error> {
        Ok((
            match self.persistent.time_offset {
                None => "".to_string(),
                Some(offset) => {
                    format!("UTC{}", FixedOffset::east(offset))
                }
            },
            Some(self.persistent.modified),
        ))
    }
}
