// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Types representing the shadoway work directory.
//! This module supports loading and storing data in it.
//!
//! The goal of this module is not just to exactly represent shadoways json
//! files. It's to convert their values to types that make the most sense to us.
//! In some places this means heavy usage of custom serde Serializers and
//! Deserializers.

use crate::Error;

use crate::traits::Sgtin96 as _;
use anyhow::anyhow;
use anyhow::Context as _;
use derive_more::Debug;
use lsdl::PropertyEx as _;
use num_traits::cast::FromPrimitive as _;
use num_traits::ToPrimitive as _;
use serde::de::Deserialize as _;
use serde::Deserializer as _;
use std::convert::TryInto as _;

pub const ITEM_REFERENCE_CBT2: u32 = 6146;
pub const ITEM_REFERENCE_CBTG: u32 = 29694;
pub const ITEM_REFERENCE_CBTL: u32 = 53988;
pub const ITEM_REFERENCE_WATER_CONTROL: u32 = 18869;
pub const ITEM_REFERENCE_IRRIGATION_CONTROL: u32 = 31653;
pub const ITEM_REFERENCE_SENSOR2: u32 = 19040;

/// Save a serializable struct to a json file
///
/// This function will replace a possibly existing file atomically by writing
/// to a temporary file first and `rename`ing onto the destination at the very
/// end. Note: this does not guarantee durability of the file - ie. if gateway
/// gets restarted with the following few seconds the file could vanish. This
/// is not seen as an issue. If it becomes required, the parent directory has
/// to be synced as well.
///
/// Internally this uses blocking tokio tasks to do synchronous file IO.
pub async fn save_json<P: AsRef<std::path::Path>, T: 'static + Clone + Send + serde::Serialize>(
    path: P,
    value: &T,
) -> Result<(), Error> {
    let path = path.as_ref();
    let value = (*value).to_owned();
    let path_tmp = path.with_file_name(format!(
        "tmp.{}",
        path.file_name()
            .ok_or_else(|| anyhow!("no filename in JSON path"))?
            .to_str()
            .ok_or_else(|| anyhow!("JSON filename has non-unicode characters"))?
    ));
    let path_tmp_fortask = path_tmp.to_owned();

    // remove tempfile if it exists so we'll definitely get a new one in case
    // a thread whose .await got aborted is still writing to it.
    match tokio::fs::remove_file(&path_tmp).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
        Ok(_) => (),
    }

    tokio::task::spawn_blocking(move || -> Result<(), Error> {
        let file = std::fs::File::create(path_tmp_fortask).context("can't create file")?;
        serde_json::to_writer_pretty(&file, &value).context("can't write json to file")?;
        file.sync_all().context("can't sync file")?;
        Ok(())
    })
    .await??;

    tokio::fs::rename(&path_tmp, path)
        .await
        .context("can't rename into final destination")?;

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, num_derive::ToPrimitive, num_derive::FromPrimitive)]
pub enum ConnectionStatus {
    Offline = 0,
    Online = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum InclusionStatus {
    #[serde(rename = "STATUS_DEVICE_INCLUDING")]
    DeviceIncluding,
    #[serde(rename = "STATUS_DEVICE_INCLUSION_FAILURE")]
    DeviceInclusionFailure,
    #[serde(rename = "STATUS_DEVICE_INCLUSION_SUCCESS")]
    DeviceInclusionSuccess,

    #[serde(rename = "STATUS_DEVICE_REPORTING")]
    DeviceReporting,
    #[serde(rename = "STATUS_DEVICE_REPORT_FAILURE")]
    DeviceReportFailure,
    #[serde(rename = "STATUS_DEVICE_REPORT_SUCCESS")]
    DeviceReportSuccess,
    #[serde(rename = "STATUS_DEVICE_REPORT_VALUES_SUCCESS")]
    DeviceReportValuesSuccess,

    #[serde(rename = "INCLUDED")]
    Included,
    #[serde(rename = "EXCLUDED")]
    Excluded,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
pub enum Product {
    Gateway,
    #[serde(rename = "DEVICE")]
    Device,
}

/// Deserialize an enum from a number
pub fn deserialize_numderive<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: num_traits::FromPrimitive,
{
    let num = u64::deserialize(deserializer)?;
    T::from_u64(num).ok_or_else(|| serde::de::Error::custom("can't convert u64 to enum"))
}

/// Serialize an enum into a number
fn serialize_numderive<S, T>(data: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
    T: num_traits::ToPrimitive,
{
    serializer.serialize_u64(
        data.to_u64()
            .ok_or_else(|| serde::ser::Error::custom("can't convert enum to u64"))?,
    )
}

/// Deserialize optional String
///
/// Since for some fields shadoway stores an empty string when there is no data
/// we have to convert that to [None] when deserializing.
pub fn deserialize_optstr<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = Option::<String>::deserialize(deserializer)?;
    if let Some(s) = s {
        if s.is_empty() {
            Ok(None)
        } else {
            Ok(Some(s))
        }
    } else {
        Ok(None)
    }
}

/// Deserialize [bool] that might be stored as an integer
///
/// Since for some fields shadoway stores booleans as integers we have to
/// convert these explicitly during deserialization.
pub fn deserialize_intbool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Visitor;

    impl serde::de::Visitor<'_> for Visitor {
        type Value = bool;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a bool or the number 0/1")
        }

        fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(v)
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            match v {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(serde::de::Error::custom("invalid number for bool")),
            }
        }
    }

    deserializer.deserialize_any(Visitor)
}

