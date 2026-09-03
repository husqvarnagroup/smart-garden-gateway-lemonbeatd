// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! radio module setup and communication

use std::fmt;

use anyhow::anyhow;
use anyhow::Context as _;
use anyhow::Error;
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tracing::Instrument as _;

const API_VERSION: u8 = 0x01;

#[allow(dead_code)]
#[derive(Debug, Copy, Clone)]
enum ApiCommand {
    // Note: the command names are consistent with names used in
    // firmware (api.c) and Python API client. Please do not invent
    // new names here.
    SetNetworkKey = 0x01,
    SetMacAddress = 0x02,
    WakeupDevice = 0x03,
    ResetDeviceNonce = 0x04,
    SetTxMacCounter = 0x05,
    GetMacAddress = 0x06,
    GetAntennaDiversityMode = 0x07,
    SetAntennaDiversityMode = 0x08,
    GetAntennaDiversity = 0x09,
    SetAntennaDiversity = 0x0a,
    GetAntennaIntExt = 0x0b,
    SetAntennaIntExt = 0x0c,
    GetAppVersion = 0x0d,
    GetTxMacCounter = 0x0e,
    Si4467StartCW = 0xe0,
    Si4467StopTx = 0xe1,
}

// from https://stackoverflow.com/questions/32710187
impl fmt::Display for ApiCommand {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

#[allow(dead_code)]
#[derive(Debug, Copy, Clone, FromPrimitive, PartialEq)]
enum ApiResultCode {
    // Note: the result code names are consistent with names used in
    // firmware (api.c) and Python API client. Please do not invent
    // new names here.
    Okay = 0x00,
    IfaceNotFound = 0x01,
    CantSetKey = 0x02,
    CantSetMacAddress = 0x03,
    UnsupportedCommand = 0x04,
    LockTimeout = 0x05,
    WakeupFailed = 0x06,
    CantSave = 0x07,
    CantSetTxMacCounter = 0x08,
    CantResetTxMacCounter = 0x09,
    TxMacCounterWouldDecrease = 0x0A,
    CantGetKey = 0x0B,
    CantResetTxMacCtr = 0x0C,
    CantGetMacAddress = 0x0D,
    Si4467CommandFailed = 0x0E,
    Si4467ContextNotFound = 0x0F,
    InvalidArgument = 0x10,
    NoHwSupport = 0x11,
    CantGetTxMacCounter = 0x12,
}

// from https://stackoverflow.com/questions/32710187
impl fmt::Display for ApiResultCode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug)]
struct ApiResponse {
    api_version: u8,
    result_code: ApiResultCode,
    length: u8,
    data: Vec<u8>,
}

pub struct Ifaddr {
    pub address: std::net::SocketAddr,
    pub destination: std::net::SocketAddr,
}

#[allow(dead_code)]
pub enum DiversityMode {
    Mcu = 0x00,
    Trx = 0x01,
}

fn sockaddr_nix2std(socket: &nix::sys::socket::SockAddr) -> Option<std::net::SocketAddr> {
    match socket {
        nix::sys::socket::SockAddr::Inet(nix::sys::socket::InetAddr::V6(v6)) => {
            Some(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                std::net::Ipv6Addr::from(v6.sin6_addr.s6_addr),
                v6.sin6_port,
                v6.sin6_flowinfo,
                v6.sin6_scope_id,
            )))
        }
        _ => None,
    }
}

async fn get_rm_addr(name: &'static str) -> Result<Ifaddr, crate::Error> {
    tokio::task::spawn_blocking(move || {
        let mut v = None;
        for addr in nix::ifaddrs::getifaddrs()
            .with_context(|| format!("can't get interface addresses for `{name}`"))?
        {
            if addr.interface_name != name {
                continue;
            }

            if let (Some(address), Some(destination)) = (addr.address, addr.destination) {
                if address.family() != nix::sys::socket::AddressFamily::Inet6 {
                    continue;
                }

                let address = sockaddr_nix2std(&address)
                    .with_context(|| format!("unsupported address: {address}"))?;
                let destination =
                    sockaddr_nix2std(&destination).context("unsupported destination")?;

                if v.is_some() {
                    anyhow::bail!("multiple addresses found");
                }

                v = Some(Ifaddr {
                    address,
                    destination,
                });
            }
        }

        v.context("no matching address found")
    })
    .await?
}

