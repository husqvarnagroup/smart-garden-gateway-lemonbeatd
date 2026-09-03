// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Implement traits and functions required for the LWM2M API

use crate::device::Lwm2mIdentifier as _;
use crate::Error;
use anyhow::anyhow;
use anyhow::Context as _;
use lwm2m::Endpoint;

/// Indicate what the IPC world knows about this device.
#[derive(Eq, PartialEq)]
pub(super) enum PublicationState {
    /// We've never announced the device.
    ///
    /// The world shouldn't know anything about it.
    Invisible,
    /// We've allocated and published an includable device
    Includable,
    /// We've published the full IPC device endpoint
    ///
    /// This means the device is supposed to be included and ready to be used.
    Ready,
}

impl lwm2m::Endpoint for crate::device::Device {
    fn get_object<'a>(
        &'a mut self,
        ty: lwm2m::ObjectType,
    ) -> Result<Box<dyn lwm2m::Object + 'a>, lwm2m::Error> {
        match &ty {
            lwm2m::ObjectType::Device => Ok(Box::new(lwm2m::objects::DeviceHandler::new(self))),
            lwm2m::ObjectType::DataDownload if self.description.is_cbtl() => {
                Ok(Box::new(lwm2m::objects::DataDownloadHandler::new(self)))
            }
            lwm2m::ObjectType::FirmwareUpdate => {
                Ok(Box::new(lwm2m::objects::FirmwareUpdateHandler::new(self)))
            }
            lwm2m::ObjectType::Lemonbeat => {
                Ok(Box::new(crate::device::LemonbeatHandler::new(self)))
            }
            lwm2m::ObjectType::ConnectionStatus => {
                Ok(Box::new(lwm2m::objects::ConnectionStatusHandler::new(self)))
            }
            lwm2m::ObjectType::LemonbeatStatusMessage => Ok(Box::new(
                lwm2m::objects::LemonbeatStatusMessageHandler::new(self),
            )),
            #[allow(unreachable_patterns)]
            other => Err(lwm2m::Error::Anyhow(anyhow!(
                "unsupported object type `{:?}`",
                other
            ))),
        }
    }

    fn object_list(&self) -> Vec<(lwm2m::ObjectType, usize)> {
        let mut r = vec![
            (lwm2m::ObjectType::Device, 0),
            (lwm2m::ObjectType::FirmwareUpdate, 0),
            (lwm2m::ObjectType::Lemonbeat, 0),
            (lwm2m::ObjectType::ConnectionStatus, 0),
        ];

        if self.description.is_cbtl() {
            r.push((lwm2m::ObjectType::DataDownload, 0));
        }

        if self.persistent.status.is_some() {
            r.push((lwm2m::ObjectType::LemonbeatStatusMessage, 0));
        }

        r
    }

    fn resource_instance_list(&self, ty: lwm2m::ObjectType) -> Vec<(usize, usize)> {
        match ty {
            lwm2m::ObjectType::Device => vec![
                (lwm2m::objects::DEVICE_MANUFACTURER, 0),
                (lwm2m::objects::DEVICE_MODEL_NUMBER, 0),
                (lwm2m::objects::DEVICE_SERIAL_NUMBER, 0),
                (lwm2m::objects::DEVICE_FIRMWARE_VERSION, 0),
                (lwm2m::objects::DEVICE_ERROR_CODE, 0),
                (lwm2m::objects::DEVICE_SUPPORTED_BINDING_AND_MODES, 0),
                (lwm2m::objects::DEVICE_DEVICE_TYPE, 0),
                (lwm2m::objects::DEVICE_SOFTWARE_VERSION, 0),
                (lwm2m::objects::DEVICE_HARDWARE_VERSION, 0),
                (lwm2m::objects::DEVICE_UTC_OFFSET, 0),
            ],
            lwm2m::ObjectType::DataDownload => vec![
                (lwm2m::objects::DATA_DOWNLOAD_SLOT, 0),
                (lwm2m::objects::DATA_DOWNLOAD_CHECKSUM, 0),
                (lwm2m::objects::DATA_DOWNLOAD_CONTENT_TAG, 0),
                (lwm2m::objects::DATA_DOWNLOAD_STATUS, 0),
            ],
            lwm2m::ObjectType::FirmwareUpdate => vec![
                (lwm2m::objects::FIRMWARE_UPDATE_PACKAGE, 0),
                (lwm2m::objects::FIRMWARE_UPDATE_PACKAGE_URI, 0),
                (lwm2m::objects::FIRMWARE_UPDATE_UPDATE, 0),
                (lwm2m::objects::FIRMWARE_UPDATE_STATE, 0),
                (lwm2m::objects::FIRMWARE_UPDATE_UPDATE_RESULT, 0),
                (
                    lwm2m::objects::FIRMWARE_UPDATE_FIRMWARE_UPDATE_DELIVERY_METHOD,
                    0,
                ),
                (lwm2m::objects::FIRMWARE_UPDATE_PKG_VERSION, 0),
            ],
            lwm2m::ObjectType::Lemonbeat => self
                .value_descriptions
                .iter()
                .filter_map(|vd| vd.resource_id().ok())
                .map(|rid| (rid, 0))
                .collect(),
            lwm2m::ObjectType::ConnectionStatus => {
                vec![(lwm2m::objects::CONNECTION_STATUS_ONLINE, 0)]
            }
            lwm2m::ObjectType::LemonbeatStatusMessage => vec![
                (lwm2m::objects::LEMONBEAT_STATUS_MESSAGE_TYPE, 0),
                (lwm2m::objects::LEMONBEAT_STATUS_MESSAGE_CODE, 0),
                (lwm2m::objects::LEMONBEAT_STATUS_MESSAGE_LEVEL, 0),
                (lwm2m::objects::LEMONBEAT_STATUS_MESSAGE_DATA, 0),
            ],
            _ => vec![],
        }
    }
}

