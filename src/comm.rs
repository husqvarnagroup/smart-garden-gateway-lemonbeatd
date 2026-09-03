// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Everything related to socket-level communication with lemonbeat devices.
//! Data gets converted to/from the XSD network types but stops there.

use crate::traits::SocketAddrEx as _;
use crate::Error;
use anyhow::anyhow;
use anyhow::Context as _;
use futures_util::SinkExt as _;
use futures_util::StreamExt as _;
use rand::Rng as _;
use std::convert::TryInto as _;
use tracing::Instrument;

lazy_static::lazy_static! {
    static ref PORTS: std::sync::Mutex<u64> = std::sync::Mutex::new(1);
    static ref WAKEUP_CHANNELS: std::sync::Mutex<[u8; 31]> = std::sync::Mutex::new([0u8; 31]);
}

/// A port allocated from the lemonbeat source port range.
///
/// Tests have shown that using ports outside that range may result in answers
/// not being delivered depending on the message type.
///
/// We're not using all of the available range to make room for lwm2mserver
/// though.
#[derive(Debug)]
struct Port {
    bit: u8,
}

impl Port {
    fn allocate() -> Result<Self, Error> {
        let mut ports = PORTS.lock().unwrap();
        if *ports == u64::MAX {
            anyhow::bail!("no free UDP source port available");
        }

        // This might loop a while if many ports are in use.
        // Since we only have 64 possible values, it probably won't be too long
        // and we're accepting the risk of stalling the CPU for a bit.
        loop {
            let bit = rand::thread_rng().gen_range(0..64);
            if *ports & (1 << bit) == 0 {
                *ports |= 1 << bit;
                return Ok(Port { bit });
            }
        }
    }

    fn number(&self) -> u16 {
        20128u16 + (self.bit as u16)
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        let mut ports = PORTS.lock().unwrap();

        assert_ne!(*ports & (1 << self.bit), 0);

        *ports &= !(1 << self.bit);
    }
}

/// A wakeup channel that's tracked globally
///
/// For short periods of time we can have more instances than we have devices
/// since every device description stores this channel.
/// This is done to simplify the usage and it doesn't matter because:
/// - the number of slots per channel(u8 => 256) is higher than the total
///   number of devices that we allow (around 25-30)
/// - most of the time, we'll have one allocation per device only, since we
///   don't receive device descriptions that often and don't keep them around
///   for too long (they'll be put into the device task command queue)
/// - if a new device needs a channel we always use the one with the fewest
///   number of allocations. If a channel is over-allocated we'll just use
///   another one with the same or a higher number.
///   You can come up with extreme cases where you'd choose channel that's much
///   worse because all the other channels are over-allocated at the same time
///   but it's really unlikely for that to happen at all, let alone repeatedly.
#[derive(Debug)]
pub struct WakeupChannel {
    /// The lemonbeat channel number
    ///
    /// We are wasting `[0]` since there is no channel 0 but it makes the rest
    /// of the code easier and less error-prone.
    index: usize,
}

impl WakeupChannel {
    // We filter out channels that appear in the channel map because using
    // one of them would cause a device to be woken up during normal
    // non-wakeup communication.
    pub fn allocate(channel_map: &crate::storage::ChannelMap) -> Result<Self, Error> {
        let mut channels = WAKEUP_CHANNELS.lock().unwrap();

        let usable_channels = channels.iter().enumerate().filter(|(channel, _)| {
            *channel >= 1 && *channel <= 30 && !channel_map.contains(*channel)
        });

        // First want to know which use-count the least used channels have.
        // This will be `0` most of the time.
        let lowest_use_count = usable_channels
            .clone()
            .min_by(|(_, n1), (_, n2)| n1.cmp(n2))
            .map(|(_, num)| num)
            .ok_or_else(|| anyhow!("can't find a matching channel"))?;

        // Now we create a list of all channels which share the same use-count
        // because from this gateways perspective they're all equally good.
        let mut least_used_channels = usable_channels
            .clone()
            .filter(|(_, num)| *num == lowest_use_count);

        // Now we choose a random channel among those, to prevent all gateways
        // of using the lowest one. This reduces interference between gatways
        // that are physically in reach.
        // It's important to use `gen_range` on the `least_used_channels` list
        // to make the selection process uniformly random without introducing
        // bias by skipping over certain values.
        let least_used_channels_count = least_used_channels.clone().count();
        let index = least_used_channels
            .nth(rand::thread_rng().gen_range(0..least_used_channels_count))
            .map(|(channel, _)| channel)
            .context("BUG: no matching least used channel")?;

        if channels[index] == u8::MAX {
            anyhow::bail!("ran out of channels");
        }

        channels[index] += 1;

        log::trace!(
            "Allocated wakeup channel {}, num_users={}",
            index,
            channels[index]
        );

        Ok(WakeupChannel { index })
    }