pub async fn get_rm_mac() -> Result<macaddr::MacAddr6, crate::Error> {
    let output = tokio::process::Command::new("fw_printenv")
        .arg("-n")
        .arg("rmaddr")
        .output()
        .await
        .context("failed to run fw_printenv")?;
    if !output.status.success() {
        return Err(anyhow!("fw_printenv failed: {:?}", output.status.code()));
    }

    let output = std::str::from_utf8(&output.stdout).context("output is not valid UTF-8")?;
    output
        .trim()
        .parse()
        .context("output is not a valid MAC address")
}

// Note: sock_task and sock_client_task are meant for forwarding
// external requsets to the radio module (which can handle only one
// TCP connection at a time), such as wakeup requests from the LWM2M
// server. The request & response format for external requests via the
// Unix socket is identical to the format of requests made via the TCP
// API.
async fn socket_client_task(
    mut handle: RadioModuleHandle,
    mut stream: tokio::net::UnixStream,
) -> Result<(), crate::Error> {
    loop {
        let api_version = match stream.read_u8().await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("can't read API version (no further requests): {e}");
                return Ok(());
            }
        };
        if api_version != API_VERSION {
            // no point in trying to continue, as following
            // bytes will not be in the format we expect
            anyhow::bail!("invalid API version: {api_version}");
        }
        let command = stream.read_u8().await.context("can't read command")?;
        let length = stream.read_u8().await.context("can't read length")?;
        let mut request = vec![api_version, command, length];
        if length > 0 {
            let mut buffer = vec![0; length.into()];
            stream
                .read_exact(&mut buffer)
                .await
                .with_context(|| anyhow!("can't read {length} bytes of payload"))?;
            request.extend(buffer);
        }
        let result = match handle
            .request_raw(request)
            .await
            .context("internal request failed")
            .and_then(core::convert::identity)
        {
            Err(e) => {
                tracing::error!("Request failed: {e}");
                continue;
            }
            Ok(r) => r,
        };

        let mut response = vec![result.api_version, result.result_code as u8, result.length];
        if result.length > 0 {
            response.extend(result.data);
        }
        stream
            .write_all(&response)
            .await
            .context("can't write result")?;
        tracing::debug!("request for command {command} handled successfully");
    }
}

async fn socket_task(handle: RadioModuleHandle) -> Result<(), crate::Error> {
    let path = crate::runtime_dir().join("radiomodule_api");
    let path_display = path.display();
    if path.exists() {
        tracing::info!("Removing old API Unix socket {path_display}");
        if let Err(e) = std::fs::remove_file(path.clone()) {
            tracing::error!("Failed to remove old socket: {e}");
            return Err(e.into());
        };
    }

    tracing::info!("Creating API Unix socket {path_display}");
    let listener = tokio::net::UnixListener::bind(path).context("failed to bind socket")?;
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let handle = handle.clone_uncounted();
                tokioutil::spawn_named(
                    "radiomodule-requests-socket-client",
                    async move {
                        if let Err(e) = socket_client_task(handle, stream).await {
                            tracing::info!("Client task failed: {e}");
                        } else {
                            tracing::info!("Client task completed");
                        }
                    }
                    .instrument(tracing::info_span!(parent: None, "radiomodule")),
                );
            }
            Err(e) => {
                tracing::error!("Listener error: {e}");
            }
        }
    }
}

struct RadioModule {
    receiver: RadioModuleReceiver,
    stream: Option<tokio::net::TcpStream>,
}

impl RadioModule {
    async fn get_connection(&mut self) -> Result<&mut tokio::net::TcpStream, crate::Error> {
        #[allow(clippy::unnecessary_unwrap)] // difficult to fix due to borrow checker
        if self.stream.is_some() {
            return Ok(self.stream.as_mut().unwrap());
        }

        let mut rm_addr = get_rm_addr("ppp0")
            .await
            .context("can't get radiomodule address")?;
        rm_addr.destination.set_port(8888);

        let stream = tokio::net::TcpStream::connect(rm_addr.destination)
            .await
            .context("can't connect")?;

        tracing::info!("New radiomodule connection");
        tracing::info!("Local: {:?}", rm_addr.address);
        tracing::info!("Radio: {:?}", rm_addr.destination);

        self.stream = Some(stream);
        self.stream.as_mut().context("BUG: no stream")
    }