/// Deserialize [u32] that might be stored as a float
///
/// Since for some fields shadoway stores integers as floats we have to
/// convert these explicitly during deserialization.
pub fn deserialize_floatu32<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Visitor;

    impl serde::de::Visitor<'_> for Visitor {
        type Value = u32;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a u32 or float close to x.0")
        }

        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            v.try_into().map_err(serde::de::Error::custom)
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            v.try_into().map_err(serde::de::Error::custom)
        }

        fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            let class_ok = matches!(
                v.classify(),
                std::num::FpCategory::Zero | std::num::FpCategory::Normal
            );

            if !class_ok || !v.is_sign_positive() || v > u32::MAX.into() || v.fract().abs() > 1e-10
            {
                Err(serde::de::Error::custom(format!(
                    "float `{v}` is not convertible to u32"
                )))
            } else {
                Ok(v as u32)
            }
        }
    }

    deserializer.deserialize_any(Visitor)
}

pub struct ChannelMap {
    inner: num_bigint::BigUint,
}

impl std::fmt::Debug for ChannelMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_list()
            .entries((0..self.inner.bits()).filter_map(|id| {
                if self.inner.bit(id) {
                    Some(id + 1)
                } else {
                    None
                }
            }))
            .finish()
    }
}

impl ChannelMap {
    pub fn new(num: num_bigint::BigUint) -> Self {
        Self { inner: num }
    }

    pub fn from_bytes_be(bytes: &[u8]) -> Self {
        Self::new(num_bigint::BigUint::from_bytes_be(bytes))
    }

    /// Return [true] if the given channel is present in this channel map
    ///
    /// It being present means that it's in use by whatever usecase this map
    /// represents.
    ///
    /// `2.2.1.7.4 Channel map` of the lemonbeat specification contains a list
    /// of possible channel map types. The last time I checked those where:
    /// - Synchronization channel map
    /// - Normal channel map
    /// - Blacklist channel map
    pub fn contains(&self, mut n: usize) -> bool {
        // check valid channel range
        if n == 0 || n > 32 {
            return false;
        }

        // - the 0th bit represents channel 1
        n -= 1;

        (&self.inner & num_bigint::BigUint::from(1u32 << n)) != 0u32.into()
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct PersistentState {
    /// timezone offset from UTC in minutes
    #[serde(default)]
    // force invalidating timezone cache
    #[serde(rename = "time_offset_2")]
    pub time_offset: Option<i32>,
    /// connection status between gateway and device
    #[serde(
        deserialize_with = "deserialize_numderive",
        serialize_with = "serialize_numderive"
    )]
    pub connection_status: ConnectionStatus,
    /// last time the device went from on- to offline or vice versa, used as
    /// the timestamp for the online property of the connection status.
    /// this is not supported by shadoway
    #[serde(default = "std::time::SystemTime::now")]
    pub connection_status_last_transition: std::time::SystemTime,
    /// current state in the FOTA process
    #[serde(default)]
    pub firmware_update_state: lwm2m::FirmwareUpdateState,
    /// result of the last update
    #[serde(default)]
    pub firmware_update_result: lwm2m::FirmwareUpdateResult,
    /// result of the last update
    #[serde(default)]
    pub firmware_update_pkg_version: String,
    #[serde(default)]
    pub status: Option<lsdl::RawStatus>,
    #[serde(skip_serializing, default = "std::time::SystemTime::now")]
    pub modified: std::time::SystemTime,
}

impl PersistentState {
    pub fn new(connection_status: ConnectionStatus) -> Self {
        Self {
            time_offset: None,
            connection_status,
            connection_status_last_transition: std::time::SystemTime::now(),
            firmware_update_state: lwm2m::FirmwareUpdateState::Idle,
            firmware_update_result: lwm2m::FirmwareUpdateResult::Initial,
            firmware_update_pkg_version: "".to_string(),
            status: None,
            modified: std::time::SystemTime::now(),
        }
    }

    pub fn load<P: AsRef<std::path::Path>>(devdir: P) -> Result<Self, Error> {
        let devdir = devdir.as_ref();
        let path = devdir.join("Persistent.json");
        let file = std::fs::File::open(&path)?;
        let mut persistent: Self = serde_json::from_reader(file)?;

        persistent.modified = std::fs::metadata(path)?.modified()?;

        // fix inconsistent state due to uncontrolled termination (SG-18427)
        if persistent.firmware_update_state == lwm2m::FirmwareUpdateState::Downloading {
            persistent.firmware_update_state = lwm2m::FirmwareUpdateState::Idle;
            persistent.firmware_update_result = lwm2m::FirmwareUpdateResult::Success;
            persistent.firmware_update_pkg_version = "".to_string();
        }

        Ok(persistent)
    }

    pub async fn save<P: AsRef<std::path::Path>>(&mut self, devdir: P) -> Result<(), Error> {
        tokio::fs::create_dir_all(devdir.as_ref()).await?;

        let path = devdir.as_ref().join("Persistent.json");
        save_json(&path, self).await?;
        self.modified = std::fs::metadata(path)?.modified()?;

        Ok(())
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct DeviceDescription {
    /// device IPv6 address
    pub address: std::net::IpAddr,
    /// device manufacturer. Specification table 2.20
    pub manufacturer: lsdl::Manufacturer,
    /// application version
    pub version_app: String,
    /// hardware version
    pub version_hw: String,
    /// bootloader version
    pub version_boot: String,
    /// stack version
    pub version_stack: String,
    /// The communication protocol that the device uses.
    /// Specification table 2.18
    pub protocol: lsdl::Protocol,
    /// manufacturer specific device type ID
    #[serde(rename = "type")]
    pub type_id: u64,
    /// product type
    pub product: Product,
    /// true if the device is included
    pub included: bool,
    /// device name
    pub name: String,
    /// SGTIN
    #[debug(ignore)]
    #[serde(
        deserialize_with = "hex::serde::deserialize",
        serialize_with = "hex::serde::serialize_upper"
    )]
    serialid: Vec<u8>,
    /// device communication mode. Specification table 2.19
    #[serde(
        deserialize_with = "deserialize_numderive",
        serialize_with = "serialize_numderive"
    )]
    pub radio_mode: lsdl::RadioMode,
    /// The radio channel that the device listens on for wake-up frames.
    pub wakeup_channel: crate::comm::WakeupChannel,
    /// The current channel map having 4 channels defined as bit mask, see
    /// Section 3.2.6.2.5.
    ///
    /// I couldn't find any good documentation of which channel map this is, so
    /// I'm gonna assume it's `Normal channel map' because that makes the most
    /// sense. According to Marcell Mueller this represents the channels, that
    /// the device uses for normal communication(like LSDL).
    #[debug(ignore)]
    #[serde(
        deserialize_with = "hex::serde::deserialize",
        serialize_with = "hex::serde::serialize_upper"
    )]
    channel_map: Vec<u8>,
    uptime: Option<u64>,

    // The fields that follow don't come from the device description but were
    // written by shadoway.
    /// legacy value used to filter non-functional devices
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub inclusion_status: Option<InclusionStatus>,

    #[serde(skip_serializing, default = "std::time::SystemTime::now")]
    pub modified: std::time::SystemTime,
}

