// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Main lemonbeatd logic
//!
//! This holds all code doing initialization and utilizing the other modules
//! to do what this service is supposed to do.

mod abortable_sleep;
use abortable_sleep::AbortableSleep;

mod comm;

mod config;
pub(crate) use config::current_config;
pub use config::set_config;
pub use config::Config;

mod crypto;

mod device;
use device::device_description_to_properties;
pub use device::DeviceHandle;

mod radiomodule;
mod radiomodule_legacy;
mod storage;
mod traits;
mod udp;
mod utils;

include!(concat!(env!("OUT_DIR"), "/version.rs"));

pub use anyhow::Error;

const CLEANUP_TASK_SLEEP_DURATION: std::time::Duration = std::time::Duration::from_secs(5 * 60);

use crate::traits::ResultEx as _;
use crate::traits::SocketAddrEx as _;
use anyhow::anyhow;
use anyhow::Context as _;
use chrono::Offset as _;
use futures_util::StreamExt as _;
use std::fs;
use tracing::Instrument;

/// Generate a new network key and save it to `path`
async fn make_key<P: AsRef<std::path::Path>>(path: P) -> Result<crypto::NetworkKey, Error> {
    let path = path.as_ref();
    tokio::fs::create_dir_all(
        path.parent()
            .ok_or_else(|| anyhow!("network-key path has no parent directory"))?,
    )
    .await
    .context("can't create parent-directory of network key")?;
    let key = crypto::NetworkKey::generate();
    key.to_file(path)
        .await
        .context("can't write network key to file")?;
    Ok(key)
}

async unsafe fn load_timezone_from_env() -> Result<(), Error> {
    // I can't find a crate declaring this symbol.
    // I checked `libc` and `nix`.
    extern "C" {
        fn tzset();
    }

    // chrono uses `localtime_r` which doesn't call `tzset` internally to
    // make it more thread safe:
    // https://sourceware.org/git/?p=glibc.git;a=blob;f=time/tzset.c;h=a06142bb58b290e1a36d2b1ada98eb8784a8c818;hb=HEAD#l574
    //
    // chrono itself never calls `tzset` itself so we have to do that
    // ourselves.
    //

    // glibc caches the value of `TZ` - even if it's a path to a file
    // like `/etc/localtime`.
    // That means glibc would never reload the file after it was
    // changed.
    // To circumvent this we change the variable, call tzset, change it
    // back and call tzset again.
    //
    // NOTE: Usually, `TZ` is not set and a compile-time provided
    //       default-path(like `/etc/localtime`) is used. In that case
    //       glibc will always reload the file because `TZ` stays
    //       `NULL`.
    //       Our component-tests do set the `TZ` variable though to
    //       emulate changing the timezone.
    if let Some(tz) =
        std::env::var("TZ").map_or(None, |s| if s.starts_with('/') { Some(s) } else { None })
    {
        // The intent here is to change the value of the variable
        // without changing the timezone.
        // This only works if the path is a symbolic link.
        let tz_canon = tokio::fs::canonicalize(&tz)
            .await
            .context("can't canonicalize TZ value")?;
        std::env::set_var("TZ", tz_canon);

        tzset();

        std::env::set_var("TZ", tz);
    }

    tzset();

    Ok(())
}

fn process_workdir_directory(
    entry: &std::fs::DirEntry,
    device_descriptions: &mut Vec<(std::path::PathBuf, Box<storage::DeviceDescription>)>,
) -> Result<(), Error> {
    lazy_static::lazy_static! {
        static ref RE_FILENAME: regex::Regex = regex::Regex::new(r"^Device_descriptionID_(.*)$").unwrap();
    }

    let filetype = entry.file_type().context("can't get filetype")?;
    if !filetype.is_dir() {
        return Ok(());
    }

    let filename = entry.file_name();
    let filename_str = filename
        .to_str()
        .ok_or_else(|| anyhow!("can't convert `{:?}` to string", filename))?;

    let address_str = match RE_FILENAME.captures(filename_str) {
        Some(v) => v.get(1).context("regex has no capture `1`")?.as_str(),
        None => return Ok(()),
    };

    let description = Box::new(
        storage::DeviceDescription::load(entry.path()).context("can't load device description")?,
    );
    if !description.loadsave_enabled() {
        tracing::info!("Found device with loadsave disabled on disk, ignore");
        return Ok(());
    }
    if description.address.to_string() != address_str {
        tracing::warn!(
            "Device description address `{}` does not match the directory name",
            description.address
        );
    }

    device_descriptions.push((entry.path(), description));
    Ok(())
}

