// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Provides [DeviceHandle] for communicating with a single device.

mod fota;
use fota::DataDownload;
use fota::FirmwareContainer;
use fota::FirmwareUploadKind;
use fota::FirmwareUploadState;

mod inclusion;

mod lemonbeat_communication;

mod lemonbeat_requests;
pub use lemonbeat_requests::device_description_to_properties;

mod loadsave;
pub use loadsave::load_spawn;
use loadsave::load_value_descriptions;
use loadsave::load_values;

mod lwm2m_utils;
use lwm2m_utils::PublicationState;

mod object_connectionstatus;
mod object_datadownload;
mod object_device;
mod object_firmwareupdate;
mod object_lemonbeat_status_message;

mod object_lemonbeat;
use object_lemonbeat::LemonbeatHandler;

mod rpcapi;
pub use rpcapi::DeviceHandle;
use rpcapi::DeviceHandleData;
use rpcapi::DeviceReceiver;
use rpcapi::DeviceRequest;

use crate::crypto;
use crate::storage;
use crate::Error;
use anyhow::anyhow;
use anyhow::Context as _;
use crc::{Crc, CRC_16_XMODEM};
use lwm2m::FirmwareUpdateState;
use single_trait_impl::single_trait_impl;
use std::convert::TryFrom as _;
use tracing::Instrument;

const MAX_TIMESTAMP_DELTA: u64 = std::time::Duration::from_secs(60).as_millis() as u64;
const GOTO_SLEEP_IMMEDIATELY: std::time::Duration = std::time::Duration::from_secs(0);
const GOTO_SLEEP_DURATION: std::time::Duration = std::time::Duration::from_secs(20);

const XMODEM: Crc<u16> = Crc::<u16>::new(&CRC_16_XMODEM);

#[single_trait_impl]
impl<T: AsRef<[storage::ValueDescription]>> ValueDescriptionList for T {
    fn by_name(&self, name: &str) -> Result<&storage::ValueDescription, Error> {
        self.as_ref()
            .iter()
            .find(|vd| matches!(vd.name.as_deref(), Some(n) if n == name))
            .ok_or_else(|| anyhow!(format!("no value description for name: {name}")))
    }

    fn by_resource_id(&self, id: usize) -> Result<&storage::ValueDescription, Error> {
        self.as_ref()
            .iter()
            .find(|vd| vd.resource_id().is_ok_and(|rid| rid == id))
            .ok_or_else(|| anyhow!(format!("no value description for resource id: {id}")))
    }

    fn validate_values(&self, values: &[storage::Value]) -> Result<(), Error> {
        let value_descriptions = self.as_ref();

        if values.len() != value_descriptions.len() {
            anyhow::bail!(
                "found {} values but {} descriptions",
                values.len(),
                value_descriptions.len(),
            );
        }

        for value_description in value_descriptions.iter() {
            let value = values
                .by_resource_id(value_description.resource_id()?)
                .context("ids of descriptions and values don't match")?;

            value_description
                .format
                .validate_value_format(value)
                .context("invalid value format")?;
        }

        Ok(())
    }
}

#[single_trait_impl]
impl<T: AsRef<[storage::Value]>> ValueList for T {
    fn by_resource_id(&self, id: usize) -> Result<&storage::Value, Error> {
        self.as_ref()
            .iter()
            .find(|v| v.resource_id().is_ok_and(|rid| rid == id))
            .ok_or_else(|| anyhow!(format!("no value for id: {id}")))
    }
}

trait Lwm2mIdentifier {
    fn resource_id(&self) -> Result<usize, Error>;
}

impl Lwm2mIdentifier for storage::Value {
    /// Use a one-to-one mapping between lemonbeat value identifier and lwm2m resource identifier
    /// Note: data types do not map in our code. Could be cleaned up by using u16 on both sides.
    fn resource_id(&self) -> Result<usize, Error> {
        usize::try_from(self.id).context(format!("lemonbeat id {} out of range for lwm2m", self.id))
    }
}

impl Lwm2mIdentifier for storage::ValueDescription {
    /// Lemonbeat value descriptor identifier match the lemonbeat value identifiers, so same mapping
    /// as for lemonbeat value identifier mapping above.
    fn resource_id(&self) -> Result<usize, Error> {
        usize::try_from(self.id).context(format!("lemonbeat id {} out of range for lwm2m", self.id))
    }
}