impl DeviceDescription {
    pub fn new<P: AsRef<[lsdl::Property]>>(
        address: std::net::IpAddr,
        properties: P,
    ) -> Result<Self, Error> {
        let mut manufacturer = Err(anyhow!("missing manufacturer"));
        let mut version_app = Err(anyhow!("missing application version"));
        let mut version_hw = Err(anyhow!("missing hardware version"));
        let mut version_boot = Err(anyhow!("missing bootloader version"));
        let mut version_stack = Err(anyhow!("missing stack version"));
        let mut protocol = Err(anyhow!("missing protocol"));
        let mut type_id = Err(anyhow!("missing type ID"));
        let mut product = Err(anyhow!("missing product"));
        let mut included = Err(anyhow!("missing included flag"));
        let mut name = Err(anyhow!("missing name"));
        let mut serialid = Err(anyhow!("missing serialid"));
        let mut radio_mode = Err(anyhow!("missing radio mode"));
        let mut wakeup_channel = Err(anyhow!("missing wakeup channel"));
        let mut channel_map = Err(anyhow!("missing channel map"));
        let mut uptime = None;

        for property in properties.as_ref() {
            let id = match property.id() {
                Some(v) => v,
                None => continue,
            };

            // NOTE: we check errors immediately so we don't ignore them in
            //       case an id occurs multiple times with only the last one
            //       being valid.
            match &id {
                lsdl::PropertyId::Manufacturer => {
                    manufacturer = Ok(property
                        .number()
                        .map_or(None, lsdl::Manufacturer::from_u64)
                        .ok_or_else(|| anyhow!("manufacturer is not a number: {:?}", property))?)
                }
                lsdl::PropertyId::ApplicationVersion => version_app = Ok(property.str()?),
                lsdl::PropertyId::HardwareVersion => version_hw = Ok(property.str()?),
                lsdl::PropertyId::BootloaderVersion => version_boot = Ok(property.str()?),
                lsdl::PropertyId::StackVersion => version_stack = Ok(property.str()?),
                lsdl::PropertyId::Protocol => {
                    protocol = Ok(property
                        .number()
                        .map_or(None, lsdl::Protocol::from_u64)
                        .ok_or_else(|| anyhow!("protocol is not a number: {:?}", property))?)
                }
                lsdl::PropertyId::Type => type_id = Ok(property.number()?),
                lsdl::PropertyId::Product => {
                    product = Ok(match property.number()? {
                        1 => Product::Gateway,
                        _ => Product::Device,
                    })
                }
                lsdl::PropertyId::Included => included = Ok(property.number()? == 1),
                lsdl::PropertyId::Name => name = Ok(property.str()?),
                lsdl::PropertyId::Sgtin => serialid = Ok(property.hex()?),
                lsdl::PropertyId::RadioMode => {
                    radio_mode = Ok(property
                        .number()
                        .map_or(None, lsdl::RadioMode::from_u64)
                        .ok_or_else(|| anyhow!("radio mode is not a number: {:?}", property))?)
                }
                lsdl::PropertyId::WakeupChannel => wakeup_channel = Ok(property.number()?),
                lsdl::PropertyId::ChannelMap => channel_map = Ok(property.hex()?),
                lsdl::PropertyId::Uptime => uptime = Some(property.number()?),

                // these values will never be used, silently ignore
                lsdl::PropertyId::ChannelScanTime
                | lsdl::PropertyId::DiversityMode
                | lsdl::PropertyId::Ipv6Address
                | lsdl::PropertyId::MacAddress
                | lsdl::PropertyId::TxPower
                | lsdl::PropertyId::WakeupInterval
                | lsdl::PropertyId::WakeupNow
                | lsdl::PropertyId::WakeupOffset => {}
            }
        }

        let included = included?;
        let product = product?;
        let type_id = type_id?;
        let serialid = serialid?;

        if serialid.len() != 12 {
            anyhow::bail!("SGTIN with unsupported length: {}", serialid.len());
        }

        Ok(Self {
            address,
            manufacturer: manufacturer?,
            version_app: version_app?.to_string(),
            version_hw: version_hw?.to_string(),
            version_boot: version_boot?.to_string(),
            version_stack: version_stack?.to_string(),
            protocol: protocol?,
            type_id,
            product,
            included,
            inclusion_status: None,
            name: name?.to_string(),
            serialid,
            radio_mode: radio_mode?,
            wakeup_channel: crate::comm::WakeupChannel::allocate_unchecked(wakeup_channel?)?,
            channel_map: channel_map?,
            uptime,
            // timestamp of the associated file's last modification
            modified: std::time::SystemTime::now(),
        })
    }

    pub fn mac_address(&self) -> Option<[u8; 6]> {
        let ip6 = match &self.address {
            std::net::IpAddr::V6(v6) => v6,
            _ => return None,
        };

        let octets = ip6.octets();
        Some([
            octets[10], octets[11], octets[12], octets[13], octets[14], octets[15],
        ])
    }

