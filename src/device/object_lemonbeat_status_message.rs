// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

use anyhow::Context as _;
use async_trait::async_trait;

#[async_trait]
impl lwm2m::objects::LemonbeatStatusMessage for crate::device::Device {
    async fn type_escaped(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<u64>, lwm2m::Error> {
        let type_id: u64 = self
            .persistent
            .status
            .as_ref()
            .context("missing status")?
            .type_id
            .into();
        Ok((type_id, Some(self.persistent.modified)))
    }

    async fn code(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<u64>, lwm2m::Error> {
        let code: u64 = self
            .persistent
            .status
            .as_ref()
            .context("missing status")?
            .code
            .into();
        Ok((code, Some(self.persistent.modified)))
    }

    async fn level(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<u64>, lwm2m::Error> {
        let level: u64 = self
            .persistent
            .status
            .as_ref()
            .context("missing status")?
            .level
            .into();
        Ok((level, Some(self.persistent.modified)))
    }

    async fn data(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<String>, lwm2m::Error> {
        let data = self
            .persistent
            .status
            .as_ref()
            .context("missing status")?
            .data
            .as_deref()
            .unwrap_or("");
        Ok((data.to_string(), Some(self.persistent.modified)))
    }
}