impl crate::device::Device {
    /// Publish includable device through IPC.
    ///
    /// An extension to [lwm2m::PubService::publish_includable_device] which
    /// simplifies the API by generating the JSON payload from both
    /// `self` and the provided arguments.
    pub(super) async fn publish_includable_device(
        &mut self,
        inclusion_started: bool,
        inclusion_completed: bool,
        inclusion_error: i64,
        op: lwm2m::Method,
    ) -> Result<(), Error> {
        let json = lwm2m::objects::make_includable_device_json(
            Some(std::time::SystemTime::now()),
            self.description.bnw_id(),
            2,
            inclusion_started,
            inclusion_completed,
            inclusion_error,
        );

        self.lwm2m_pub_service
            .publish_includable_device(self.description.address, &json, op)
            .context("can't publish includable device")?;

        self.publication_state = match op {
            lwm2m::Method::Delete => PublicationState::Includable,
            _ => PublicationState::Invisible,
        };

        Ok(())
    }

    /// Sends a IPC deletion event for this device.
    pub(super) async fn publish_deletion(&mut self) -> Result<(), lwm2m::Error> {
        // SG-20457: check publication status?

        self.lwm2m_pub_service
            .publish_device_deletion(self.description.bnw_id())

        // SG-20457: reset publication status?
    }

    /// Publishes the whole IPC endpoint with all objects and resources.
    pub(super) async fn publish_endpoint(&mut self) -> Result<(), Error> {
        if !self.description.included {
            return Ok(());
        }

        let payload = self
            .json_endpoint()
            .await
            .context("can't generate endpoint json")?;

        self.lwm2m_pub_service
            .publish_update(self.description.bnw_id(), "".to_string(), payload)
            .context("can't publish endpoint update IPC event")?;

        self.publication_state = PublicationState::Ready;

        Ok(())
    }

    pub(super) async fn publish_device_object(&mut self) -> Result<(), Error> {
        if !self.description.included || self.publication_state != PublicationState::Ready {
            return Ok(());
        }

        let object_type = lwm2m::ObjectType::Device;

        let mut payload = serde_json::json!(self
            .serializable_object(object_type, 0)
            .await
            .context("can't serialize device object 0")?);
        payload
            .as_object_mut()
            .context("BUG: device object payload is not a json object")?
            .insert("_urn".to_string(), object_type.urn().into());

        self.lwm2m_pub_service
            .publish_update(self.description.bnw_id(), "device/0".to_string(), payload)
            .context("can't publish device update IPC event")?;

        Ok(())
    }

    /// Publishes the values of all requested resources.
    ///
    /// This will send a single multi-operation IPC-message.
    pub(super) async fn publish_resource_instances(
        &mut self,
        object_type: lwm2m::ObjectType,
        object_instance: usize,
        resources: &[(usize, usize)],
    ) -> Result<(), Error> {
        if !self.description.included || self.publication_state != PublicationState::Ready {
            return Ok(());
        }

        let object = self.get_object(object_type).context("can't find object")?;
        let mut payload = serde_json::Map::new();

        for (resource_id, resource_instance) in resources {
            payload.insert(
                object
                    .get_resource_name(*resource_id)
                    .context("can't get resource name")?
                    .to_string(),
                serde_json::json!(object
                    .read_resource(object_instance, *resource_id, *resource_instance)
                    .await
                    .context("can't read resource")?),
            );
        }

        let path = format!("{}/{}", object_type.as_str(), object_instance);
        drop(object);

        payload.insert("_urn".to_string(), object_type.urn().into());

        self.lwm2m_pub_service
            .publish_update(self.description.bnw_id(), path, payload.into())
            .context("can't publish resource instance update IPC event")?;

        Ok(())
    }

    /// Publishes a single IPC-message with the value of the given resource.
    pub(super) async fn publish_resource_instance(
        &mut self,
        object_type: lwm2m::ObjectType,
        object_instance: usize,
        resource_id: usize,
        resource_instance: usize,
    ) -> Result<(), Error> {
        if !self.description.included || self.publication_state != PublicationState::Ready {
            return Ok(());
        }

        let object = self.get_object(object_type).context("can't find object")?;

        let mut payload = serde_json::json!(object
            .read_resource(object_instance, resource_id, resource_instance)
            .await
            .context("can't read resource")?);
        payload
            .as_object_mut()
            .context("BUG: resource payload is not a json object")?
            .insert("_urn".to_string(), object_type.urn().into());

        let path = format!(
            "{}/{}/{}{}",
            object_type.as_str(),
            object_instance,
            object
                .get_resource_name(resource_id)
                .context("can't get resource name")?,
            crate::utils::display_fn(move |f| {
                if resource_instance != 0 {
                    write!(f, "/{resource_instance}")
                } else {
                    Ok(())
                }
            })
        );
        drop(object);

        self.lwm2m_pub_service
            .publish_update(self.description.bnw_id(), path, payload)
            .context("can't publish resource instance update IPC event")?;

        Ok(())
    }
}