fn load_device_descriptions<P>(
    workdir: P,
) -> Result<Vec<(std::path::PathBuf, Box<storage::DeviceDescription>)>, Error>
where
    P: AsRef<std::path::Path>,
{
    let mut device_descriptions = Vec::new();

    let dir = std::fs::read_dir(workdir).context("can't open workdir")?;
    for entry in dir {
        let entry = entry.context("can't get device entry")?;

        if let Err(e) = process_workdir_directory(&entry, &mut device_descriptions) {
            tracing::error!(
                "Can't process direct workdir entry `{:?}`: {:?}",
                entry.path(),
                e
            );
            // ignore error so a broken device doesn't prevent starting
            // `lemonbeatd`
        }
    }
    Ok(device_descriptions)
}

pub(crate) struct ServiceContext {
    network: std::sync::Arc<crate::crypto::Network>,
    interface: std::net::SocketAddr,
    workdir: std::path::PathBuf,
    devices: std::collections::HashMap<std::net::IpAddr, DeviceHandle>,
    lwm2m_pub_service: lwm2m::PubService,
    radiomodule: radiomodule::RadioModuleHandle,
    /// this lock is used to ensure that there'll never be two tasks trying to
    /// set the UTC offset because that'd have a higher chance to fail due to
    /// collisions in-air - especially when we have a lot of device tasks.
    ///
    /// The pingtask locks it as well because those might have the same issues.
    radio_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl ServiceContext {
    pub fn new<P: AsRef<std::path::Path>>(
        network: crate::crypto::Network,
        interface: std::net::SocketAddr,
        workdir: P,
        lwm2m_pub_service: lwm2m::PubService,
        radiomodule: radiomodule::RadioModuleHandle,
    ) -> Self {
        Self {
            network: std::sync::Arc::new(network),
            interface,
            workdir: workdir.as_ref().to_path_buf(),
            devices: std::collections::HashMap::new(),
            lwm2m_pub_service,
            radiomodule,
            radio_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn add_device(&mut self, description: Box<storage::DeviceDescription>) -> DeviceHandle {
        let address = description.address;
        let device = crate::device::new_spawn(
            self.interface,
            self.workdir.clone(),
            self.lwm2m_pub_service.clone(),
            self.radiomodule.clone(),
            description,
            self.radio_lock.clone(),
            self.network.clone(),
        );
        self.devices.insert(address, device.clone());
        device
    }

    pub fn find_device(&mut self, address: &std::net::IpAddr) -> Option<DeviceHandle> {
        if let Some(dev) = self.devices.get(address).cloned() {
            // the device exists, but it was excluded and is no longer valid
            if dev.is_closed() {
                self.devices.remove(address);
                return None;
            }
            return Some(dev);
        }
        None
    }

    fn get_ip_for_bnw_id(&self, bnw_id: &str) -> Option<std::net::IpAddr> {
        let addr = self
            .devices
            .iter()
            .find(|(_, device)| device.bnw_id() == bnw_id);
        if let Some((address, _)) = addr {
            return Some(*address);
        }
        None
    }

    pub fn find_device_by_bnw_id(&mut self, bnw_id: &str) -> Option<DeviceHandle> {
        let key = self.get_ip_for_bnw_id(bnw_id);
        if let Some(addr) = key {
            if let std::collections::hash_map::Entry::Occupied(data) = self.devices.entry(addr) {
                if data.get().is_closed() {
                    data.remove_entry();
                    return None;
                }
                return Some(data.get().clone());
            }
        }
        None
    }

    pub fn handle_timezone_change(&self) -> Result<(), Error> {
        tracing::info!("Timezone changed, notify all devices");

        for device in self.devices.values() {
            // XXX: we rely on the fact that this function is `nowait` so
            //      it'll never (implicitly) wait for us to release the lock on
            //      `self`.
            // NOTE: We ignore RPC errors since we don't need to notify removed
            //       devices.
            match device.handle_timezone_change() {
                // The device was deleted but the handle wasn't yet
                Err(tokio_task_rpc_util::Error::ReceiverClosed) => continue,
                // The device was deleted after queueing, but before processing
                // this request.
                Err(tokio_task_rpc_util::Error::RecvError) => continue,
                Ok(()) => (),
            }
        }

        Ok(())
    }

    pub fn delete_inactive_devices(&mut self) {
        // this does two things at once:
        // - it gives devices a chance to delete themselves when they become
        //   inactive
        // - it deletes device handles whose receiver tasks are gone
        self.devices.retain(|address, device| {
            if device.is_closed() {
                return false;
            }

            // NOTE: We ignore RPC errors since we don't need to delete deleted
            //       devices ;)
            match device.delete_if_inactive() {
                // the receiver task is gone, so we can delete the handle
                Err(tokio_task_rpc_util::Error::ReceiverClosed) => return false,
                // this should never happen since this is a nowait function
                Err(tokio_task_rpc_util::Error::RecvError) => {
                    tracing::error!("BUG: device `{:?}` returned recv error", address);
                }

                Ok(()) => (),
            }

            true
        });
    }
}

async fn handle_device_description(
    ctx: std::sync::Arc<std::sync::Mutex<ServiceContext>>,
    network: lsdl::xsd::device_description::networkType,
    addr: std::net::SocketAddr,
) -> Result<(), Error> {
    let properties = device_description_to_properties(network)
        .context("can't load device description properties")?;
    let description = Box::new(
        storage::DeviceDescription::new(addr.ip(), properties)
            .context("can't parse device description properties")?,
    );

    let encrypted = addr.encrypted();
    if description.included && !encrypted {
        tracing::warn!(
            "Ignore unencrypted device description with included=true flowinfo={}",
            addr.flowinfo().raw()
        );
        return Ok(());
    }

    match (
        description.manufacturer,
        description.product,
        description.type_id,
    ) {
        (lsdl::Manufacturer::Gardena, storage::Product::Device, _) => (),
        _ => {
            tracing::info!("Ignore device description from unsupported device");
            return Ok(());
        }
    }

    let device = ctx.lock().unwrap().find_device(&description.address);
    if let Some(mut device) = device {
        device
            .received_device_description(description)
            .context("can't set device-description")?;
    } else {
        if description.included {
            // this device is included but missing from our list. Possible reasons for that:
            // - we failed to complete the device exclusion
            // - we failed to save or lost(flash corruption) the persistent data for that device
            // - (unlikely) somebody got hold of our network key and joined our network.
            tracing::debug!("Unknown, included device");
            return Ok(());
        }
        ctx.lock().unwrap().add_device(description);
    }

    Ok(())
}

fn handle_status_inner(
    ctx: std::sync::Arc<std::sync::Mutex<ServiceContext>>,
    network: &mut lsdl::xsd::status::networkType,
    addr: std::net::SocketAddr,
) -> Result<(), Error> {
    let device = ctx.lock().unwrap().find_device(&addr.ip());

    let mut device = match device {
        None => {
            tracing::info!(
                device_ip = ?addr.ip(),
                "Received status update from unknown device",
            );
            return Ok(());
        }
        Some(device) => device,
    };

    let responses = lsdl::get_responses!(network, status).context("can't get status responses")?;

    for response in responses {
        match response {
            lsdl::xsd::status::deviceTypeInner::status_report(report) => {
                let status = lsdl::RawStatus::from(report.clone());

                let level = status
                    .level()
                    .map_or_else(|| "<unknown>".to_string(), |level| format!("{level:?}"));
                let code = status
                    .code()
                    .map_or_else(|e| format!("<unknown:{e}>"), |code| format!("{code:?}"));

                tracing::debug!(%addr, %level, %code, "Lemonbeat status");

                device
                    .handle_status(status)
                    .context("can't update values")?;
            }
            _ => anyhow::bail!("got unsupported variant as status response"),
        }
    }

    Ok(())
}

async fn handle_status(
    ctx: std::sync::Arc<std::sync::Mutex<ServiceContext>>,
    mut network: lsdl::xsd::status::networkType,
    addr: std::net::SocketAddr,
) -> Result<(), Error> {
    if !addr.encrypted() {
        tracing::info!("Ignore unencrypted status message");
        return Ok(());
    }

    // we do this in another function so we can print the raw status message in
    // case we failed to handle it
    handle_status_inner(ctx, &mut network, addr)
        .with_context(|| format!("failed to process lemonbeat status: {network:#?}"))
}

async fn handle_value(
    ctx: std::sync::Arc<std::sync::Mutex<ServiceContext>>,
    mut network: lsdl::xsd::value::networkType,
    addr: std::net::SocketAddr,
) -> Result<(), Error> {
    if !addr.encrypted() {
        tracing::info!("Ignore unencrypted value");
        return Ok(());
    }

    let device = ctx.lock().unwrap().find_device(&addr.ip());

    let mut device = match device {
        None => {
            tracing::info!(
                device_ip = ?addr.ip(),
                "Received value update from unknown device",
            );
            return Ok(());
        }
        Some(device) => device,
    };

    let reports =
        lsdl::get_responses!(network, value).context("can't get value response vector")?;

    if reports.is_empty() {
        tracing::warn!("Got value report with 0 values");
        return Ok(());
    }

    let mut values = Vec::with_capacity(reports.len());
    for report in reports {
        match report {
            lsdl::xsd::value::deviceTypeInner::value_report(report) => {
                match storage::Value::new(report) {
                    Err(e) => tracing::error!("Can't parse value: {}", e),
                    Ok(value) => values.push(value),
                }
            }
            _ => tracing::warn!("Got unsupported variant as values response"),
        }
    }

    if let Err(e) = device.update_values(values) {
        tracing::warn!("Can't update value: {}", e);
    }

    Ok(())
}

async fn handle_partnerinfo(
    _ctx: std::sync::Arc<std::sync::Mutex<ServiceContext>>,
    _network: lsdl::xsd::partner_information::networkType,
    addr: std::net::SocketAddr,
) -> Result<(), Error> {
    if !addr.encrypted() {
        tracing::info!("Ignore unencrypted partner information");
        return Ok(());
    }

    // XXX: we don't need this info but listening on the partner_information
    //      port prevents an ICMP being sent back for every incoming message.
    Ok(())
}

fn default_workdir() -> std::path::PathBuf {
    // first try the systemd-provided directory
    match std::env::var("STATE_DIRECTORY") {
        Ok(var) => return var.into(),
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("STATE_DIRECTORY has non-unicode characters")
        }
        Err(std::env::VarError::NotPresent) => (),
    }

    // then the a subdirectory in the current working directory
    std::env::current_dir()
        .expect("can't get the current working directory")
        .join("work")
}

pub(crate) fn runtime_dir() -> std::path::PathBuf {
    // first try the systemd-provided directory
    match std::env::var("RUNTIME_DIRECTORY") {
        Ok(var) => return var.into(),
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("RUNTIME_DIRECTORY has non-unicode characters")
        }
        Err(std::env::VarError::NotPresent) => (),
    }

    // then the a subdirectory in the current working directory
    std::env::current_dir()
        .expect("can't get the current working directory")
        .join("runtime")
}

#[derive(Debug, argh::FromArgs)]
/// lemonbeat server
struct Args {
    /// working directory for persistent data.
    ///
    /// Default value with highest prio first: `STATE_DIRECTORY`, CWD.
    #[argh(option, default = "default_workdir()")]
    work_dir: std::path::PathBuf,

