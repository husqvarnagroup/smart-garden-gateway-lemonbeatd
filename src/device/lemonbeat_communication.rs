// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Communicate with the device and track it's availability.
//!
//! This includes:
//! - online/offline state
//! - wakeup state
//! - sending requests
//! - sending messages
//!
//! This does **NOT** include specific request or message types and their
//! (de-)serialization. For that, look at [lemonbeat_requests](super::lemonbeat_requests).

use crate::comm;
use crate::storage;
use crate::traits::SocketAddrEx as _;
use crate::Error;
use anyhow::anyhow;
use anyhow::Context as _;
use rand::Rng as _;
use std::convert::TryInto as _;

const WAKE_DURATION: std::time::Duration = std::time::Duration::from_secs(20);

impl crate::device::Device {
    pub(super) fn online(&self) -> bool {
        matches!(
            self.persistent.connection_status,
            storage::ConnectionStatus::Online
        )
    }

    /// Call this when you know the online state
    ///
    /// This will handle everything that's needed on such a state change. This
    /// includes sending events and saving to storage.
    pub(super) async fn set_online(&mut self, online: bool) {
        let state_changed = online != self.online();

        self.last_communication_attempt = Some(tokio::time::Instant::now());

        // just had some (successful or unsuccessful) communication. So ping sleep timer
        // has to be recalculated.
        self.sleep_pingtask.abort();

        if !state_changed {
            return;
        }

        self.online_changed_at = tokio::time::Instant::now();
        tracing::info!("Online = {}", online);

        if online {
            // We can only set the UTC time if the device is online.
            self.sleep_utctask.abort();
        }

        self.persistent.connection_status_last_transition = std::time::SystemTime::now();
        self.persistent.connection_status = if online {
            storage::ConnectionStatus::Online
        } else {
            storage::ConnectionStatus::Offline
        };

        if let Err(e) = self
            .publish_resource_instance(
                lwm2m::ObjectType::ConnectionStatus,
                0,
                lwm2m::objects::CONNECTION_STATUS_ONLINE,
                0,
            )
            .await
        {
            tracing::error!("Failed to publish connection status via IPC: {}", e);
        }

        if let Err(e) = self.save_persistent_state().await {
            tracing::error!(
                "Failed to save device-description after changed online flag: {:?}",
                e
            );
        }
    }

