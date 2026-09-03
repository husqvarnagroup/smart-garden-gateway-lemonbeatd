// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! A random collection of useful traits

use anyhow::anyhow;
use anyhow::Context as _;
use bitflags::bitflags;
use num_traits::ToPrimitive;
use single_trait_impl::single_trait_impl;

#[single_trait_impl]
impl<Idx: Clone> RangeEx<Idx> for std::ops::Range<Idx> {
    /// The same as [chunks](https://doc.rust-lang.org/std/primitive.slice.html#method.chunks) but for Ranges
    fn chunks(&self, chunk_size: Idx) -> RangeChunks<Idx> {
        RangeChunks {
            range: self.clone(),
            chunk_size,
        }
    }
}

pub struct RangeChunks<Idx> {
    range: std::ops::Range<Idx>,
    chunk_size: Idx,
}

impl<Idx: Copy + num_traits::Num + std::cmp::Ord + std::ops::AddAssign> std::iter::Iterator
    for RangeChunks<Idx>
{
    type Item = std::ops::Range<Idx>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.range.is_empty() {
            None
        } else {
            let start = self.range.start;
            let diff = (self.range.end - self.range.start).min(self.chunk_size);

            self.range.start += diff;

            Some(start..start + diff)
        }
    }
}

pub const fn bitmask(len: usize) -> u64 {
    let mut val = 0u64;

    let mut i = 0;
    while i < len {
        val |= 1 << i;

        i += 1;
    }

    val
}

/// Trait to interpret an array of bytes as a SGTIN96 and extract values according
/// to <https://www.gs1.org/sites/default/files/docs/epc/tds_1_1_rev_1_27-standard-20050510.pdf>
pub trait Sgtin96 {
    #[allow(dead_code)] // used in other crates
    fn header(&self) -> u8;
    fn item_reference(&self) -> u32;
    fn serial(&self) -> u64;
}

impl Sgtin96 for num_bigint::BigUint {
    fn header(&self) -> u8 {
        let value: num_bigint::BigUint = (self >> 92) & num_bigint::BigUint::from(bitmask(96 - 92));
        // PANIC: this can't happen due to the bitmask
        value.to_u8().unwrap()
    }

    fn item_reference(&self) -> u32 {
        let value: num_bigint::BigUint = (self >> 38) & num_bigint::BigUint::from(bitmask(58 - 38));
        // PANIC: this can't happen due to the bitmask
        value.to_u32().unwrap()
    }

    fn serial(&self) -> u64 {
        let value: num_bigint::BigUint = self & num_bigint::BigUint::from(bitmask(38));
        // PANIC: this can't happen due to the bitmask
        value.to_u64().unwrap()
    }
}

/// Based on: <https://www.reddit.com/r/rust/comments/jpg0pp/comment/gbeusao/?utm_source=share&utm_medium=web2x&context=3>
pub trait FnHelper<'a, P, O, C> {
    type R: core::future::Future<Output = O> + 'a;

    fn call(&self, val: &'a mut P, ctx: &'a mut C) -> Self::R;
}

impl<'a, R, F, P, O, C> FnHelper<'a, P, O, C> for F
where
    R: core::future::Future<Output = O> + 'a,
    F: Fn(&'a mut P, &'a mut C) -> R,
    P: 'a,
    C: 'a,
{
    type R = R;

    fn call(&self, val: &'a mut P, ctx: &'a mut C) -> Self::R {
        (self)(val, ctx)
    }
}

bitflags! {
    pub struct Ds: u8 {
        const LOCAL_USE = 0b00000011;
        const USE_NETWORK_KEY = 0b00000100;
        const PORTFILTER_ENABLED = 0b00001000;
    }
}

pub struct TrafficClass(u8);

const DS_BITS: u8 = 0b11111100;
const DS_SHIFT: u8 = 2;

impl TrafficClass {
    pub fn new(ds: Ds) -> Self {
        let mut o = Self(0);
        o.set_ds(ds);
        o
    }

    // even if we never use it, this function is useful to understand the code
    #[allow(dead_code)]
    pub fn ds(&self) -> Option<Ds> {
        Ds::from_bits((self.0 & DS_BITS) >> DS_SHIFT)
    }

    pub fn set_ds(&mut self, ds: Ds) {
        let ds = ds.bits();

        self.0 = (self.0 & !DS_BITS) | (ds << DS_SHIFT);
    }

    pub fn raw(&self) -> u8 {
        self.0
    }
}

impl From<TrafficClass> for u8 {
    fn from(traffic_class: TrafficClass) -> Self {
        traffic_class.raw()
    }
}

impl From<u8> for TrafficClass {
    fn from(raw: u8) -> Self {
        Self(raw)
    }
}

const IPV6_FLOWINFO_MASK: u32 = 0x0FFFFFFF_u32.to_be();
const IPV6_FLOWLABEL_MASK: u32 = 0x000FFFFF_u32.to_be();
const IPV6_TCLASS_MASK: u32 = IPV6_FLOWINFO_MASK & !IPV6_FLOWLABEL_MASK;
const IPV6_TCLASS_SHIFT: u32 = 20;

/// Raw flowinfo as received by [std::net::SocketAddrV6::flowinfo]
pub struct Flowinfo(u32);