/// Creates flowinfo with just the encryption flag set.
fn make_flowinfo(encrypt: bool) -> crate::traits::Flowinfo {
    crate::traits::Flowinfo::new(crate::traits::TrafficClass::new(
        crate::traits::Ds::LOCAL_USE
            | if encrypt {
                crate::traits::Ds::USE_NETWORK_KEY
            } else {
                crate::traits::Ds::empty()
            },
    ))
}

/// Create a new device, spawn the task and return a [DeviceHandle].
///
/// Only call this for new devices for which you've just received a
/// device-description.
pub fn new_spawn<P: AsRef<std::path::Path>>(
    interface: std::net::SocketAddr,
    workdir: P,
    lwm2m_pub_service: lwm2m::PubService,
    radiomodule: crate::radiomodule::RadioModuleHandle,
    description: Box<crate::storage::DeviceDescription>,
    radio_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    network: std::sync::Arc<crypto::Network>,
) -> DeviceHandle {
    tracing::debug!("New device: {:#?}", description);

    let devdir = workdir
        .as_ref()
        .join(format!("Device_descriptionID_{}", description.address));
    let (device, handle) = Device::new(
        interface,
        devdir,
        lwm2m_pub_service,
        radiomodule,
        description,
        network,
        None,
        NewDeviceSource::Air,
    );

    device.spawn(handle.clone_uncounted(), radio_lock);

    handle
}

enum NewDeviceSource {
    /// We saw wireless communication from this device
    Air,
    /// We loaded the device from storage
    Storage,
}

/// Hold all state we need for a single Lemonbeat device
///
/// The public constructors of this struct will spawn a new tokio task und move
/// the struct into that task. It's not shared in any way.
///
/// Instead, those constructors return a cloneable [DeviceHandle] which can be
/// used to run functions on the data via an internal request channel.
/// See [tokio_task_rpc] for more information.
///
/// The reason why we do this is that it allows us to code for complex and
/// long-running processes like device inclusion in a very readable and
/// synchronous-looking way.
///
/// At the same time we don't have to care about data synchronization at all
/// making the code both faster and easier to read and write.
///
/// The classic way of implementing this with the same constraints would be
/// through callback-style event loops. This tends to spread code around a lot
/// making it hard to follow.
///
/// Instead we do it as described before and make sure it's fast and efficient:
/// - according to the [tokio] documentation, tasks have a very low overhead
/// - arguments have to be moved through the internal [tokio::sync::mpsc].
///   For bigger data like [storage::DeviceDescription] we put them in a [Box]
///   so we only have to transfer the internal heap-pointer.
/// - for the lemonbeat side, most RPC calls are events sending us some updated
///   data. The callee doesn't have to wait for the result of that request.
///   It'll just queue the data and proceed with the next packet
///   (possibly for a different device).
///
/// There's an issue with the last point though as this requires us to be able
/// to queue several requests and process them in time.
/// When the queue is full we do drop new requests until there's space again.
/// This was done under the assumption that UDP is lossy anyway and that it'd
/// be okay to drop requests like that.
struct Device {
    /// The address of the ppp-interface.
    ///
    /// Sent packages will use this as their source-address.
    interface: std::net::SocketAddr,
    /// Path to the directory which holds all state for this device.
    ///
    /// This is simply the device-specific directory you know from shadoway.
    devdir: std::path::PathBuf,
    /// Cryptographic network information.
    ///
    /// This structure holds the network's key and allows us to reinclude the
    /// device. We have to (re-)do this ourselves within the device code due to
    /// the way FOTA works.
    ///
    /// We're using an Arc so we can share the data across all devices. We
    /// never need to modify it during the runtime of lemonbeatd.
    network: std::sync::Arc<crypto::Network>,
    /// LWM2M pub-service, used for sending events.
    lwm2m_pub_service: lwm2m::PubService,
    radiomodule: crate::radiomodule::RadioModuleHandle,
    /// The last device-description we received.
    ///
    /// Making this a [Box] makes it more efficient to move it around through
    /// tokio channels.
    description: Box<crate::storage::DeviceDescription>,
    persistent: storage::PersistentState,
    /// All of the devices value descriptions.
    ///
    /// Those are read during inclusion and should never change without a
    /// FOTA-reinclusion. Those must also match [values](Self::values). We don't care about
    /// the order, but the length must be the same, there must be no
    /// duplicates and there must be one value for every value-description.
    value_descriptions: Vec<storage::ValueDescription>,
    /// All of the values.
    ///
    /// See [value_descriptions](Self::value_descriptions) for more information
    values: Vec<storage::Value>,
    /// The receiver side of the main device task.
    ///
    /// This will receive, process and answer requests and represents the main
    /// API to this struct. See [rpcapi].
    receiver: DeviceReceiver,
    /// Prevents deletion of inactive, unincluded devices
    dont_delete_until: tokio::time::Instant,
    /// The instant in time until which the device will (probably) be awake.
    ///
    /// It's okay for this to be an estimation, but it should be estimated too
    /// low, and not too high. It's better to wake the device up when it's
    /// still awake rather than sending a message to a sleeping device because
    /// we assumed it would still be awake.
    awake_until: Option<tokio::time::Instant>,
    /// When was the last time we tried to talk to device, if any?
    ///
    /// This is used to make a decision about when to ping the device.
    last_communication_attempt: Option<tokio::time::Instant>,
    /// The last time the argument to `set_online` changed
    ///
    /// This is basically the same as `persistent.connection_status_last_transition`
    /// but monotonic.
    online_changed_at: tokio::time::Instant,
    /// The last time a value was written to the device
    last_write: Option<tokio::time::Instant>,
    /// The last time rf_link_quality was published
    last_rf_link_quality_publish: Option<tokio::time::Instant>,
    /// Abortion handle to the internal ping task. Abort means "communication (attempt) happened -
    /// recalculate sleep duration until next potential ping. Don't ping."
    sleep_pingtask: crate::AbortableSleep,
    /// Abortion handle to the internal UTC-update task.
    sleep_utctask: crate::AbortableSleep,
    /// Abortion handle to the internal FOTA-retry task.
    sleep_fotatask: crate::AbortableSleep,
    /// If [true], our and the device clock drifted apart too much.
    time_out_of_sync: bool,
    /// Optional upload instructions.
    ///
    /// As soon as this holds some data, the
    /// [try_continue_fota](Self::try_continue_fota) function
    /// will process it by uploading data to the device. You also have to
    /// cancel sleep_fotatask totrigger this process. You might
    /// want to use [init_firmware_image](Self::init_firmware_image) to
    /// provide metadata to the device and simplify the process.
    upload_state: Option<FirmwareUploadState>,
    /// Data-download information that we provide via the LWM2M API.
    data_download: DataDownload,
    /// Our publication state. See [PublicationState] for details.
    publication_state: PublicationState,
}

