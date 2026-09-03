// SPDX-FileCopyrightText: GARDENA GmbH
// SPDX-FileCopyrightText: Tokio Contributors
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Modified version of `tokio-util/src/udp/frame.rs`, licensed under MIT (LICENSES/MIT.txt).
//! Source: https://raw.githubusercontent.com/tokio-rs/tokio/8198ef38814c45f9dc02fcbf826225b5cf32a6bb/tokio-util/src/udp/frame.rs
//!
//! We moved away from AsyncFd due to a [bug](https://github.com/tokio-rs/tokio/issues/4349)
//! that prevents reacting on polling errors.

use tokio_util::codec::{Decoder, Encoder};

use futures_core::Stream;
use std::net::UdpSocket;

use async_io::Async;
use bytes::{BufMut, BytesMut};
use futures_core::ready;
use futures_sink::Sink;
use nix::errno::Errno;
use nix::sys::socket::{
    recvmsg, sendmsg, ControlMessage, ControlMessageOwned, InetAddr, MsgFlags, SockAddr,
};
use nix::sys::uio::IoVec;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::unix::io::AsFd;
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{io, mem::MaybeUninit};

use crate::traits::SocketAddrEx as _;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ICMP")]
    Icmp,
}

/// A unified [`Stream`] and [`Sink`] interface to an underlying `UdpSocket`, using
/// the `Encoder` and `Decoder` traits to encode and decode frames.
///
/// Raw UDP sockets work with datagrams, but higher-level code usually wants to
/// batch these into meaningful chunks, called "frames". This method layers
/// framing on top of this socket by using the `Encoder` and `Decoder` traits to
/// handle encoding and decoding of messages frames. Note that the incoming and
/// outgoing frame types may be distinct.
///
/// This function returns a *single* object that is both [`Stream`] and [`Sink`];
/// grouping this into a single object is often useful for layering things which
/// require both read and write access to the underlying object.
///
/// If you want to work more directly with the streams and sink, consider
/// calling [`split`] on the `UdpFramed` returned by this method, which will break
/// them into separate objects, allowing them to interact more easily.
///
/// [`Stream`]: futures_core::Stream
/// [`Sink`]: futures_sink::Sink
/// [`split`]: https://docs.rs/futures/0.3/futures/stream/trait.StreamExt.html#method.split
#[must_use = "sinks do nothing unless polled"]
#[derive(Debug)]
pub struct UdpFramed<C, T = UdpSocket>
where
    T: AsFd,
{
    socket: Async<T>,
    codec: C,
    rd: BytesMut,
    wr: BytesMut,
    cmsgs: Vec<u8>,
    out_addr: SocketAddr,
    flushed: bool,
    is_readable: bool,
    current_addr: Option<SocketAddr>,
}

const INITIAL_RD_CAPACITY: usize = 64 * 1024;
const INITIAL_WR_CAPACITY: usize = 8 * 1024;
const CMSG_CAPACITY: usize = 1024;

impl<C, T> Unpin for UdpFramed<C, T> where T: AsFd {}