impl Flowinfo {
    pub fn new(traffic_class: TrafficClass) -> Self {
        // NOTE: implement this properly
        let flowlabel = 0u32;

        Self(((traffic_class.0 as u32) << IPV6_TCLASS_SHIFT).to_be() | flowlabel)
    }

    pub fn traffic_class(self) -> TrafficClass {
        TrafficClass((u32::from_be(self.0 & IPV6_TCLASS_MASK) >> IPV6_TCLASS_SHIFT) as u8)
    }

    pub fn raw(&self) -> u32 {
        self.0
    }
}

impl std::convert::From<u32> for Flowinfo {
    fn from(flow_info: u32) -> Self {
        Self(flow_info)
    }
}

impl From<Flowinfo> for u32 {
    fn from(flowinfo: Flowinfo) -> Self {
        flowinfo.0
    }
}

#[single_trait_impl]
impl SocketAddrEx for std::net::SocketAddr {
    fn flowinfo(&self) -> Flowinfo {
        match self {
            std::net::SocketAddr::V4(_) => 0,
            std::net::SocketAddr::V6(addr) => addr.flowinfo(),
        }
        .into()
    }

    fn set_flowinfo<F: Into<u32>>(&mut self, new_flowinfo: F) -> Result<(), anyhow::Error> {
        match self {
            std::net::SocketAddr::V4(_) => Err(anyhow!("IPv4 doesn't support flowinfo")),
            std::net::SocketAddr::V6(addr) => {
                addr.set_flowinfo(new_flowinfo.into());
                Ok(())
            }
        }
    }

    fn encrypted(&self) -> bool {
        self.flowinfo().traffic_class().ds().map_or_else(
            || false,
            |ds| {
                // NOTE: this assumes that this function is only used on
                //       incoming packets.
                if ds.contains(Ds::PORTFILTER_ENABLED) {
                    tracing::warn!("message with portfilter enable-bit set");
                }
                ds.contains(Ds::LOCAL_USE) && ds.contains(Ds::USE_NETWORK_KEY)
            },
        )
    }
}

#[single_trait_impl]
impl<T, E> ResultEx<T, E> for Result<T, E> {
    /// Stable version of <https://github.com/rust-lang/rust/issues/91345>
    fn s_inspect<F: FnOnce(&T)>(self, f: F) -> Self {
        if let Ok(ref t) = self {
            f(t);
        }

        self
    }

    /// Stable version of <https://github.com/rust-lang/rust/issues/91345>
    fn s_inspect_err<F: FnOnce(&E)>(self, f: F) -> Self {
        if let Err(ref e) = self {
            f(e);
        }

        self
    }
}

#[single_trait_impl]
impl<T> OptionEx<T> for Option<T> {
    /// Stable version of <https://github.com/rust-lang/rust/issues/91345>
    fn s_inspect<F: FnOnce(&T)>(self, f: F) -> Self {
        if let Some(ref x) = self {
            f(x);
        }

        self
    }
}

#[single_trait_impl]
impl<T: std::io::Read> ReadExt for T {
    fn read_u32_le(&mut self) -> std::io::Result<u32> {
        let mut buf = [0; 4];

        self.read_exact(&mut buf)?;

        Ok(u32::from_le_bytes(buf))
    }

    fn read_u16_le(&mut self) -> std::io::Result<u16> {
        let mut buf = [0; 2];

        self.read_exact(&mut buf)?;

        Ok(u16::from_le_bytes(buf))
    }

    fn read_u8(&mut self) -> std::io::Result<u8> {
        let mut buf = [0; 1];

        self.read_exact(&mut buf)?;

        Ok(buf[0])
    }
}

#[single_trait_impl]
impl<T: AsRef<[u8]>> CursorExt for std::io::Cursor<T> {
    fn position_usize(&self) -> Result<usize, anyhow::Error> {
        self.position()
            .try_into()
            .context("cursor position doesn't fit into usize")
    }

    fn remaining(&self) -> Result<u64, anyhow::Error> {
        let total: u64 = self.get_ref().as_ref().len().try_into()?;
        Ok(total - self.position())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_log::test]
    fn rangechunks() {
        let mut iter = (2..10).chunks(2);
        assert_eq!(iter.next(), Some(2..4));
        assert_eq!(iter.next(), Some(4..6));
        assert_eq!(iter.next(), Some(6..8));
        assert_eq!(iter.next(), Some(8..10));
        assert_eq!(iter.next(), None);

        let mut iter = (2..10).chunks(3);
        assert_eq!(iter.next(), Some(2..5));
        assert_eq!(iter.next(), Some(5..8));
        assert_eq!(iter.next(), Some(8..10));

        let mut iter = (2..10).chunks(100);
        assert_eq!(iter.next(), Some(2..10));
    }

    /// There used to be a bug where we ignored the most significant but if every value
    /// verify that this is fixed by checking against a sgtin with all 1s
    #[test_log::test]
    fn sgtin_bitmask() {
        let sgtin =
            num_bigint::BigUint::from_bytes_be(&hex::decode("FFFFFFFFFFFFFFFFFFFFFFFF").unwrap());
        assert_eq!(sgtin.header(), 0b1111);
        assert_eq!(sgtin.item_reference(), 0b11111111111111111111);
        assert_eq!(sgtin.serial(), 0b11111111111111111111111111111111111111);
    }
}