    pub fn sgtin(&self) -> num_bigint::BigUint {
        let mut sgtin = num_bigint::BigUint::from_bytes_be(self.serialid.as_ref());
        let startmask_not = !0b11u64;

        // If it doesn't fit into 64 bits, there's no way that it's a valid SGTIN
        // because it's longer than 96bits. Using 0 as a representative invalid SGTIN
        // so the checks that follow will get us in the fixup path
        // Also, the reason why we convert it in first place is that it makes the `match` below way
        // more efficient than it would be with heap-allocated bigints
        //
        // bits 36 and 37 belong to the serial but we read them too so we can
        // compare against the usual hex representation
        let start = ((&sgtin) >> 36u64).to_u64().unwrap_or(0) & startmask_not;
        tracing::trace!("SGTIN start: {:X}", start);

        if matches!(self.product, Product::Gateway) {
            if !matches!(start, 0x3034f8ee90155b4 | 0x3034f8ee902d438) {
                tracing::info!("Gateway radio module has unexpected SGTIN {:X}", sgtin);
            }

            // no need to patch Gateway SGTINs as we assume that they are correct. Cloud Adapter
            // (more specifically Gateway Device Server) uses SGTIN as stored in U-Boot env. So
            // we could have a mismatch in the logs.
            return sgtin;
        }

        let is_valid = matches!(
            start,
            0x3034F8EE90126D4
                | 0x3034F8EE9012674
                | 0x3034F8EE9006008
                | 0x3034F8EE9016028
                | 0x3034F8EE901EE94
                | 0x3034F8EE902273C
                | 0x3034F8EE9012980
                | 0x3035C33A881CFF8
                | 0x3035C33A8834B90
        );
        if !is_valid {
            let original_sgtin = sgtin.clone();

            let replacement: u64 = match self.type_id {
                1 => 0x3014F8EE90126D4,
                2 => 0x3014F8EE9012674,
                3 => 0x3014F8EE9006008,
                4 => 0x3014F8EE9016028,
                6 => 0x3014F8EE901EE94,
                7 => 0x3014F8EE902273C,
                8 => 0x3014F8EE9012980,
                9 => 0x3015C33A881CFF8,
                10 => 0x3015C33A8834B90,
                _ => return sgtin,
            };
            let value = num_bigint::BigUint::from(replacement & startmask_not) << 36;

            // NOTE: BigUint doesn't implement `Not` or `&= u64`
            sgtin &= num_bigint::BigUint::from(crate::traits::bitmask(38));
            sgtin |= value;

            tracing::debug!(
                "Patched invalid SGTIN {:X} with {:X} (start: {:X})",
                original_sgtin,
                sgtin,
                start
            );
        }

        sgtin
    }

    pub fn device_type(&self) -> &'static str {
        match self.sgtin().item_reference() {
            ITEM_REFERENCE_WATER_CONTROL => "Water Control",
            18845 | ITEM_REFERENCE_SENSOR2 => "Sensor",
            ITEM_REFERENCE_CBT2 | ITEM_REFERENCE_CBTG | ITEM_REFERENCE_CBTL => "Robotics Lawnmower",
            22538 => "Automatic Home and Garden Pump",
            ITEM_REFERENCE_IRRIGATION_CONTROL => "Irrigation Control",
            35279 => "Power Adapter",
            21869 | 46350 => "Gateway",
            _ => "unknown",
        }
    }

    pub fn is_cbt2(&self) -> bool {
        self.sgtin().item_reference() == ITEM_REFERENCE_CBT2
    }

    pub fn is_cbtl(&self) -> bool {
        self.sgtin().item_reference() == ITEM_REFERENCE_CBTL
    }

    pub fn is_lawnmower(&self) -> bool {
        matches!(
            self.sgtin().item_reference(),
            ITEM_REFERENCE_CBT2 | ITEM_REFERENCE_CBTG | ITEM_REFERENCE_CBTL
        )
    }

    pub fn is_watering_device(&self) -> bool {
        matches!(
            self.sgtin().item_reference(),
            ITEM_REFERENCE_WATER_CONTROL | ITEM_REFERENCE_IRRIGATION_CONTROL
        )
    }

    pub fn is_sensor2(&self) -> bool {
        self.sgtin().item_reference() == ITEM_REFERENCE_SENSOR2
    }

    pub fn bnw_id(&self) -> String {
        hex::encode_upper(self.sgtin().to_bytes_be())
    }

    pub fn is_radio_module(&self) -> bool {
        matches!(self.product, Product::Gateway)
    }

    /// returns `true` if this device should be persisted
    pub fn loadsave_enabled(&self) -> bool {
        self.included
            && self.inclusion_status.unwrap_or(InclusionStatus::Included)
                == InclusionStatus::Included
    }

    pub fn channel_map(&self) -> ChannelMap {
        ChannelMap::from_bytes_be(&self.channel_map)
    }

    /// NOTE: We are using the timestamp of the file to figure out when we
    /// received the information from the device. Since we're also storing our
    /// own data in that same file and update it when it changes that's not
    /// actually true.
    pub async fn save<P: AsRef<std::path::Path>>(&mut self, devdir: P) -> Result<(), Error> {
        tokio::fs::create_dir_all(devdir.as_ref()).await?;

        let path = devdir
            .as_ref()
            .join(format!("Device_descriptionID_{}.json", self.address));
        save_json(&path, self).await?;

        // Storing this date as a timestamp for device lwm2m messages. it's updated
        // whenever a device description is received. The filesystem's metadata
        // will also be updated whenever that file changes, so should there be writes
        // to the file, the timestamp would not reflect the actual time that
        // value was retrieved. As the timestamps are not currently used that
        // is ok for now.
        // The timestamp is loaded after saving the file so it does not change
        // when loading the description from file after a restart of lemonbeatd.
        self.modified = std::fs::metadata(path)?.modified()?;
        Ok(())
    }

    pub fn load<P: AsRef<std::path::Path>>(devdir: P) -> Result<Self, Error> {
        let devdir = devdir.as_ref();
        let path = devdir
            .join(
                devdir
                    .file_name()
                    .ok_or_else(|| anyhow!("devdir has no filename"))?,
            )
            .with_extension("json");
        let file = std::fs::File::open(&path)?;
        let mut description: DeviceDescription = serde_json::from_reader(file)?;
        description.modified = std::fs::metadata(path)?.modified()?;

        Ok(description)
    }

    /// Parses stack version string. If parsing fails, we return the default version 1.0.0
    pub fn version_stack_parsed(&self) -> version_compare::Version<'_> {
        version_compare::Version::from(&self.version_stack).unwrap_or_else(|| {
            version_compare::Version::from_parts(
                "",
                vec![
                    version_compare::Part::Number(1),
                    version_compare::Part::Number(0),
                    version_compare::Part::Number(0),
                ],
            )
        })
    }
}