impl<C, T> Stream for UdpFramed<C, T>
where
    T: AsFd,
    C: Decoder,
    C::Error: From<Error>,
{
    type Item = Result<(C::Item, SocketAddr), C::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        pin.rd.reserve(INITIAL_RD_CAPACITY);

        loop {
            // Are there still bytes left in the read buffer to decode?
            if pin.is_readable {
                if let Some(frame) = pin.codec.decode_eof(&mut pin.rd)? {
                    let current_addr = pin
                        .current_addr
                        .expect("will always be set before this line is called");

                    return Poll::Ready(Some(Ok((frame, current_addr))));
                }

                // if this line has been reached then decode has returned `None`.
                pin.is_readable = false;
                pin.rd.clear();
            }

            // We're out of data. Try and fetch more data to decode

            // Convert `&mut [MaybeUnit<u8>]` to `&mut [u8]` because we will be
            // writing to it via `poll_recv_from` and therefore initializing the memory.
            let buf = unsafe {
                &mut *(pin.rd.chunk_mut() as *mut _ as *mut [MaybeUninit<u8>] as *mut [u8])
            };

            let msg = loop {
                pin.cmsgs.clear();
                match recvmsg(
                    pin.socket.as_fd().as_raw_fd(),
                    &[],
                    Some(&mut pin.cmsgs),
                    MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_ERRQUEUE | MsgFlags::MSG_CTRUNC,
                ) {
                    #[allow(unreachable_patterns)]
                    Err(Errno::EAGAIN) | Err(Errno::EWOULDBLOCK) => {
                        // no errors on the socket
                    }
                    Err(e) => {
                        return Poll::Ready(Some(Err(From::from(std::io::Error::other(format!(
                            "failed to read error queue: {e:?}"
                        ))))));
                    }
                    Ok(msg) => {
                        if msg.flags.contains(MsgFlags::MSG_CTRUNC) {
                            return Poll::Ready(Some(Err(From::from(std::io::Error::other(
                                "an error happened but there was not enough memory to read it",
                            )))));
                        }

                        for cmsg in msg.cmsgs() {
                            log::debug!("Control message: {:#?}", cmsg);

                            match cmsg {
                                ControlMessageOwned::Ipv4RecvErr(..)
                                | ControlMessageOwned::Ipv6RecvErr(..) => {
                                    return Poll::Ready(Some(Err(From::from(Error::Icmp))))
                                }
                                _ => {}
                            }
                        }
                    }
                }

                pin.cmsgs.clear();
                match recvmsg(
                    pin.socket.as_fd().as_raw_fd(),
                    &[IoVec::from_mut_slice(buf)],
                    Some(&mut pin.cmsgs),
                    MsgFlags::MSG_DONTWAIT,
                ) {
                    Ok(result) => break result,

                    // those two may not have the same value on all platforms
                    #[allow(unreachable_patterns)]
                    Err(Errno::EAGAIN) | Err(Errno::EWOULDBLOCK) => (),

                    Err(e) => {
                        return Poll::Ready(Some(Err(From::from(std::io::Error::other(format!(
                            "recv failed: {e:?}"
                        ))))))
                    }
                }

                // XXX: as long as we're using `async_io`, this has to come
                //      after `recvmsg`. The reason is unknown.
                ready!(pin.socket.poll_readable(cx))?;
            };

            let mut tclass = 0x00;

            for cmsg in msg.cmsgs() {
                if let ControlMessageOwned::Ipv6TrafficClass(tclass2) = cmsg {
                    tclass = tclass2;
                }
            }

            let tclass: u8 = match tclass.try_into() {
                Ok(v) => v,
                Err(_) => {
                    return Poll::Ready(Some(Err(From::from(std::io::Error::other(format!(
                        "traffic class `{tclass:X}` doesn't fit into u8"
                    ))))))
                }
            };

            let addr = msg
                .address
                .ok_or_else(|| std::io::Error::other("recvmsg didn't return an address"))?;
            let mut addr = match addr {
                SockAddr::Inet(inet) => inet.to_std(),
                _ => {
                    return Poll::Ready(Some(Err(From::from(std::io::Error::other(
                        "recvmsg returned a non-inet address",
                    )))))
                }
            };

            if let Err(e) = addr.set_flowinfo(crate::traits::Flowinfo::new(tclass.into())) {
                return Poll::Ready(Some(Err(From::from(std::io::Error::other(format!(
                    "set_flowinfo failed: {e:?}"
                ))))));
            }

            unsafe {
                pin.rd.advance_mut(msg.bytes);
            }

            pin.current_addr = Some(addr);
            pin.is_readable = true;
        }
    }
}