    async fn close_stream(&mut self) {
        let stream = match self.stream.as_mut() {
            Some(stream) => stream,
            None => return,
        };

        if let Err(e) = stream.shutdown().await {
            tracing::error!("Failed to shut down stream: {e}");
        }

        self.stream = None;
    }
}

#[tokio_task_rpc::interface(handle_visibility = "pub")]
impl RadioModule {
    async fn request_raw(&mut self, request: Vec<u8>) -> anyhow::Result<ApiResponse> {
        let mut stream = self.get_connection().await?;

        let (request_first, request_rest) = request.split_first().context("empty request")?;

        // The stream might have been closed before we received this request.
        // In that case the write will fail and it'd be stupid to fail the
        // whole request due to that.
        // So instead, detect that by writing a single byte and retry with a
        // fresh connection if that fails. Checking for the error kind might be
        // more strict but I find retrying in any case more robust.
        if let Err(e) = stream.write_all(&[*request_first]).await {
            tracing::warn!("Stream error on first byte, retry: {e}");
            self.close_stream().await;

            stream = self.get_connection().await?;
            if let Err(e) = stream.write_all(&[*request_first]).await {
                self.close_stream().await;
                return Err(e).context("failed to write request (first)");
            }
        }

        if let Err(e) = stream.write_all(request_rest).await {
            self.close_stream().await;
            return Err(e).context("failed to write request (rest)");
        }

        // read API version
        let api_version = match stream.read_u8().await {
            Err(e) => {
                self.close_stream().await;
                return Err(e).context("failed to read response API version");
            }
            Ok(v) => {
                if v != API_VERSION {
                    // no point in trying to continue, as following
                    // bytes will not be in the format we expect
                    anyhow::bail!("invalid API version: {v}");
                }
                v
            }
        };

        // read result code
        let result_code = match stream.read_u8().await {
            Err(e) => {
                self.close_stream().await;
                return Err(e).context("failed to read response result code");
            }
            Ok(v) => ApiResultCode::from_u8(v).unwrap(),
        };

        // read data length
        let length = match stream.read_u8().await {
            Err(e) => {
                self.close_stream().await;
                return Err(e).context("failed to read response length");
            }
            Ok(v) => v,
        };

        // read data
        let mut data = vec![0; length.into()];
        match stream.read_exact(&mut data).await {
            Err(e) => {
                self.close_stream().await;
                return Err(e).context("failed to read response length");
            }
            Ok(v) => v,
        };

        let response = ApiResponse {
            api_version,
            result_code,
            length,
            data,
        };

        Ok(response)
    }
}

pub fn create() -> RadioModuleHandle {
    let (handle, receiver) = RadioModuleHandle::new();

    tokioutil::spawn_named(
        "radiomodule-requests-rpc",
        async move {
            let mut radiomodule = RadioModule {
                receiver,
                stream: None,
            };
            radiomodule.handle_requests().await;
        }
        .instrument(tracing::info_span!(parent: None, "radiomodule")),
    );

    tokioutil::spawn_named(
        "radiomodule-requests-socket",
        socket_task(handle.clone_uncounted())
            .instrument(tracing::info_span!(parent: None, "radiomodule")),
    );

    handle
}

impl RadioModuleHandle {
    async fn request_unchecked(
        &mut self,
        command: ApiCommand,
        data: Vec<u8>,
    ) -> anyhow::Result<ApiResponse> {
        let datalen: u8 = data
            .len()
            .try_into()
            .context("data length doesn't fit into u8")?;
        let mut request = vec![API_VERSION, command as u8, datalen];
        request.extend(data);
        self.request_raw(request).await?
    }
    async fn request(&mut self, command: ApiCommand, data: Vec<u8>) -> anyhow::Result<ApiResponse> {
        let response = self.request_unchecked(command, data).await?;

        if response.result_code == ApiResultCode::Okay {
            tracing::debug!(
                "API request {}[{}] completed successfully (API version: {}, data length: {}).",
                command.to_string(),
                command as u8,
                response.api_version,
                response.length
            );
        } else {
            tracing::warn!(
                "API request {}[{}] failed with result {}[{}] (API version: {}, data length: {}).",
                command.to_string(),
                command as u8,
                response.result_code.to_string(),
                response.result_code as u8,
                response.api_version,
                response.length
            );

            anyhow::bail!("API error: {}", response.result_code);
        }

        Ok(response)
    }