    /// path to shadoway state directory.
    ///
    /// Usually you don't have to change it but having it as an option allows
    /// testing it outside of the gateway.
    #[argh(option, default = "std::path::PathBuf::from(\"/var/lib/shadoway\")")]
    shadoway_state_dir: std::path::PathBuf,

    /// radiomodule mac address
    #[argh(option)]
    mac_address: Option<macaddr::MacAddr6>,

    /// print the version and exit
    #[argh(switch)]
    version: bool,
}

async fn handle_ipc_all_devices_request(
    ctx: &std::sync::Arc<std::sync::Mutex<ServiceContext>>,
) -> Result<serde_json::Value, Error> {
    let mut devices = ctx.lock().unwrap().devices.clone();
    let mut payload = std::collections::HashMap::new();
    for (addr, device) in devices.iter_mut() {
        let bnw_id = device.bnw_id();

        let res = match device
            .as_ipso()
            .instrument(tracing::info_span!(parent:None, "device", device=%bnw_id))
            .await
        {
            // The device was deleted but the handle wasn't yet
            Err(tokio_task_rpc_util::Error::ReceiverClosed) => continue,
            // The device was deleted after queueing, but before processing
            // this request.
            Err(tokio_task_rpc_util::Error::RecvError) => continue,
            Ok(res) => res,
        };

        match res {
            Err(e) => {
                tracing::warn!(
                    device=%bnw_id,
                    "Failed to get IPSO data for device at address {:?}: {:?}",
                    *addr,
                    e
                );
            }

            Ok(Some(json)) => {
                payload.insert(device.bnw_id(), json);
            }
            // 'None' means either the device is the radio module or
            // unincluded, so not to be added the device list
            Ok(None) => {}
        }
    }
    Ok(serde_json::json!(payload))
}

async fn handle_ipc_device_inclusion_request(
    ctx: &std::sync::Arc<std::sync::Mutex<ServiceContext>>,
    request: &lwm2m::Request,
) -> Result<Option<serde_json::Value>, Error> {
    lazy_static::lazy_static! {
        static ref RE_INCLUDE:regex::Regex = regex::Regex::new(r"^includable_device/([0-9]+)/include$").unwrap();
    }

    let captures = match RE_INCLUDE.captures(
        request
            .entity
            .path
            .to_str()
            .ok_or_else(|| anyhow!("entity path is not valid UTF8"))?,
    ) {
        Some(v) => v,
        None => return Ok(None),
    };
    let includable_id: u16 = captures
        .get(1)
        .ok_or_else(|| anyhow!("can't get includable-id from valid path"))?
        .as_str()
        .parse()
        .context("includable-id is not a u16")?;
    let lwm2m_pub_service = ctx.lock().unwrap().lwm2m_pub_service.clone();
    let address = lwm2m_pub_service
        .address_from_includable_id(includable_id)
        .ok_or_else(|| anyhow!("includable_device `{}` not found", includable_id))?;
    let mut device = ctx
        .lock()
        .unwrap()
        .find_device(&address)
        .ok_or_else(|| anyhow!("device `{}` not found", address))?;

    device.include().context("can't include device")?;

    Ok(Some(serde_json::json!({})))
}

async fn handle_ipc_device_repair_request(
    ctx: &std::sync::Arc<std::sync::Mutex<ServiceContext>>,
    request: &lwm2m::Request,
) -> Result<Option<serde_json::Value>, Error> {
    lazy_static::lazy_static! {
        static ref RE_INCLUDE:regex::Regex = regex::Regex::new(r"^device/([a-zA-Z0-9]+)/repair$").unwrap();
    }

    let captures = match RE_INCLUDE.captures(
        request
            .entity
            .path
            .to_str()
            .ok_or_else(|| anyhow!("entity path is not valid UTF8"))?,
    ) {
        Some(v) => v,
        None => return Ok(None),
    };
    let device_id = captures
        .get(1)
        .ok_or_else(|| anyhow!("can't get device-id from valid path"))?
        .as_str();
    let mut device = ctx
        .lock()
        .unwrap()
        .find_device_by_bnw_id(device_id)
        .ok_or_else(|| anyhow!("device `{}` not found", device_id))?;

    device
        .repair()
        .await
        .context("can't call repair")?
        .context("can't repair device")?;

    Ok(Some(serde_json::json!({})))
}

async fn handle_ipc_device_request(
    ctx: &std::sync::Arc<std::sync::Mutex<ServiceContext>>,
    request: lwm2m::Request,
    device: String,
) -> Result<serde_json::Value, Error> {
    let mut device = ctx
        .lock()
        .unwrap()
        .find_device_by_bnw_id(&device)
        .ok_or_else(|| anyhow!("device `{}` not found", device))?;
    device
        .handle_ipc_request(request)
        .await?
        .context("can't handle IPC request")
}

async fn handle_ipc_request(
    ctx: std::sync::Arc<std::sync::Mutex<ServiceContext>>,
    request: lwm2m::Request,
) -> Result<serde_json::Value, Error> {
    match request.entity.kind.clone() {
        lwm2m::EntityKind::Gateway { service } => {
            if service != "lemonbeatd" {
                anyhow::bail!("unsupported service type: {}", service);
            }

            if matches!(request.entity.path.to_str(), Some("devices")) {
                handle_ipc_all_devices_request(&ctx).await
            } else if let Some(ret) = handle_ipc_device_inclusion_request(&ctx, &request).await? {
                Ok(ret)
            } else if let Some(ret) = handle_ipc_device_repair_request(&ctx, &request).await? {
                Ok(ret)
            } else {
                Err(anyhow!("unsupported service path"))
            }
        }
        lwm2m::EntityKind::Device { device } => {
            handle_ipc_device_request(&ctx, request, device).await
        }
    }
}

async fn handle_fw_update_message(
    _ctx: std::sync::Arc<std::sync::Mutex<ServiceContext>>,
    _network: lsdl::xsd::firmware_update::networkType,
    addr: std::net::SocketAddr,
) -> Result<(), Error> {
    if !addr.encrypted() {
        tracing::info!("Ignore unencrypted firmware update message");
        return Ok(());
    }

    tracing::debug!("An unexpected firmware update message was received, discarding...");
    Ok(())
}

async fn setup_radiomodule(
    mac_address: &macaddr::MacAddr6,
    network: &crate::crypto::Network,
    device_descriptions: &[(std::path::PathBuf, Box<storage::DeviceDescription>)],
    work_dir: std::path::PathBuf,
) -> Result<radiomodule::RadioModuleHandle, Error> {
    let mut radiomodule = radiomodule::create();

    let rm_version = radiomodule.get_app_version().await.unwrap();
    tracing::info!("Radio module version: {}", rm_version);

    tracing::info!("Set MAC address to {}", mac_address);
    radiomodule
        .set_mac_address(mac_address)
        .await
        .context("failed to set MAC address")?;

    tracing::info!("Set network_key");
    radiomodule
        .set_network_key(network)
        .await
        .context("failed to set network key")?;

    tracing::info!("Set antenna diversity mode");
    let result = radiomodule
        .set_antenna_diversity_mode(radiomodule::DiversityMode::Trx)
        .await
        .context("failed to set antenna diversity mode")?;

    if !result {
        tracing::info!("Hardware does not support antenna diversity");
    }

    let msc_current = radiomodule
        .get_tx_mac_counter()
        .await
        .context("can't get MSC")?;

    tracing::info!("current MSC: {}", msc_current);

    // store TX MAC sequence counter for potential rollback
    fs::write(work_dir.join("TXMSC.txt"), format!("{msc_current}\n"))?;

    for (devdir, device_description) in device_descriptions {
        if !device_description.is_radio_module() {
            continue;
        }

        tracing::info!("Found gateway device {}", device_description.address);

        let msc = radiomodule_legacy::load_msc_hacky(devdir)
            .await
            .context("can't load MSC (hacky)")?;

        let msc: u64 = msc.into();

        // The old firmware sends us `raw >> 10 + 1`.
        // That means we have to undo the `>> 10` but don't have to increment.
        let msc_raw = msc << 10;

        tracing::info!("stored MSC: {}", msc_raw);

        if msc_raw > msc_current {
            tracing::info!("setting restored MSC");
            radiomodule
                .set_tx_mac_counter(msc_raw)
                .await
                .context("can't set MSC")?;
        }
    }

    Ok(radiomodule)
}

async fn process_dbus_messages(
    context: &std::sync::Arc<std::sync::Mutex<ServiceContext>>,
) -> Result<(), Error> {
    let (resource, conn) = dbus_tokio::connection::new_system_sync()?;
    let _handle = tokio::spawn(async {
        let err = resource.await;
        panic!("Lost connection to D-Bus: {err}");
    });

    let mut mr = dbus::message::MatchRule::new_signal(
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
    );
    mr.path = Some("/org/freedesktop/timedate1".into());

    let radio_lock = context.lock().unwrap().radio_lock.clone();

    let (_incoming_signal, stream) = conn.add_match(mr).await?.msg_stream();
    stream
        .for_each(|_msg| async {
            // The lock ensures that at least none of our device tasks uses the
            // C-library's timezone data while writing to it.
            let res = unsafe {
                let _guard = radio_lock.lock().await;
                load_timezone_from_env().await
            };
            if let Err(e) = res {
                tracing::error!("Failed to change process timezone: {:?}", e);

                // We don't know what state we left the timezone in. It's
                // possible that we have a wrong or broken timezone now.
                // New devices would start using that but by returning here we
                // can at least make sure that we don't set that timezone to
                // the existing devices (yet).
                return;
            }

            if let Err(e) = context.lock().unwrap().handle_timezone_change() {
                tracing::error!("Failed to handle timezone change: {:?}", e);
            }
        })
        .await;

    Ok(())
}

/// calls `handle_timezone_change` when the UTC offset changes due to DST
///
/// DST: Daylight Savings Time
/// Currently, this polls the timezone. Ideally chrono would give us access to
/// the timezone information that tells us when the changes happen.
async fn process_dst_changes(
    context: &std::sync::Arc<std::sync::Mutex<ServiceContext>>,
) -> Result<(), Error> {
    let mut offset = chrono::Local::now().offset().fix().local_minus_utc();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30 * 60)).await;

        let new_offset = chrono::Local::now().offset().fix().local_minus_utc();
        if new_offset != offset {
            offset = new_offset;

            if let Err(e) = context.lock().unwrap().handle_timezone_change() {
                tracing::error!("Failed to handle timezone change: {:?}", e);
            }
        }
    }
}

