// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

use async_trait::async_trait;

#[async_trait]
impl lwm2m::objects::ConnectionStatus for crate::device::Device {
    async fn online(
        &self,
        _object: usize,
        _resource: usize,
    ) -> Result<lwm2m::TimedData<bool>, lwm2m::Error> {
        Ok((
            self.online(),
            Some(self.persistent.connection_status_last_transition),
        ))
    }

    async fn check(
        &mut self,
        _object: usize,
        _resource: usize,
        _args: Option<Vec<String>>,
    ) -> Result<(), lwm2m::Error> {
        if let Err(e) = self
            .request_config_status_raw(Some(crate::device::GOTO_SLEEP_IMMEDIATELY))
            .await
        {
            tracing::debug!("Error pinging device: {:?}", e);
        }
        Ok(())
    }
}