    pub fn allocate_unchecked(index: u64) -> Result<Self, Error> {
        let mut channels = WAKEUP_CHANNELS.lock().unwrap();

        let index: usize = index.try_into().context("can't convert index to usize")?;

        let num_users = channels
            .get_mut(index)
            .ok_or_else(|| anyhow!("index is out of range"))?;
        if *num_users == u8::MAX {
            anyhow::bail!("ran out of channels");
        }

        *num_users += 1;

        log::trace!(
            "(unchecked) allocated wakeup channel {}, num_users={}",
            index,
            num_users
        );

        Ok(WakeupChannel { index })
    }

    pub fn channel(&self) -> u64 {
        // PANIC: this can't happen because the number of `WAKEUP_CHANNELS` is
        //        way lower than even u32. Unfortunately I don't know how to
        //        create static assertions on the inner type of a mutex.
        self.index.try_into().unwrap()
    }
}

impl Drop for WakeupChannel {
    fn drop(&mut self) {
        let mut channels = WAKEUP_CHANNELS.lock().unwrap();

        assert_ne!(channels[self.index], 0);

        channels[self.index] -= 1;

        log::trace!(
            "Deallocated wakeup channel {}, num_users={}",
            self.index,
            channels[self.index]
        );
    }
}

impl Clone for WakeupChannel {
    fn clone(&self) -> Self {
        Self::allocate_unchecked(self.index.try_into().unwrap()).unwrap()
    }
}