fn spawn_devices(
    mut device_descriptions: Vec<(std::path::PathBuf, Box<storage::DeviceDescription>)>,
    context: std::sync::Arc<std::sync::Mutex<ServiceContext>>,
) {
    let (interface, lwm2m_pub_service, radiomodule, radio_lock, network) = {
        let context = context.lock().unwrap();

        (
            context.interface,
            context.lwm2m_pub_service.clone(),
            context.radiomodule.clone(),
            context.radio_lock.clone(),
            context.network.clone(),
        )
    };

    for (path, description) in device_descriptions.drain(..) {
        let span = tracing::info_span!("device", device=%description.bnw_id());
        let _enter = span.enter(); // ok - not within async function

        let address = description.address;

        // Historically the gateway itself was a device a well, so we have to
        // filter those in case we're using a migrated work directory.
        if description.is_radio_module() {
            continue;
        }

        let device = match crate::device::load_spawn(
            interface,
            &path,
            lwm2m_pub_service.clone(),
            radiomodule.clone(),
            description,
            radio_lock.clone(),
            network.clone(),
        ) {
            Ok(device) => device,
            Err(e) => {
                tracing::error!("Can't load_spawn `{:?}`: {:?}", path, e);
                continue;
            }
        };

        // XXX: If the device does already exist, we received a
        //      device-description with included=false before we got a chance
        //      to load it from disk(with included=true). If that is the case,
        //      it got excluded while lemonbeatd wasn't running and the data we
        //      stored has no meaning.
        //      Since fixing this would require making assumptions about the
        //      code which might not be true forever, we decided to ignore it
        //      because it will fix itself after 10s when the device sends the
        //      next device-description. If it doesn't (because it was the last
        //      one), then we might as well never received it because we
        //      started 10s later than we actually did.
        context.lock().unwrap().devices.insert(address, device);
    }
}