/// Information about the current and the maximum number of timers, actions,
/// etc. for a specific device.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct MemoryInformation {
    /// The service ID, see Table 2.14
    pub id: u32,
    /// The maximum number of services that the device supports
    pub count: u32,
    /// The number of free memory slots
    pub free_count: u32,
}

impl MemoryInformation {
    pub fn new(info: &lsdl::xsd::memory_information::memoryInformationType) -> Self {
        Self {
            id: info.memory_id,
            count: info.count,
            free_count: info.free_count,
        }
    }

    pub fn id(&self) -> Option<lsdl::MemoryId> {
        lsdl::MemoryId::from_u32(self.id)
    }

    pub fn num_allocated(&self) -> u32 {
        self.count.saturating_sub(self.free_count)
    }
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "format")]
pub enum ValueFormat {
    #[serde(rename = "number")]
    Number {
        unit: String,
        min: f64,
        max: f64,
        step: f64,
    },
    #[serde(rename = "hexbin")]
    Binary {
        #[serde(rename = "max", deserialize_with = "deserialize_floatu32")]
        /// NOTE: shadoway always sets this to `0`
        max_length: u32,
    },
    #[serde(rename = "string")]
    String {
        #[serde(rename = "max", deserialize_with = "deserialize_floatu32")]
        max_length: u32,
    },
}

impl ValueFormat {
    /// validate the data type inside `value` matches this format
    pub fn validate_value_format(&self, value: &Value) -> Result<(), Error> {
        match (self, &value.data) {
            (
                Self::Number {
                    unit: _,
                    min: _,
                    max: _,
                    step: _,
                },
                ValueData::Number(_),
            ) => Ok(()),
            (Self::Binary { max_length: _ }, ValueData::Binary(_)) => Ok(()),
            (Self::String { max_length: _ }, ValueData::String(_)) => Ok(()),
            _ => Err(anyhow!("value doesn't match the value description")),
        }
    }
}

/// Description of each possible value for a specific device, e.g., type, mode,
/// etc.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ValueDescription {
    /// ID of the description value
    pub id: u32,
    /// Possible interaction with value.
    /// This is called `mode` in the lemonbeat specification.
    pub permission: lsdl::Permission,
    /// The name of the device
    #[serde(deserialize_with = "deserialize_optstr")]
    pub name: Option<String>,
    /// value type, see Table 2.25
    #[serde(rename = "type")]
    pub type_id: lsdl::ValueType,
    #[serde(flatten)]
    pub format: ValueFormat,
    /// Specifies if the value is persistent during a power cycling
    ///
    /// NOTE: shadoway stores this as an integer
    #[serde(deserialize_with = "deserialize_intbool")]
    pub persistent: bool,
    /// Specifies if the value is a virtual value
    pub virtual_value: bool,
}

impl ValueDescription {
    pub fn new(
        description: &lsdl::xsd::value_description::valueDescriptionType,
    ) -> Result<Self, Error> {
        let persistent = match &description.persistent {
            0 => false,
            1 => true,
            other => anyhow::bail!("invalid value for `persistent`: {}", other),
        };
        let virtual_value = match &description.virtual_.unwrap_or(0) {
            0 => false,
            1 => true,
            other => anyhow::bail!("invalid value for `virtual_`: {}", other),
        };

        if description.inner.len() != 1 {
            anyhow::bail!(
                "invalid number of value description types: {}",
                description.inner.len()
            );
        }
        let format = match &description.inner[0] {
            lsdl::xsd::value_description::valueDescriptionTypeInner::number_format(f) => {
                ValueFormat::Number {
                    unit: f.unit.to_string(),
                    min: f.min,
                    max: f.max,
                    step: f.step,
                }
            }
            lsdl::xsd::value_description::valueDescriptionTypeInner::string_format(f) => {
                ValueFormat::String {
                    max_length: f.max_length,
                }
            }
            lsdl::xsd::value_description::valueDescriptionTypeInner::hexBinary_format(f) => {
                ValueFormat::Binary {
                    max_length: f.max_length,
                }
            }
        };

        Ok(Self {
            id: description.value_id,
            permission: lsdl::Permission::from_u32(description.mode)
                .ok_or_else(|| anyhow!("unsupported value for `mode`: {}", description.mode))?,
            name: description.name.clone(),
            type_id: lsdl::ValueType::from_u32(description.type_id)
                .ok_or_else(|| anyhow!("unsupported value for `type`: {}", description.type_id))?,
            format,
            persistent,
            virtual_value,
        })
    }

    pub async fn save<P: AsRef<std::path::Path>>(&self, dir: P) -> Result<(), Error> {
        let path = dir
            .as_ref()
            .join(format!("Value_description_{}.json", self.id));
        save_json(path, self).await
    }

    pub fn load<P: AsRef<std::path::Path>>(path: P) -> Result<Self, Error> {
        let file = std::fs::File::open(path)?;
        Ok(serde_json::from_reader(file)?)
    }