    /// Returns [true] is awake
    ///
    /// If it's not, the device needs to be woken up prior to any communication.
    /// Otherwise, it simply won't answer.
    fn is_awake(&self) -> bool {
        // The device is always awake and doesn't need wakeups.
        // This is usually the case for all AC-powered devices.
        if !matches!(self.description.radio_mode, lsdl::RadioMode::WakeOnRadio) {
            return true;
        }

        if !self.description.included {
            // 1) before the inclusion the device is always awake (that's my
            //    assumption, I didn't verify this)
            // 2) the radio module encrypts wakeup messages using the network
            //    key. That's why it wouldn't work anyway.
            return true;
        }

        if let Some(awake_until) = self.awake_until {
            // prevent clock synchronization issues by pretending the device to be
            // sleeping one second too early
            // PANIC: an instance going backwards would be a tokio or std bug.
            let now = tokio::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(1))
                .unwrap();
            now < awake_until
        } else {
            false
        }
    }

    /// Call this after you received a message from the device.
    ///
    /// This function will make sure to update the wakeup state using the
    /// provided timestamps.
    fn device_received_message(
        &mut self,
        sendtime: tokio::time::Instant,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<(), Error> {
        // we don't know when the device received the request because of all
        // the layers between us and the device and the radio module's internal
        // retry.
        // So assume the worst case of the message being received infinitely
        // fast since waking up a woke device only hurts power consumption
        // which is better than failing future communication because the device
        // is sleeping and we think it's awake.
        if let Some(go_to_sleep) = &go_to_sleep {
            self.awake_until = Some(
                sendtime
                    .checked_add(*go_to_sleep)
                    .context("can't add `go_to_sleep` to `now`")?,
            );
        }
        Ok(())
    }

    /// Call this if you sent a message and you got neither ICMP nor an answer.
    ///
    /// Just like [device_received_message](Self::device_received_message), this will update the wakeup state.
    /// Since we don't know if the message was received we'll assume the worst
    /// case though.
    fn device_received_message_maybe(
        &mut self,
        sendtime: tokio::time::Instant,
        go_to_sleep: Option<std::time::Duration>,
    ) -> Result<(), Error> {
        // we don't know if the device received our request or not, update
        // `awake_until`to the soonest possible sleep time.
        if let Some(go_to_sleep) = &go_to_sleep {
            let new_awake_until = sendtime
                .checked_add(*go_to_sleep)
                .context("can't add `go_to_sleep` to `now`")?;

            if self.awake_until.is_none()
                || matches!(self.awake_until, Some(v) if new_awake_until < v)
            {
                self.awake_until = Some(new_awake_until);
            }
        }
        Ok(())
    }

    /// Like [send_request_ex](Self::send_request_ex) but with default values.
    ///
    /// - `timeout`: [None]
    /// - `num_attempts`: 3
    pub(super) async fn send_request<N: Unpin + lsdl::Network + lsdl::NetworkPort + Default>(
        &mut self,
        encrypt: bool,
        network: N,
    ) -> Result<N, Error> {
        self.send_request_ex(encrypt, network, None, 3).await
    }

    /// Send a request to a device.
    ///
    /// A request is not a lemonbeat concept but more of a calling convention.
    /// It means that we expect an answer to the UDP source port.
    ///
    /// On success, the received data is returned.  
    /// After a certain time, this function will give up waiting and return an
    /// error.  
    /// In case of internal errors, an `Error` is returned.
    pub(super) async fn send_request_ex<N: Unpin + lsdl::Network + lsdl::NetworkPort + Default>(
        &mut self,
        encrypt: bool,
        network: N,
        timeout: Option<std::time::Duration>,
        num_attempts: usize,
    ) -> Result<N, Error> {
        let instant_start = std::time::Instant::now();
        let mut address = self.address();
        address.set_flowinfo(crate::device::make_flowinfo(encrypt))?;

        // We use the same request across all attempts so we use the same port
        // for all of them. This means that we'll accept any answer, no matter
        // which of the attempts it was intended for.
        // We concluded that we prefer this behavior over having slow requests
        // fail due to us ignoring those older answers. Requests can take a
        // long time without even receiving ICMP if the radiomodule does CSMA.
        //
        // Another disadvantage of using the same port is that we may receive
        // ICMPs to previous attempts even though the current attempt succeeds.
        // As long we're not at the last attempt this will simply trigger
        // another retry though at which we'll receive an answer.
        let mut request = crate::comm::LemonbeatRequest::new(&self.interface, address, &network)
            .await
            .context("can't create request")?;

        let go_to_sleep = network.go_to_sleep();
        let mut duration = None;
        let mut final_res = Err(comm::RequestError::Timeout);
        for _ in 0..num_attempts {
            if let Some(duration) = duration.take() {
                tokio::time::sleep(duration).await;
            }

            if !self.is_awake() {
                if let Err(err) = self.wakeup(WAKE_DURATION).await.context("can't wake up") {
                    self.set_online(false).await;
                    return Err(err);
                }
            }

            if let Some(timeout) = timeout {
                request.set_timeout(timeout);
            }
            let instant_attempt = tokio::time::Instant::now();
            let res = request.attempt_send_and_recv().await;
            match &res {
                Err(comm::RequestError::Udp(crate::udp::Error::Icmp)) => {
                    log::info!(
                        "Request resulted in ICMP port={} call=T-{}s attempt=T-{}s",
                        N::get_port(),
                        instant_start.elapsed().as_secs_f64(),
                        instant_attempt.elapsed().as_secs_f64()
                    );

                    // Getting an ICMP does not mean we are sure that packet did not reach the
                    // device - only that no ACK got received. So adjust awake_until accordingly:
                    self.device_received_message_maybe(instant_attempt, go_to_sleep)?;
                    duration = Some(std::time::Duration::from_millis(
                        rand::thread_rng().gen_range(1500..2000),
                    ));
                }
                Err(comm::RequestError::Internal(e)) => {
                    log::error!("Internal request error: {:?}", e);

                    duration = Some(std::time::Duration::from_millis(
                        rand::thread_rng().gen_range(1500..2000),
                    ));

                    self.device_received_message_maybe(instant_attempt, go_to_sleep)?;
                }
                Err(comm::RequestError::Timeout) => {
                    log::warn!(
                        "Request timed out port={} call=T-{}s attempt=T-{}s",
                        N::get_port(),
                        instant_start.elapsed().as_secs_f64(),
                        instant_attempt.elapsed().as_secs_f64()
                    );
                    self.device_received_message_maybe(instant_attempt, go_to_sleep)?;
                }
                Ok(_) => {
                    self.device_received_message(instant_attempt, go_to_sleep)?;
                }
            };

            final_res = res;
            if final_res.is_ok() {
                break;
            }
        }

        let online = match &final_res {
            // don't change the online state on internal errors
            Err(crate::comm::RequestError::Internal(_)) => self.online(),
            Err(crate::comm::RequestError::Timeout) => false,
            Err(crate::comm::RequestError::Udp(crate::udp::Error::Icmp)) => false,
            Ok(_) => true,
        };

        // A single attempt is not enough evidence to know the device is
        // offline. When we received an answer we do know if it's online
        // though - no matter how often we tried.
        if num_attempts > 1 || online {
            self.set_online(online).await;
        }

        Ok(final_res?)
    }

    /// Send a message to a device.
    ///
    /// A message is not a lemonbeat concept but more of a calling convention.
    /// It means that you don't expect an answer to your UDP source port.
    /// You may or may not expect an answer on a different channel which you
    /// can wait for in the function `wait_for_answer`.
    ///
    /// After a certain time, this function will give up waiting and return
    /// `Ok(None)`.  
    /// If `wait_for_answer` succeeded, this function will return `Ok` with the
    /// return value of that function.  
    /// In case of internal errors, an `Error` is returned.
    pub(super) async fn send_message<N, F, O, C>(
        &mut self,
        encrypt: bool,
        network: &N,
        wait_for_answer: F,
        answer_required: bool,
        ctx: &mut C,
    ) -> Result<Option<O>, Error>
    where
        N: Unpin + lsdl::Network + lsdl::NetworkPort + Default,
        for<'a> F: crate::traits::FnHelper<'a, Self, O, C>,
    {
        let instant_start = std::time::Instant::now();
        let mut address = self.address();
        address.set_flowinfo(crate::device::make_flowinfo(encrypt))?;

        let go_to_sleep = network.go_to_sleep();
        let mut duration = None;

        let timeout = if answer_required {
            crate::current_config().send_message_timeout
        } else {
            std::time::Duration::from_secs(4)
        };

        for _ in 0..3 {
            if let Some(duration) = duration.take() {
                tokio::time::sleep(duration).await;
            }

            if !self.is_awake() {
                if let Err(err) = self.wakeup(WAKE_DURATION).await.context("can't wake up") {
                    self.set_online(false).await;
                    return Err(err);
                }
            }

            let instant_attempt = tokio::time::Instant::now();

            // We want to use a different port for every attempt to prevent
            // retrying due to a previous attempts ICMP.
            // See `send_request_ex` for a more detailed explanation.
            let mut request = crate::comm::LemonbeatRequest::new(&self.interface, address, network)
                .await
                .context("can't create request")?;
            request.attempt_send().await.context("failed to send")?;

            let elapsed = instant_attempt.elapsed();
            if elapsed > std::time::Duration::from_secs(5) {
                tracing::warn!(
                    "Sending message took a long time call=T-{}s",
                    elapsed.as_secs_f64()
                );
            }

            tokio::select!(
                res = request.read_answer() => {
                    let elapsed = instant_start.elapsed();
                    if elapsed > std::time::Duration::from_secs(10) {
                        tracing::warn!("Reading message answer took a long time call=T-{}s", elapsed.as_secs_f64())
                    }

                    match res {
                        Ok(_network) => {
                            self.device_received_message_maybe(instant_attempt, go_to_sleep)?;
                            anyhow::bail!("received a response to a message");
                        }
                        Err(comm::RequestError::Udp(crate::udp::Error::Icmp)) => {
                            tracing::info!("Message resulted in ICMP port={} call=T-{}s attempt=T-{}s", N::get_port(),
                            instant_start.elapsed().as_secs_f64(),
                            instant_attempt.elapsed().as_secs_f64());

                            // Getting an ICMP does not mean we are sure that packet did not reach the
                            // device - only that no ACK got received. So adjust awake_until accordingly:
                            self.device_received_message_maybe(instant_attempt, go_to_sleep)?;
                            duration = Some(std::time::Duration::from_millis(
                                rand::thread_rng().gen_range(1500..2000),
                            ));
                            continue;
                        }
                        Err(e) => {
                            self.device_received_message_maybe(instant_attempt, go_to_sleep)?;
                            return Err(e).context("failed to read answer");
                        }
                    }
                },

                res = wait_for_answer.call(self, ctx) => {
                    let elapsed = instant_start.elapsed();
                    if elapsed > std::time::Duration::from_secs(10) {
                        tracing::warn!("Side-channel message answer took a long time call=T-{}s", elapsed.as_secs_f64())
                    }

                    self.device_received_message(instant_attempt, go_to_sleep)?;
                    self.set_online(true).await;
                    return Ok(Some(res));
                },

                _res = tokio::time::sleep(timeout) => {
                    let elapsed = instant_start.elapsed();
                    if elapsed > std::time::Duration::from_secs(10) {
                        tracing::warn!("Message took a long time to time out call=T-{}s", elapsed.as_secs_f64())
                    }

                    self.device_received_message_maybe(instant_attempt, go_to_sleep)?;

                    if answer_required {
                        let total_elapsed = instant_start.elapsed();

                        tracing::info!("Message timed out port={} call=T-{}s attempt=T-{}s", N::get_port(),
                            total_elapsed.as_secs_f64(),
                            instant_attempt.elapsed().as_secs_f64());

                        continue;
                    } else {
                        return Ok(None);
                    }
                },
            );
        }

        self.set_online(false).await;
        Err(anyhow!("ran out of attempts"))
    }

    /// Try to wake the device up.
    ///
    /// If successful, the device will be awake for `duration`. The internal
    /// state tracking the wakeup state will be updated.
    async fn wakeup(&mut self, duration: std::time::Duration) -> Result<(), Error> {
        let wakeup_start = std::time::Instant::now();

        let wakeup_channel = self.description.wakeup_channel.channel() as u8;
        tracing::debug!("Wakeup starting on channel {}", wakeup_channel);

        let device_addr = match self.description.address {
            std::net::IpAddr::V6(a) => a,
            _ => anyhow::bail!("IPv6 required"),
        };

        let mac_addr: [u8; 6] = device_addr.octets()[10..16].try_into()?;

        // Three attempts need to be sufficient: user wants faster feedback and higher level
        // components could run into their timeout before wakeup attempts are completed.
        for _ in 0..3 {
            let wakeup_time = tokio::time::Instant::now();

            self.radiomodule
                .try_wakeup_device(&mac_addr, duration, wakeup_channel)
                .await
                .context("unexpected wakeup issue")?;

            let res = tokio::time::timeout(
                std::time::Duration::from_secs(4),
                self.receiver
                    .remove_one_handle_status_request(|instant, status| {
                        instant >= wakeup_time.into_std()
                            && matches!(
                                status.code(),
                                Ok(lsdl::StatusCode::System(lsdl::StatusCodeSystem::Awake))
                            )
                    }),
            )
            .await;

            match res {
                Err(_) => {
                    tracing::debug!("Wakeup timed out waiting for confirmation, retry");
                    continue;
                }
                Ok(v) => {
                    tracing::info!("Wakeup succeeded on channel {}", wakeup_channel);
                    v
                }
            }
            .ok_or_else(|| anyhow!("failed to receive"))?;

            tracing::debug!("Wakeup confirmation received, continue");

            self.awake_until = Some(
                wakeup_time
                    .checked_add(duration)
                    .context("can't add wakeup duration to `now`")?,
            );

            let elapsed = wakeup_start.elapsed();
            if elapsed > std::time::Duration::from_secs(6) {
                tracing::warn!("Wakeup took a long time call=T-{}s", elapsed.as_secs_f64())
            }

            return Ok(());
        }

        match self.persistent.connection_status {
            storage::ConnectionStatus::Offline => {
                tracing::info!(
                    "Wakeup of offline device failed on channel {}",
                    wakeup_channel
                );
            }
            storage::ConnectionStatus::Online => {
                tracing::warn!(
                    "Wakeup of online device failed on channel {}",
                    wakeup_channel
                );
            }
        }

        Err(anyhow!("attempts exceeded"))
    }

    /// Set the lemonbeat value `command` to the given value.
    pub(super) async fn set_command(&mut self, command: i64) -> Result<(), Error> {
        self.set_lemonbeat_value(
            "command",
            lwm2m::Value::new(lwm2m::ValueData::from(command), None),
        )
        .await
        .context("failed to set command")
    }

    /// Sets the lemonbeat value `name` to `value`.
    ///
    /// `value` uses an lwm2m type because we're currently using that API to
    /// send the request. That makes it easier for both the caller and the
    /// implementation of this function and we don't have to duplicate any
    /// code.
    pub(super) async fn set_lemonbeat_value(
        &mut self,
        name: &str,
        value: lwm2m::Value,
    ) -> Result<(), Error> {
        // XXX: Instead of inventing our own internal API for this one usecase,
        //      we just make an IPC call through our external API.
        self.handle_ipc_request(lwm2m::Request {
            op: lwm2m::Method::Write,
            entity: lwm2m::Entity {
                path: format!("lemonbeat/0/{name}/0").into(),
                kind: lwm2m::EntityKind::Device {
                    device: self.description.bnw_id(),
                },
            },
            payload: Some(lwm2m::Payload::Value(value)),
        })
        .await?;

        Ok(())
    }
}