impl Device {
    // I'm not sure how to solve this. Yes, the number of arguments is
    // horrible. But I'm not sure if we can group them into one or multiple\
    // contexts in a way that makes sense.
    #![allow(clippy::too_many_arguments)]
    fn new(
        interface: std::net::SocketAddr,
        devdir: std::path::PathBuf,
        lwm2m_pub_service: lwm2m::PubService,
        radiomodule: crate::radiomodule::RadioModuleHandle,
        description: Box<crate::storage::DeviceDescription>,
        network: std::sync::Arc<crypto::Network>,
        persistent: Option<crate::storage::PersistentState>,
        source: NewDeviceSource,
    ) -> (Self, DeviceHandle) {
        let (handle, receiver) = DeviceHandle::new(DeviceHandleData {
            bnw_id: description.bnw_id(),
        });

        let connection_status = match &source {
            NewDeviceSource::Air => storage::ConnectionStatus::Online,
            NewDeviceSource::Storage => storage::ConnectionStatus::Offline,
        };
        let mut persistent =
            persistent.unwrap_or_else(|| crate::storage::PersistentState::new(connection_status));

        // in case lemonbeatd terminated during a firmware update, the state would
        // be still 'Downloading', preventing any other updates. As it's never ok
        // for lemonbeatd to be terminated during an update, we can safely reset
        // the state here. another case of an invalid state is when we receive
        // a dev desc with the included flag set to true, while in 'updating'
        // state, which means the update failed and we need to reset the state, too.
        if match persistent.firmware_update_state {
            lwm2m::FirmwareUpdateState::Downloading => true,
            lwm2m::FirmwareUpdateState::Updating if description.included => true,
            _ => false,
        } {
            persistent.firmware_update_state = lwm2m::FirmwareUpdateState::Idle;
        }

        (
            Self {
                interface,
                devdir,
                network,
                lwm2m_pub_service,
                radiomodule,
                description,
                persistent,
                value_descriptions: Vec::new(),
                values: Vec::new(),
                receiver,
                dont_delete_until: tokio::time::Instant::now() + rpcapi::MAX_UNINCLUDED_DURATION,
                awake_until: None,
                sleep_pingtask: crate::AbortableSleep::new(),
                last_communication_attempt: match &source {
                    NewDeviceSource::Air => Some(tokio::time::Instant::now()),
                    NewDeviceSource::Storage => None,
                },
                online_changed_at: tokio::time::Instant::now(),
                last_write: None,
                last_rf_link_quality_publish: None,
                sleep_utctask: crate::AbortableSleep::new(),
                sleep_fotatask: crate::AbortableSleep::new(),
                time_out_of_sync: false,
                upload_state: None,
                data_download: DataDownload::default(),
                publication_state: PublicationState::Invisible,
            },
            handle,
        )
    }