async fn prepare_workdir(args: &Args) -> Result<(), Error> {
    // It might already exist in case it was provided by systemd or if this is
    // not the first run
    tokio::fs::create_dir_all(&args.work_dir)
        .await
        .context("can't create workdir")?;

    let shadoway_workdir = args.shadoway_state_dir.join("work");

    // This file was created by shadoway to buffer events that have not been
    // sent to the backend. It can be quite big since it can store up to 10000
    // messages and is no longer needed.
    //
    // We remove it before the migration so we don't unnecessarily move it to
    // the new location.
    match tokio::fs::remove_file(shadoway_workdir.join("messages.json")).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => tracing::error!("failed to remove messages.json: {}", e),
        Ok(()) => tracing::info!("successfully deleted messages.json"),
    }

    if shadoway_workdir.exists() {
        tracing::info!("shadoway work directory exists. Attempt migration.");

        let is_empty = tokio::fs::read_dir(&args.work_dir)
            .await
            .context("can't start reading workdir")?
            .next_entry()
            .await
            .context("can't read next entry in workdir")?
            .is_none();
        if !is_empty {
            anyhow::bail!("shadoway directory exists but our workdir is not empty");
        }

        // Move the contents of shadoways workdir to our new workdir.
        // We don't move the folder itself so we keep permissions that systemd
        // has possibly set on that directory.
        let mut workdir_reader = tokio::fs::read_dir(&shadoway_workdir).await?;
        while let Some(entry) = workdir_reader.next_entry().await? {
            let old_path = entry.path();
            let new_path = args.work_dir.join(
                old_path
                    .file_name()
                    .with_context(|| format!("no filename in `{}`", old_path.display()))?,
            );

            tokio::fs::rename(&old_path, &new_path)
                .await
                .with_context(|| {
                    format!(
                        "failed to move `{}` to `{}`",
                        old_path.display(),
                        new_path.display()
                    )
                })?;
        }

        // It should now be empty and thus a simple remove should work.
        // If not, we failed to do our job.
        tokio::fs::remove_dir(shadoway_workdir)
            .await
            .context("failed to delete shadoway workdir after migration")?;

        tracing::info!("Successfully migrated shadoway state directory");
    }

    match tokio::fs::remove_dir_all(&args.shadoway_state_dir).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        // We don't fail here because we don't care. The workdir is already
        // gone so the migration was successful. We might fail removing the
        // rest due to permission or filesystem errors but we can keep working
        // without removing it.
        Err(e) => tracing::error!("Failed to remove shadoway state directory: {}", e),
        Ok(()) => tracing::info!("Successfully deleted shadoway state directory"),
    }

    Ok(())
}