    fn makesetmsg_valuesettype(
        &self,
        vst: lsdl::xsd::value::valueSetType,
    ) -> Result<lsdl::xsd::value::deviceTypeInner, Error> {
        if !matches!(
            self.permission,
            lsdl::Permission::ReadWrite | lsdl::Permission::WriteOnly
        ) {
            anyhow::bail!("can't set a readonly value");
        }

        Ok(lsdl::xsd::value::deviceTypeInner::value_set(vst))
    }

    #[allow(dead_code)]
    pub fn makesetmsg_string(&self, s: &str) -> Result<lsdl::xsd::value::deviceTypeInner, Error> {
        match &self.format {
            ValueFormat::String { max_length } => {
                let length: u32 = s
                    .len()
                    .try_into()
                    .context("string length doesn't fit into u32")?;
                if length > *max_length {
                    anyhow::bail!("max_length:{} got:{}", max_length, length);
                }
            }
            other => anyhow::bail!("can't set `{:?}`-value", other),
        }

        self.makesetmsg_valuesettype(lsdl::xsd::value::valueSetType {
            value_id: self.id,
            timestamp: 0,
            number: None,
            hexBinary: None,
            string: Some(s.to_string()),
        })
    }

    #[allow(dead_code)]
    pub fn makesetmsg_number(
        &self,
        number: f64,
    ) -> Result<lsdl::xsd::value::deviceTypeInner, Error> {
        match &self.format {
            ValueFormat::Number { .. } => {
                // NOTE we don't check anything because that's hard to do with
                //      floats and we can let the device do that.
            }
            other => anyhow::bail!("can't set `{:?}`-value", other),
        }

        self.makesetmsg_valuesettype(lsdl::xsd::value::valueSetType {
            value_id: self.id,
            timestamp: 0,
            number: Some(number),
            hexBinary: None,
            string: None,
        })
    }

    #[allow(dead_code)]
    pub fn makesetmsg_binary(
        &self,
        binary: &[u8],
    ) -> Result<lsdl::xsd::value::deviceTypeInner, Error> {
        match &self.format {
            ValueFormat::Binary { max_length } => {
                let length: u32 = binary
                    .len()
                    .try_into()
                    .context("slice length doesn't fit into u32")?;
                // NOTE: Shadoway always stored 0 instead of the length so we
                //       can't verify it if the device was not included by us.
                //       Assuming that no device actually uses 0, we use it as
                //       an indicator that the information is missing.
                if *max_length > 0 && length > *max_length {
                    anyhow::bail!("max_length:{} got:{}", max_length, length);
                }
            }
            other => anyhow::bail!("can't set `{:?}`-value", other),
        }

        self.makesetmsg_valuesettype(lsdl::xsd::value::valueSetType {
            value_id: self.id,
            timestamp: 0,
            number: None,
            hexBinary: Some(hex::encode(binary)),
            string: None,
        })
    }
}

#[derive(Clone, PartialEq)]
pub enum ValueData {
    Number(f64),
    String(String),
    Binary(Vec<u8>),
}

impl std::fmt::Debug for ValueData {
    /// manual implementation which prints `Binary` in hex
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ValueData::Number(n) => std::fmt::Formatter::debug_tuple(f, "Number")
                .field(&n)
                .finish(),
            ValueData::String(s) => std::fmt::Formatter::debug_tuple(f, "String")
                .field(&s)
                .finish(),
            ValueData::Binary(v) => std::fmt::Formatter::debug_tuple(f, "Binary")
                .field(&hex::encode(v))
                .finish(),
        }
    }
}

impl std::fmt::Display for ValueData {
    /// manual implementation to meet troubleshooters needs of no internal type information
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ValueData::Number(n) => std::fmt::Display::fmt(n, f),
            ValueData::String(s) => std::fmt::Display::fmt(s, f),
            ValueData::Binary(v) => write!(f, "0x{}", hex::encode(v)),
        }
    }
}