    /// Spawn the internal device task and return.
    ///
    /// This consumes self because it will be moved into the task.
    /// You should only communicate with the device through a [DeviceHandle].
    /// This will also start a timer that periodically checks the device status,
    /// with the interval depending on the current status (online/offline).
    fn spawn(
        mut self,
        mut handle: DeviceHandle,
        radio_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    ) {
        let sleep_pingtask = self.sleep_pingtask.handle();
        let sleep_utctask = self.sleep_utctask.handle();
        let sleep_fotatask = self.sleep_fotatask.handle();
        let handle2 = handle.clone_uncounted();
        let handle3 = handle.clone_uncounted();
        let address = self.description.address;
        let bnw_id = self.description.bnw_id();

        tokioutil::spawn_named(
            &format!("lb-dev-requests-{address}"),
            async move {
                // this is important because we may have created this device in
                // response to receiving the device-description of an unknown
                // device.
                // `set_description` wasn't called yet and thus no event was sent.
                if self.online() && !self.description.included {
                    if let Err(e) = self
                        .publish_includable_device(false, false, 0, lwm2m::Method::Update)
                        .await
                    {
                        tracing::error!("Failed to publish includable device: {:?}", e);
                    }
                }

                self.handle_requests().await;
            }
            .instrument(tracing::info_span!(parent:None, "device", device=%bnw_id)),
        );

        let radio_lock2 = radio_lock.clone();
        tokioutil::spawn_named(
            &format!("lb-dev-ping-{address}"),
            async move {
                loop {
                    let guard = radio_lock2.lock().await;
                    let duration = match handle.try_ping().await {
                        Ok(Ok(next)) => next,
                        Ok(Err(e)) => {
                            tracing::warn!("Failed to ping the device: {}", e);
                            crate::current_config().error_timeout_interval
                        }
                        Err(tokio_task_rpc_util::Error::ReceiverClosed) => break,
                        Err(tokio_task_rpc_util::Error::RecvError) => {
                            tracing::warn!("Failed to read `try_ping` answer");

                            // this shouldn't happen and we don't know if it'll
                            // recover or not so wait a little bit to prevent
                            // infinite loops in case it doesn't.
                            crate::current_config().error_timeout_interval
                        }
                    };
                    drop(guard);

                    if let crate::abortable_sleep::SleepResult::Dropped =
                        sleep_pingtask.sleep(duration).await
                    {
                        break;
                    }
                }
            }
            .instrument(tracing::info_span!(parent:None, "device-ping", device=%bnw_id)),
        );

        tokioutil::spawn_named(
            &format!("lb-dev-utc-{address}"),
            async move {
                let mut handle = handle2;

                loop {
                    let guard = radio_lock.lock().await;
                    let duration = match handle.try_set_utc_offset().await {
                        Ok(Ok(next)) => next,
                        Ok(Err(e)) => {
                            tracing::warn!("Failed to set UTC offset for device: {:?}", e);
                            crate::current_config().error_timeout_interval
                        }
                        Err(tokio_task_rpc_util::Error::ReceiverClosed) => break,
                        Err(tokio_task_rpc_util::Error::RecvError) => {
                            tracing::warn!("Failed to read `try_set_utc_offset` answer");

                            // this shouldn't happen and we don't know if it'll
                            // recover or not so wait a little bit to prevent
                            // infinite loops in case it doesn't.
                            crate::current_config().error_timeout_interval
                        }
                    };
                    drop(guard);

                    if let crate::abortable_sleep::SleepResult::Dropped =
                        sleep_utctask.sleep(duration).await
                    {
                        break;
                    }
                }
            }
            .instrument(tracing::info_span!(parent:None, "device-utc", device=%bnw_id)),
        );

        tokioutil::spawn_named(
            &format!("lb-dev-fota-{address}"),
            async move {
                let mut handle = handle3;

                loop {
                    let duration = match handle.try_continue_fota().await {
                        Ok(next) => next,
                        Err(tokio_task_rpc_util::Error::ReceiverClosed) => break,
                        Err(tokio_task_rpc_util::Error::RecvError) => {
                            tracing::warn!("Failed to read `try_continue_fota` answer");

                            // this shouldn't happen and we don't know if it'll
                            // recover or not so wait a little bit to prevent
                            // infinite loops in case it doesn't.
                            crate::current_config().error_timeout_interval
                        }
                    };

                    if let crate::abortable_sleep::SleepResult::Dropped =
                        sleep_fotatask.sleep(duration).await
                    {
                        break;
                    }
                }
            }
            .instrument(tracing::info_span!(parent:None, "device-fota", device=%bnw_id)),
        );
    }