    pub async fn get_app_version(&mut self) -> Result<String, crate::Error> {
        match self.request(ApiCommand::GetAppVersion, Vec::new()).await {
            Err(e) => Err(e).context("failed to get RM firmware version"),
            Ok(r) => Ok(String::from_utf8(r.data).unwrap()),
        }
    }

    pub async fn set_network_key(
        &mut self,
        network: &crate::crypto::Network,
    ) -> Result<(), crate::Error> {
        self.request(
            ApiCommand::SetNetworkKey,
            network.raw_network_key().to_vec(),
        )
        .await
        .map(|_| ())
    }

    pub async fn set_mac_address(
        &mut self,
        mac_address: &macaddr::MacAddr6,
    ) -> Result<(), crate::Error> {
        self.request(ApiCommand::SetMacAddress, mac_address.as_bytes().to_vec())
            .await
            .map(|_| ())
    }

    /// Set origin of antenna diversity control (SiM3U167 MCU or
    /// Si4467 TRX).
    ///
    /// Note that for antenna diversity to work, SiM3U167 firmware
    /// must take care of setting up the Si4467 to output the antenna
    /// diversity signal on GPIO1. However, it is safe to enable the
    /// Si4467 as source for the antenna diversity signal in any case,
    /// as by default it will just drive the GPIO1 pin low, thus
    /// enabling the default antenna.
    pub async fn set_antenna_diversity_mode(
        &mut self,
        mode: DiversityMode,
    ) -> Result<bool, crate::Error> {
        let response = self
            .request_unchecked(ApiCommand::SetAntennaDiversityMode, vec![mode as u8])
            .await?;

        match response.result_code {
            ApiResultCode::Okay => Ok(true),
            ApiResultCode::NoHwSupport => Ok(false),
            _ => anyhow::bail!("API error: {}", response.result_code),
        }
    }

    /// Attempts to send wakeup to device. Might not succeed, caller needs to check for
    /// confirmation through status message. Only returns error if unexpected issue occurs.
    pub async fn try_wakeup_device(
        &mut self,
        address: &[u8; 6],
        duration: std::time::Duration,
        channel: u8,
    ) -> Result<(), crate::Error> {
        let duration_ms: u32 = duration
            .as_millis()
            .try_into()
            .context("duration doesn't fit into u32")?;
        let request_data = [
            &duration_ms.to_le_bytes(),
            &address[..],
            &channel.to_le_bytes(),
        ]
        .concat();

        match self
            .request_unchecked(ApiCommand::WakeupDevice, request_data)
            .await?
            .result_code
        {
            ApiResultCode::Okay => Ok(()),
            ApiResultCode::WakeupFailed => {
                tracing::info!("Wakeup failed to send");
                Ok(())
            }
            code => anyhow::bail!("Unexpected wakeup error: {}", code),
        }
    }

    pub async fn reset_device_nonce(&mut self, address: &[u8; 6]) -> Result<(), crate::Error> {
        self.request(ApiCommand::ResetDeviceNonce, address.to_vec())
            .await
            .map(|_| ())
    }

    pub async fn get_tx_mac_counter(&mut self) -> Result<u64, crate::Error> {
        match self.request(ApiCommand::GetTxMacCounter, Vec::new()).await {
            Err(e) => Err(e).context("failed to get TX MAC counter"),
            Ok(r) => Ok(u64::from_le_bytes(r.data.try_into().unwrap())),
        }
    }

    pub async fn set_tx_mac_counter(&mut self, counter: u64) -> Result<(), crate::Error> {
        self.request(ApiCommand::SetTxMacCounter, counter.to_le_bytes().to_vec())
            .await
            .map(|_| ())
    }
}