/// main entry point to lemonbeatd
///
/// This is the simplest way to extract the functionality into a crate.
/// As soon as somebody actually wants to use it, the API will definitely have
/// to improve.
pub async fn run() -> Result<(), Error> {
    let args: Args = argh::from_env();
    tracing::info!("Args: {:?}", args);

    if args.version {
        println!("{VERSION}");
        return Ok(());
    }

    tracing::debug!("LSDL version: {}", lsdl::get_version());
    tracing::info!("Lemonbeatd version: {}", VERSION);

    prepare_workdir(&args).await?;

    tokio::fs::create_dir_all(crate::runtime_dir())
        .await
        .context("can't create runtime dir")?;

    let network_key_path = args.work_dir.join("Network_management/Network_key.json");

    let network_key = match crypto::NetworkKey::from_file(&network_key_path) {
        Err(crypto::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            make_key(&network_key_path)
                .await
                .context("can't create network key")?
        }
        Err(e) => return Err(e).context("can't load network key"),
        Ok(v) => v,
    };
    let network = crypto::Network::new(network_key, None)
        .context("can't init crypto module with network key")?;

    let mac_address = if let Some(mac_address) = args.mac_address {
        mac_address
    } else if let Ok(mac_address) = radiomodule::get_rm_mac()
        .await
        .s_inspect_err(|e| tracing::warn!("Can't load RM MAC from uboot, using default: {e:#}"))
    {
        mac_address
    } else {
        // This MAC address was copied from the old rm-flashing python script
        macaddr::MacAddr6::new(0x8c, 0x05, 0x51, 0x00, 0x06, 0xe2)
    };

    let work_dir = args.work_dir.clone();
    let device_descriptions =
        tokio::task::spawn_blocking(move || load_device_descriptions(work_dir))
            .await?
            .context("can't load device descriptions from storage")?;

    let interface_addr: std::net::SocketAddr = "[fc00::6:100:0:0]:8888".parse().unwrap();

    let work_dir = args.work_dir.clone();
    let radiomodule = setup_radiomodule(&mac_address, &network, &device_descriptions, work_dir)
        .await
        .context("failed to setup radiomodule")?;

    // NOTE: this is a tokio-mutex for two reasons:
    //       - we can have both the sequence and the inner pubservice in one struct
    //       - the packets go out in order of their sequence numbers.
    // NOTE: this doesn't init the socket so nobody can subscribe yet and all
    //       outgoing events will be dropped. We'll init it as soon as we're
    //       ready.
    let (pub_service_builder, pub_service) = sg_ipc::PubServiceBuilder::new();
    let pub_service = lwm2m::PubService::new(pub_service, "lemonbeatd".to_string());

    let context = std::sync::Arc::new(std::sync::Mutex::new(ServiceContext::new(
        network,
        interface_addr,
        &args.work_dir,
        pub_service.clone(),
        radiomodule,
    )));

    let svc = comm::Service::new(interface_addr, context.clone(), handle_device_description);
    svc.start();

    let svc = comm::Service::new(interface_addr, context.clone(), handle_status);
    svc.start();

    let svc = comm::Service::new(interface_addr, context.clone(), handle_value);
    svc.start();

    let svc = comm::Service::new(interface_addr, context.clone(), handle_fw_update_message);
    svc.start();

    let svc = comm::Service::new(interface_addr, context.clone(), handle_partnerinfo);
    svc.start();

    // This has to be ready before spawning any devices so they can't miss any
    // timezone changes. the events can be processes in parallel to the spawn
    // call below and the sync mutex on the service context ensures that adding
    // new devices and notifying all known devices about a timezone change
    // can't run in parallel.
    let context2 = context.clone();
    tokioutil::spawn_named("lb-devcheck", async move {
        let ret = process_dbus_messages(&context2).await;
        tracing::error!("process_dbus_messages returned: {:?}", ret);
    });

    let context2 = context.clone();
    tokioutil::spawn_named("lb-dst-notifier", async move {
        let ret = process_dst_changes(&context2).await;
        tracing::error!("process_dst_changes returned: {:?}", ret);
    });

    // only now we load devices so they don't start communicating
    // before the radio module is included and ready to forward traffic
    let context2 = context.clone();
    tokio::task::spawn_blocking(move || spawn_devices(device_descriptions, context2)).await?;

    let rep_service = sg_ipc::RepService::new("/tmp/lemonbeatd-command.ipc");
    let context2 = context.clone();
    lwm2m::start_repservice(rep_service, "lemonbeatd".to_string(), move |msg| {
        handle_ipc_request(context2.clone(), msg)
    })
    .context("can't start rep service")?;

    pub_service_builder
        .start("/tmp/lemonbeatd-event.ipc")
        .context("can't start IPC pub service")?;

    // NOTE: component-tests use this as an indicator that the inclusion
    //       is done - including everything being written to storage.
    systemd_async::notify(false, "READY=1")
        .await
        .context("can't notify systemd")?;

    loop {
        tokio::time::sleep(CLEANUP_TASK_SLEEP_DURATION).await;
        context.lock().unwrap().delete_inactive_devices();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_log::test(tokio::test)]
    async fn load_devices() {
        tokio::task::spawn_blocking(move || {
            let (_pub_service_builder, pub_service) = sg_ipc::PubServiceBuilder::new();
            let pub_service = lwm2m::PubService::new(pub_service, "lemonbeatd".to_string());

            let radiomodule = radiomodule::create();

            let workdir = "./test_data/storage/sample_work";
            let network = crypto::Network::new(crypto::NetworkKey::generate(), None).unwrap();
            let ctx = std::sync::Arc::new(std::sync::Mutex::new(ServiceContext::new(
                network,
                "127.0.0.1:0".parse().unwrap(),
                workdir,
                pub_service,
                radiomodule,
            )));
            let device_descriptions = load_device_descriptions(workdir).unwrap();

            assert_eq!(ctx.lock().unwrap().devices.len(), 0);
            assert_eq!(device_descriptions.len(), 1);

            spawn_devices(device_descriptions, ctx.clone());
            assert_eq!(ctx.lock().unwrap().devices.len(), 1);
        })
        .await
        .unwrap();
    }
}