    /// Returns [None] if a network message doesn't need a gotosleep field.
    ///
    /// The return value of this function is not constant and can change
    /// depending on the state of the device. Specifically during FOTA.u
    fn filter_go_to_sleep(
        &self,
        value: Option<std::time::Duration>,
    ) -> Option<std::time::Duration> {
        if self.description.radio_mode == lsdl::RadioMode::AlwaysOnline {
            // device never sleeps, don't send go_to_sleep value
            None
        } else {
            match value {
                Some(duration) if duration == crate::current_config().gotosleep_duration_fota => {
                    // never adjust special firmware upload go_to_sleep value
                    Some(duration)
                }
                Some(duration) => {
                    if self.persistent.firmware_update_state == FirmwareUpdateState::Downloading {
                        // don't adjust go_to_sleep during firmware upload
                        None
                    } else {
                        // use requested go_to_sleep value
                        Some(duration)
                    }
                }
                None => None,
            }
        }
    }

    /// Return a socket address with source and destination addresses set.
    ///
    /// This address should be used for sending any packages to the device.
    fn address(&self) -> std::net::SocketAddr {
        let mut address = self.interface;
        address.set_ip(self.description.address);
        address
    }

    #[allow(dead_code)]
    fn get_value(&self, val_name: &str) -> Result<&storage::Value, Error> {
        let value_description = self
            .value_descriptions
            .by_name(val_name)
            .context("can't find value description")?;
        self.values.by_resource_id(value_description.resource_id()?)
    }

    /// Update value from a report received from the device.
    async fn update_value(
        &mut self,
        value: storage::Value,
    ) -> Result<Option<(usize, usize)>, Error> {
        let path = self.values_path();
        let resource_id = value.resource_id()?;
        let current = self
            .values
            .iter_mut()
            .find(|v| v.id == value.id)
            .ok_or_else(|| anyhow!("value {} not found", value.id))?;
        if !value.same_type(current) {
            anyhow::bail!("value types don't match");
        }

        *current = value;

        let gateway_timestamp = crate::utils::gateway_timestamp();

        // check timestamp for time drift
        if current.timestamp != 0 {
            let delta = gateway_timestamp.abs_diff(current.timestamp);

            if delta > MAX_TIMESTAMP_DELTA && !self.time_out_of_sync {
                tracing::warn!(
                    "Time drift: timestamp for value with id {} diverges by {:?} ms",
                    current.id,
                    delta,
                );
                self.time_out_of_sync = true
            } else if delta <= MAX_TIMESTAMP_DELTA && self.time_out_of_sync {
                // SG-18576: we would want to log this as "notice" if possible
                tracing::warn!("Time drift: recovered");
                self.time_out_of_sync = false
            }
        }

        // always use gateway current time as value timestamp (for backwards compatibility)
        current.timestamp = gateway_timestamp;

        let value_description = self.value_descriptions.by_resource_id(resource_id);
        let value_ref = value_description
            .as_ref()
            .map_or(current.id.to_string(), |vd| {
                vd.name.clone().unwrap_or_else(|| current.id.to_string())
            });

        tracing::info!("Reported value {} as {}", value_ref, current.data());

        current
            .save(path)
            .await
            .context("can't save updated value")?;

        let name = value_description
            .as_ref()
            .map_or(None, |vd| vd.name.as_deref());
        Ok(match name {
            // These trigger A LOT and the cloud doesn't use them
            // Gardena uses AWS MQTT and pays per message so this check
            // actually saves money.
            Some(
                "command_result" | "internal_communication_status" | "internal_connection_state",
            ) if self.description.is_lawnmower() => None,
            Some("experimental_model_state" | "soil_moisture_experimental_model")
                if self.description.is_sensor2() =>
            {
                None
            }
            _ => Some((resource_id, 0)),
        })
    }