impl<I, C, T> Sink<(I, SocketAddr)> for UdpFramed<C, T>
where
    T: AsFd,
    C: Encoder<I>,
    C::Error: From<std::io::Error>,
{
    type Error = C::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if !self.flushed {
            match self.poll_flush(cx)? {
                Poll::Ready(()) => {}
                Poll::Pending => return Poll::Pending,
            }
        }

        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: (I, SocketAddr)) -> Result<(), Self::Error> {
        let (frame, out_addr) = item;

        let pin = self.get_mut();

        pin.codec.encode(frame, &mut pin.wr)?;
        pin.out_addr = out_addr;
        pin.flushed = false;

        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.flushed {
            return Poll::Ready(Ok(()));
        }

        let Self {
            ref socket,
            ref mut out_addr,
            ref mut wr,
            ..
        } = *self;

        let traffic_class: libc::c_int = out_addr.flowinfo().traffic_class().raw().into();
        let n = loop {
            match sendmsg(
                socket.get_ref().as_fd().as_raw_fd(),
                &[IoVec::from_slice(wr)],
                &[ControlMessage::Ipv6TrafficClass(&traffic_class)],
                MsgFlags::MSG_DONTWAIT,
                Some(&SockAddr::new_inet(InetAddr::from_std(out_addr))),
            ) {
                Ok(result) => break result,

                #[allow(unreachable_patterns)]
                Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EWOULDBLOCK) => (),

                Err(e) => {
                    return Poll::Ready(Err(std::io::Error::from(e).into()));
                }
            }

            // XXX: as long as we're using `async_io`, this has to come
            //      after `sendmsg`. The reason is unknown.
            ready!(socket.poll_writable(cx))?;
        };

        let wrote_all = n == self.wr.len();
        self.wr.clear();
        self.flushed = true;

        let res = if wrote_all {
            Ok(())
        } else {
            Err(io::Error::other("failed to write entire datagram to socket").into())
        };

        Poll::Ready(res)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.poll_flush(cx))?;
        Poll::Ready(Ok(()))
    }
}

#[allow(dead_code)]
impl<C, T> UdpFramed<C, T>
where
    T: AsFd,
{
    /// Create a new `UdpFramed` backed by the given socket and codec.
    ///
    /// See struct level documentation for more details.
    pub fn new(socket: T, codec: C) -> std::io::Result<UdpFramed<C, T>> {
        nix::sys::socket::setsockopt(
            socket.as_fd().as_raw_fd(),
            nix::sys::socket::sockopt::Ipv6RecvErr,
            &true,
        )?;

        nix::sys::socket::setsockopt(
            socket.as_fd().as_raw_fd(),
            nix::sys::socket::sockopt::Ipv6RecvTclass,
            &true,
        )?;

        Ok(Self {
            socket: Async::new(socket)?,
            codec,
            out_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
            rd: BytesMut::with_capacity(INITIAL_RD_CAPACITY),
            wr: BytesMut::with_capacity(INITIAL_WR_CAPACITY),
            cmsgs: Vec::with_capacity(CMSG_CAPACITY),
            flushed: true,
            is_readable: false,
            current_addr: None,
        })
    }

    /// Returns a reference to the underlying I/O stream wrapped by `Framed`.
    ///
    /// # Note
    ///
    /// Care should be taken to not tamper with the underlying stream of data
    /// coming in as it may corrupt the stream of frames otherwise being worked
    /// with.
    pub fn get_ref(&self) -> &T {
        self.socket.get_ref()
    }

    /// Returns a mutable reference to the underlying I/O stream wrapped by `Framed`.
    ///
    /// # Note
    ///
    /// Care should be taken to not tamper with the underlying stream of data
    /// coming in as it may corrupt the stream of frames otherwise being worked
    /// with.
    pub unsafe fn get_mut(&mut self) -> &mut T {
        self.socket.get_mut()
    }

    /// Returns a reference to the underlying codec wrapped by
    /// `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying codec
    /// as it may corrupt the stream of frames otherwise being worked with.
    pub fn codec(&self) -> &C {
        &self.codec
    }

    /// Returns a mutable reference to the underlying codec wrapped by
    /// `UdpFramed`.
    ///
    /// Note that care should be taken to not tamper with the underlying codec
    /// as it may corrupt the stream of frames otherwise being worked with.
    pub fn codec_mut(&mut self) -> &mut C {
        &mut self.codec
    }

    /// Returns a reference to the read buffer.
    pub fn read_buffer(&self) -> &BytesMut {
        &self.rd
    }

    /// Returns a mutable reference to the read buffer.
    pub fn read_buffer_mut(&mut self) -> &mut BytesMut {
        &mut self.rd
    }

    /// Consumes the `Framed`, returning its underlying I/O stream.
    pub fn into_inner(self) -> T {
        self.socket.into_inner().unwrap()
    }
}