impl serde::Serialize for WakeupChannel {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.channel().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for WakeupChannel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::allocate_unchecked(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// EXI tokio codec
///
/// This codec handles all the EXI and XML encoding/decoding and allows
/// transferring [lsdl::Network] types directly.
#[derive(Default)]
pub struct EXICodec<Item> {
    pd: std::marker::PhantomData<Item>,
}

impl<Item> tokio_util::codec::Encoder<&Item> for EXICodec<Item>
where
    Item: lsdl::Network + lsdl::NetworkPort,
{
    type Error = Error;

    fn encode(&mut self, item: &Item, dst: &mut bytes::BytesMut) -> Result<(), Self::Error> {
        let mut writer = quick_xml::Writer::new(std::io::Cursor::new(Vec::new()));
        item.to_xml_writer(&mut writer)
            .context("can't convert Network to xml")?;
        let xml = writer.into_inner().into_inner();

        unsafe {
            dst.set_len(1024);
        }
        let len = lsdl::compress_xml(Item::get_port(), &xml, dst.as_mut(), 0)
            .context("can't compress xml")?;
        unsafe {
            dst.set_len(len);
        }
        Ok(())
    }
}

impl<Item> tokio_util::codec::Decoder for EXICodec<Item>
where
    Item: lsdl::Network + lsdl::NetworkPort,
{
    type Item = Item;
    type Error = RequestError;

    fn decode(&mut self, _src: &mut bytes::BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        Err(RequestError::Internal(anyhow!(
            "EXICodec doesn't support partial decoding"
        )))
    }

    fn decode_eof(&mut self, buf: &mut bytes::BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if buf.is_empty() {
            return Ok(None);
        }

        let mut xml = [0; 4096];

        let res = lsdl::decompress_exi(Item::get_port(), buf, &mut xml, 0)
            .context("can't decompress exi");
        // it is important to clear the buffer now to prevent trying to parse
        // the same data over and over again in case this fails and the caller
        // wants to retry assuming that errors go away.
        buf.clear();
        let len = res?;

        let xml = &xml[..len];

        let mut reader = quick_xml::Reader::from_reader(xml);
        reader.trim_text(true);
        let mut ctx = lsdl::ReadContext {
            reader,
            buf: Vec::new(),
        };
        Ok(Some(
            Item::from_xml_readctx(&mut ctx).context("can't parse lsdl xml")?,
        ))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    #[error("timeout")]
    Timeout,
    #[error("internal: {0:?}")]
    Internal(#[from] anyhow::Error),
    #[error("UDP: {0:?}")]
    Udp(#[from] crate::udp::Error),
}

/// we don't want to bother our users with handling this case since it's just
/// another internal error.
/// This error comes from the udp implementation.
impl From<std::io::Error> for RequestError {
    fn from(e: std::io::Error) -> Self {
        let e: anyhow::Error = e.into();
        Self::Internal(e.context("IO error"))
    }
}

/// lemonbeat request instance
///
/// the initial idea behind this was to make it possible to re-send a network
/// message through the same socket in case a previous attempt failed.  
/// It turned out that we'd rather use a new socket to prevent receiving
/// answers to an old request after we did another attempt.  
/// The implementation was still kept for the following reasons:  
/// - it makes it possible to implement `lemonbeat_request` without duplicating
///   code
/// - it doesn't increase the number of lines all that much
pub struct LemonbeatRequest<'a, N> {
    framed: crate::udp::UdpFramed<EXICodec<N>>,
    device_addr: std::net::SocketAddr,
    network: &'a N,
    timeout: std::time::Duration,
    /// we're only allowed to use the port as long as this struct is alive
    #[allow(dead_code)]
    port: Port,
}

impl<'a, N> LemonbeatRequest<'a, N>
where
    N: Unpin + lsdl::Network + lsdl::NetworkPort + Default,
{
    pub async fn new(
        local_addr: &std::net::SocketAddr,
        mut device_addr: std::net::SocketAddr,
        network: &'a N,
    ) -> Result<LemonbeatRequest<'a, N>, Error> {
        device_addr.set_port(N::get_port());

        let port = Port::allocate().context("can't allocate port")?;
        let mut local_addr = *local_addr;
        local_addr.set_port(port.number());

        log::debug!("Construct request from {} to {}", local_addr, device_addr);

        let sock = tokio::net::UdpSocket::bind(local_addr)
            .await
            .context("can't bind to ppp interface")?;
        let framed = crate::udp::UdpFramed::new(
            sock.into_std()
                .context("can't convert tokio udp socket to std")?,
            EXICodec::default(),
        )
        .context("can't create UdpFramed")?;

        Ok(Self {
            framed,
            device_addr,
            network,
            timeout: crate::current_config().request_default_timeout,
            port,
        })
    }

    pub async fn attempt_send(&mut self) -> Result<(), RequestError> {
        log::debug!("Send request to {}", self.device_addr);
        self.framed
            .send((self.network, self.device_addr))
            .await
            .map_err(|e| RequestError::Internal(e.context("can't send lemonbeat request")))
    }

    pub async fn read_answer(&mut self) -> Result<N, RequestError> {
        log::debug!("Read answer from {}", self.device_addr);
        Ok(self
            .framed
            .next()
            .await
            .unwrap_or_else(|| Err(RequestError::Internal(anyhow!("BUG: stream ended"))))?
            .0)
    }

    pub async fn attempt_send_and_recv(&mut self) -> Result<N, RequestError> {
        // NOTE: we include `send` in the timeout because we don't know the
        //       reason for that failure. We just hope that it'll go away by
        //       backing off a little and retrying later on.
        let next =
            async {
                match self.framed.send((self.network, self.device_addr)).await {
                    Err(e) => Err(RequestError::Internal(
                        e.context("can't send lemonbeat request"),
                    )),

                    Ok(_) => self.framed.next().await.unwrap_or_else(|| {
                        Err(RequestError::Internal(anyhow!("BUG: stream ended")))
                    }),
                }
            };

        let next = tokio::time::timeout(self.timeout, next);
        next.await
            .unwrap_or(Err(RequestError::Timeout))
            .map(|(network, _address)| network)
    }

    pub fn set_timeout(&mut self, timeout: std::time::Duration) {
        self.timeout = timeout;
    }
}

/// a lemonbeat service
///
/// This service waits for messages of the provided network type.  
/// The network type tells this service what port to listen on.
pub struct Service<N, F, D> {
    network: std::marker::PhantomData<N>,
    data: D,
    f: F,
    interface_addr: std::net::SocketAddr,
}

impl<N, F, Fut, D> Service<N, F, D>
where
    N: 'static + std::fmt::Debug + Send + Unpin + lsdl::Network + lsdl::NetworkPort + Default,
    D: 'static + Clone + Send + Sync,
    F: 'static + Send + Sync + Fn(D, N, std::net::SocketAddr) -> Fut,
    Fut: std::future::Future<Output = Result<(), Error>> + Send,
{
    /// creates a service that listens on `interface_addr`
    ///
    /// it will call `f` with `data`, the received package, and the sender
    /// address for each packet.  
    /// `data` is just a caller context.
    pub fn new(interface_addr: std::net::SocketAddr, data: D, f: F) -> Self {
        Self {
            network: std::marker::PhantomData,
            data,
            f,
            interface_addr,
        }
    }

    async fn task_handler(&mut self) -> Result<(), Error> {
        // we usually get multicast packets so listen everywhere
        self.interface_addr
            .set_ip(std::net::Ipv6Addr::UNSPECIFIED.into());
        self.interface_addr.set_port(N::get_port());

        tracing::info!(socket=?self.interface_addr, "Started service");

        let sock = tokio::net::UdpSocket::bind(&self.interface_addr)
            .await
            .context("failed to bind service socket")?;
        let mut framed = crate::udp::UdpFramed::new(
            sock.into_std()
                .context("can't convert tokio udp socket to std")?,
            EXICodec::default(),
        )
        .context("can't create UdpFramed")?;

        loop {
            let (frame, addr): (N, _) = match framed
                .next()
                .await
                .ok_or_else(|| anyhow!("UDP listener socket got closed"))?
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!("Receive error: {}", e);
                    continue;
                }
            };
            tracing::trace!(sender=?addr, flowinfo=%addr.flowinfo().raw(), "{:#?}", frame);

            if let Err(e) = (self.f)(self.data.clone(), frame, addr).await {
                tracing::error!(sender=?addr, "Failed to process frame: {}", e);
            }
        }
    }

    /// start a tokio task which processes requests
    pub fn start(mut self) {
        tokioutil::spawn_named(
            &format!("lb-svc-{}", N::get_port()),
            async move {
                // PANIC: if we compile with `panic=abort`, this will kill the
                //        whole server instead of just this task.
                //        That's exactly what we want so systemd can restart us
                //        and we're back to a state where all tasks are running.
                //        (hopefully ¯\_(ツ)_/¯(
                self.task_handler().await.unwrap();
            }
            .instrument(tracing::info_span!(
                parent: None,
                "lbsvc",
                port = N::get_port()
            )),
        );
    }
}

#[cfg(test)]
mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    #[test_log::test]
    fn ports() {
        *PORTS.lock().unwrap() = 0;

        let mut ports: Vec<_> = (0..64)
            .map(|_| {
                let port = Port::allocate().unwrap();
                assert_ne!(*PORTS.lock().unwrap() & (1 << port.bit), 0);
                assert_eq!(port.number(), 20128u16 + (port.bit as u16));
                port
            })
            .collect();

        match Port::allocate() {
            Err(_) => (),
            other => panic!("unexpected result: {other:?}"),
        }

        ports.pop();

        match Port::allocate() {
            Ok(_) => (),
            other => panic!("unexpected result: {other:?}"),
        }

        ports.clear();
        assert_eq!(*PORTS.lock().unwrap(), 0);
    }

    #[test_log::test]
    fn wakeup_channel() {
        let channel_map = crate::storage::ChannelMap::new(num_bigint::BigUint::from_bytes_be(&[
            0x10, 0x08, 0x08, 0x04,
        ]));
        eprintln!("{channel_map:#?}");

        let channel0 = WakeupChannel::allocate(&channel_map).unwrap();

        let channel1 = WakeupChannel::allocate(&channel_map).unwrap();
        assert_ne!(channel1.index, channel0.index);

        let channel2 = WakeupChannel::allocate(&channel_map).unwrap();
        assert_ne!(channel2.index, channel0.index);
        assert_ne!(channel2.index, channel1.index);
    }
}