    /// Forget about this device.
    ///
    /// This will send IPC events, delete all state and stop the device task.
    /// If the device is currently included, we'll attempt to exclude it.
    #[tracing::instrument(name = "exclusion", skip_all)]
    async fn forget(&mut self, reason: &str) {
        if self.description.included {
            tracing::info!(
                remote = true,
                reason,
                "Removal requested, send device exclusion and remove internal state"
            );

            // Known points of failure:
            // - lemonbeatd gets terminated before sending the exclusion request
            // - the exclude packet gets lost (on the way to the radio module or in-air)
            // - the packet can't be delivered because the device went out of reach
            //   or shut down
            //
            // XXX: even on failure, we want to delete all internal state
            let res = self.send_exclusion_message().await;
            tracing::info!("Exclusion result: {:?}", res);

            // Technically this is not correct because it may not have
            // succeeded or may still be in progress.
            // This makes the state in RAM match the persistent storage though.
            // In addition to that it ensures that we know that the device
            // should be excluded even if we fail to delete the device for
            // whatever reason.
            self.description.included = false;
        } else {
            tracing::info!(reason, "Removal requested, remove internal state");
        }

        self.receiver.set_enabled(false);

        // NOTE: What if anything after this comment fails?
        //       At this point we have set the status to excluded and requested the device
        //       struct to be deleted so we would exclude it when receiving the next device
        //       description.
        //       We'd still not touch the (possibly broken) flash data though
        //       causing issues after a restart.

        // we don't want to delete the state of the radiomodule because it has
        // the sequence count
        if let Ok(true) = tokio::fs::try_exists(&self.devdir).await {
            // By deleting the work-directory we'll not know about it after a restart of lemonbeatd.
            // If we fail to exclude the device we'll notice that the next time we receive
            // it's device-description with included=true and reattempt the exclusion from that handler.
            //
            // Also, save does nothing for un-included devices so we have to remove
            // the directory without that API.
            if let Err(e) = tokio::fs::remove_dir_all(&self.devdir).await {
                // the directory might not exist if we failed during post-inclusion
                tracing::warn!("Failed to delete device directory: {:?}", e);
            }
        }

        match self.publication_state {
            PublicationState::Invisible => (),
            PublicationState::Includable => {
                if let Err(e) = self
                    .publish_includable_device(true, true, 0, lwm2m::Method::Delete)
                    .await
                {
                    tracing::error!("Failed to publish deleted includable device: {:?}", e);
                }

                if let Err(e) = self
                    .lwm2m_pub_service
                    .remove_includable_device(self.description.address)
                {
                    tracing::error!("Failed to remove includable device: {:?}", e);
                }
            }
            PublicationState::Ready => {
                if let Err(e) = self.publish_deletion().await {
                    tracing::error!("Failed to publish device deletion: {:?}", e);
                }
            }
        }

        tracing::info!("Removal successful");
    }

    /// Update device description as received from the device.
    async fn set_description(&mut self, description: Box<storage::DeviceDescription>) {
        self.description = description;
        if let Err(e) = self.save_device_description().await {
            tracing::error!("failed to save updated device description: {:?}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_log::test]
    fn load_device_data() -> Result<(), Error> {
        let dir =
            std::fs::read_dir("test_data/lemonbeat/random_work").context("can't open workdir")?;
        for entry in dir {
            let entry = entry.context("can't get device entry")?;

            (|| -> anyhow::Result<()> {
                storage::DeviceDescription::load(entry.path())
                    .context("can't load device description")?;

                let value_descriptions = load_value_descriptions(entry.path())
                    .context("can't load value descriptions")?;

                let values =
                    load_values(entry.path(), &value_descriptions).context("can't load values")?;

                value_descriptions
                    .validate_values(&values)
                    .context("failed to validate values")?;

                Ok(())
            })()
            .with_context(|| format!("failed to process entry `{entry:?}`"))
            .unwrap();
        }

        Ok(())
    }
}