fn serialize_valuedata<S>(data: &ValueData, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match data {
        ValueData::Number(n) => serializer.serialize_str(&format!("{n}")),
        ValueData::String(s) => serializer.serialize_str(s),
        ValueData::Binary(v) => hex::serde::serialize_upper(v, serializer),
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Value {
    pub id: u32,
    pub timestamp: u64,
    /// type and data
    ///
    /// shadoway stores this as a string without any information about the type.
    /// - during serialization we can simply convert it to a string.
    /// - during deserialization we need the value_descriptor to parse it
    ///   correctly.
    #[serde(rename = "value", serialize_with = "serialize_valuedata")]
    data: ValueData,
}

impl Value {
    pub fn new(value: &lsdl::xsd::value::valueReportType) -> Result<Self, Error> {
        let data = if let Some(number) = value.number {
            if value.string.is_some() || value.hexBinary.is_some() {
                anyhow::bail!("multiple types in value");
            }

            ValueData::Number(number)
        } else if let Some(string) = &value.string {
            if value.number.is_some() || value.hexBinary.is_some() {
                anyhow::bail!("multiple types in value");
            }

            ValueData::String(string.to_string())
        } else if let Some(hex_binary) = &value.hexBinary {
            if value.number.is_some() || value.string.is_some() {
                anyhow::bail!("multiple types in value");
            }

            ValueData::Binary(hex::decode(hex_binary)?)
        } else {
            anyhow::bail!("no types in value");
        };

        Ok(Self {
            id: value.value_id.ok_or_else(|| anyhow!("missing value_id"))?,
            timestamp: value.timestamp,
            data,
        })
    }

    /// save value to disk
    ///
    /// shadoway does save two files per value. suffixed with `r` for read and
    /// `w` for write.
    /// Since we don't care about what was written and only care about the
    /// actual device state we only ever read/write the `r` variant.
    pub async fn save<P: AsRef<std::path::Path>>(&self, dir: P) -> Result<(), Error> {
        let path = dir.as_ref().join(format!("Value_{}r.json", self.id));
        save_json(path, self).await
    }

    /// load value from disk
    ///
    /// we need to implement [serde::Deserialize] manually so we can parse the
    /// string in `value` based on type information stored in the value
    /// description.
    ///
    /// See [Value::save] for more information about read/write value variants.
    pub fn load<P: AsRef<std::path::Path>>(
        path: P,
        descriptions: &[ValueDescription],
    ) -> Result<Self, Error> {
        struct Visitor<'a> {
            descriptions: &'a [ValueDescription],
        }

        impl<'de> serde::de::Visitor<'de> for Visitor<'de> {
            type Value = Value;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "a `Value` struct")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut id = None;
                let mut timestamp = None;
                let mut value = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "id" => {
                            if id.is_some() {
                                return Err(serde::de::Error::duplicate_field("id"));
                            }
                            id = Some(map.next_value::<u32>()?)
                        }
                        "timestamp" => {
                            if timestamp.is_some() {
                                return Err(serde::de::Error::duplicate_field("timestamp"));
                            }
                            timestamp = Some(map.next_value::<u64>()?)
                        }
                        "value" => {
                            if value.is_some() {
                                return Err(serde::de::Error::duplicate_field("value"));
                            }
                            value = Some(map.next_value::<String>()?)
                        }
                        _ => {
                            let _ = map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }

                let id = id.ok_or_else(|| serde::de::Error::missing_field("id"))?;
                let timestamp =
                    timestamp.ok_or_else(|| serde::de::Error::missing_field("timestamp"))?;
                let value = value.ok_or_else(|| serde::de::Error::missing_field("value"))?;

                let description =
                    self.descriptions
                        .iter()
                        .find(|vd| vd.id == id)
                        .ok_or_else(|| {
                            serde::de::Error::custom(format!(
                                "can't find value-description for value `{id}`"
                            ))
                        })?;

                // Note: parsing values will fall-back to un-initialized value in case it fails,
                //       this should be self healing once the device pushes out a value update.
                //       Considered alternatives:
                //       1) let loading of device fail and ignore device (user to re-include)
                //       2) re-run post-inclusion to get proper values: optimal but too much work
                //          and complexity - only found two affected devices in sample > 5'000.
                let data = match &description.format {
                    ValueFormat::Number {
                        unit: _,
                        min: _,
                        max: _,
                        step: _,
                    } => ValueData::Number(value.parse().unwrap_or_else(|_| {
                        tracing::warn!(
                            "Failed to load number value {:?} - continue as uninitialised",
                            description.name
                        );
                        f64::NAN
                    })),
                    ValueFormat::String { max_length: _ } => ValueData::String(value),
                    ValueFormat::Binary { max_length: _ } => {
                        ValueData::Binary(hex::decode(&value).unwrap_or_else(|_| {
                            tracing::warn!(
                                "Failed to load binary value {:?} - continue as uninitialised",
                                description.name
                            );
                            vec![]
                        }))
                    }
                };

                Ok(Value {
                    id,
                    timestamp,
                    data,
                })
            }
        }

        let file = std::fs::File::open(path)?;
        let mut d = serde_json::Deserializer::new(serde_json::de::IoRead::new(file));
        Ok(d.deserialize_map(Visitor { descriptions })?)
    }

    #[allow(dead_code)]
    pub fn as_number(&self) -> Result<&f64, Error> {
        match &self.data {
            ValueData::Number(n) => Ok(n),
            _ => Err(anyhow!("not a number")),
        }
    }

    #[allow(dead_code)]
    pub fn as_string(&self) -> Result<&str, Error> {
        match &self.data {
            ValueData::String(s) => Ok(s),
            _ => Err(anyhow!("not a string")),
        }
    }

    #[allow(dead_code)]
    pub fn as_binary(&self) -> Result<&[u8], Error> {
        match &self.data {
            ValueData::Binary(b) => Ok(b),
            _ => Err(anyhow!("not a binary")),
        }
    }

    pub fn data(&self) -> &ValueData {
        &self.data
    }

    #[allow(dead_code)]
    pub fn set_number(&mut self, number: f64) -> Result<(), Error> {
        match &mut self.data {
            ValueData::Number(n) => {
                *n = number;
                Ok(())
            }
            _ => Err(anyhow!("not a number")),
        }
    }

    #[allow(dead_code)]
    pub fn set_string(&mut self, s: &str) -> Result<(), Error> {
        match &mut self.data {
            ValueData::String(current_str) => {
                *current_str = s.to_string();
                Ok(())
            }
            _ => Err(anyhow!("not a string")),
        }
    }

    #[allow(dead_code)]
    pub fn set_binary(&mut self, binary: &[u8]) -> Result<(), Error> {
        match &mut self.data {
            ValueData::Binary(b) => {
                b.clear();
                b.extend(binary);
                Ok(())
            }
            _ => Err(anyhow!("not a binary")),
        }
    }

    pub fn same_type(&self, other: &Value) -> bool {
        matches!(
            (&self.data, &other.data),
            (ValueData::Number(_), ValueData::Number(_))
                | (ValueData::String(_), ValueData::String(_))
                | (ValueData::Binary(_), ValueData::Binary(_))
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::create_version;

    #[test_log::test]
    fn load_hexbin_value_description() {
        let description =
            ValueDescription::load("./test_data/storage/Value_description_hexbin.json").unwrap();
        assert_eq!(
            description,
            ValueDescription {
                id: 23,
                permission: lsdl::Permission::ReadOnly,
                name: Some("schedule_state".to_string()),
                type_id: lsdl::ValueType::GeneralPurpose,
                format: ValueFormat::Binary { max_length: 0 },
                persistent: false,
                virtual_value: false,
            }
        );
    }

    #[test_log::test(tokio::test)]
    async fn save_hexbin_value_description() {
        let expected = tokio::task::spawn_blocking(move || {
            ValueDescription::load("./test_data/storage/Value_description_hexbin.json").unwrap()
        })
        .await
        .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        expected.save(&tmp).await.unwrap();

        let description = tokio::task::spawn_blocking(move || {
            ValueDescription::load(tmp.path().join("Value_description_23.json")).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(description, expected);
    }

    #[test_log::test]
    fn load_value_unknown_attribute() {
        let description =
            ValueDescription::load("./test_data/storage/Value_description_unknown_attribute.json")
                .unwrap();

        let value = Value::load(
            "./test_data/storage/Value_unknown_attribute.json",
            &[description],
        )
        .unwrap();

        assert_eq!(
            value,
            Value {
                id: 37,
                timestamp: 1629142915419,
                data: ValueData::Binary(
                    hex::decode(concat!(
                        "56352052686F646F64656E6472656E",
                        "0000000000000000000000000000000000000000",
                        "0000000000000000000000000000000000000000",
                        "000000000000000000"
                    ))
                    .unwrap()
                ),
            }
        );
    }

    #[test_log::test]
    fn channel_map() {
        let description = DeviceDescription::load(
            "./test_data/storage/sample_work/Device_descriptionID_fc00::6:94bb:ae16:8bb0",
        )
        .unwrap();
        let channel_map = description.channel_map();

        for i in 0..=64 {
            log::debug!("Test channel {}", i);

            assert_eq!(channel_map.contains(i), matches!(i, 3 | 12 | 20 | 29))
        }
    }

    #[test_log::test]
    fn sgtin_api() {
        let description = DeviceDescription::load(
            "./test_data/storage/sample_work/Device_descriptionID_fc00::6:94bb:ae16:8bb0",
        )
        .unwrap();

        let sgtin = description.sgtin();
        assert_eq!(
            sgtin,
            num_bigint::BigUint::from_bytes_be(&hex::decode("3034F8EE901EE94000002FDB").unwrap())
        );
        assert_eq!(description.device_type(), "Irrigation Control");
        assert_eq!(description.bnw_id(), "3034F8EE901EE94000002FDB");
        assert_eq!(sgtin.header(), 3);
        assert_eq!(sgtin.item_reference(), 31653);
        assert_eq!(sgtin.serial(), 0x2FDB);
    }

    #[rstest::rstest]
    #[case("3034F8EE902D43800002E02B", "3034F8EE902D43800002E02B", 1, false)]
    #[case("3034F8EE90126D4000010379", "3034F8EE90126D4000010379", 1, true)]
    #[case("0000000000126D6F98E18BD0", "3014F8EE90126D6F98E18BD0", 1, true)]
    #[case("3034F8EE901267400000B3E2", "3034F8EE901267400000B3E2", 2, true)]
    #[case("3034F8EE900000281045740D", "3014F8EE901267681045740D", 2, true)]
    #[case("3034F8EE90060082EFC8F554", "3034F8EE90060082EFC8F554", 3, true)]
    #[case("3034F8EE900000233CC2D326", "3014F8EE900600A33CC2D326", 3, true)]
    #[case("3034F8EE901602A8A93EEE22", "3034F8EE901602A8A93EEE22", 4, true)]
    #[case("3034F8EE9000002585C3F73B", "3014F8EE901602A585C3F73B", 4, true)]
    #[case("000000000000FEF29A26E4E6", "3014F8EE901EE9729A26E4E6", 6, true)]
    #[case("3034F8EE901EE94000008771", "3034F8EE901EE94000008771", 6, true)]
    #[case("3034F8EE902273E1CED2B39C", "3034F8EE902273E1CED2B39C", 7, true)]
    #[case("3034F8EE9000002183F62406", "3014F8EE902273E183F62406", 7, true)]
    #[case("3034F8EE9012983B9BD09539", "3034F8EE9012983B9BD09539", 8, true)]
    #[case("0000000000129827B9C7475F", "3014F8EE90129827B9C7475F", 8, true)]
    #[case("3035C33A881CFFBBE95988E3", "3035C33A881CFFBBE95988E3", 9, true)]
    #[case("3035C33A8800002D43A6508A", "3015C33A881CFFAD43A6508A", 9, true)]
    #[case("03035C33A8834B92FFF3D1CF", "3015C33A8834B912FFF3D1CF", 10, true)]
    #[case("3035C33A8834B925750D9CD0", "3035C33A8834B925750D9CD0", 10, true)]
    #[test_log::test]
    fn sgtin_translation(
        #[case] input: &str,
        #[case] output_exp: &str,
        #[case] type_id: u64,
        #[case] is_device: bool,
    ) {
        let output_exp = num_bigint::BigUint::from_bytes_be(&hex::decode(output_exp).unwrap());

        let mut description = DeviceDescription::load(
            "./test_data/storage/sample_work/Device_descriptionID_fc00::6:94bb:ae16:8bb0",
        )
        .unwrap();

        description.serialid = hex::decode(input).unwrap();
        description.type_id = type_id;
        description.product = if is_device {
            Product::Device
        } else {
            Product::Gateway
        };

        let sgtin = description.sgtin();

        tracing::trace!(
            "In : {:096b}",
            num_bigint::BigUint::from_bytes_be(&description.serialid)
        );
        tracing::trace!("Out: {:096b}", sgtin);
        tracing::trace!("Exp: {:096b}", output_exp);

        assert_eq!(sgtin, output_exp);
    }

    #[test_log::test]
    fn stack_version_handling() {
        let description = DeviceDescription::load(
            "./test_data/storage/sample_work/Device_descriptionID_fc00::6:94bb:ae16:8bb0",
        )
        .unwrap();

        assert_eq!(description.version_stack_parsed(), create_version(1, 5, 3));
        assert!(create_version(0, 10, 10) < description.version_stack_parsed());

        let description = DeviceDescription::load(
            "./test_data/storage/sample_work/Device_descriptionID_fc00::6:94bb:ae2b:1ffa",
        )
        .unwrap();

        assert_eq!(description.version_stack_parsed(), create_version(1, 0, 0));
    }
}
