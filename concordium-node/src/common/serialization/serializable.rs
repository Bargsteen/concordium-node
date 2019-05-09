use crate::common::serialization::WriteArchive;

use concordium_common::UCursor;
use failure::Fallible;

use std::{collections::HashSet, ops::Deref};

pub trait Serializable<T: ?Sized = Self> {
    fn serialize<A>(&self, archive: &mut A) -> Fallible<()>
    where
        A: WriteArchive;
}

// Basic types
// ==============================================================================================

impl Serializable for u8 {
    #[inline]
    fn serialize<A>(&self, archive: &mut A) -> Fallible<()>
    where
        A: WriteArchive, {
        archive.write_u8(*self)
    }
}

impl Serializable for u16 {
    #[inline]
    fn serialize<A>(&self, archive: &mut A) -> Fallible<()>
    where
        A: WriteArchive, {
        archive.write_u16(*self)
    }
}

impl Serializable for String {
    #[inline]
    fn serialize<A>(&self, archive: &mut A) -> Fallible<()>
    where
        A: WriteArchive, {
        archive.write_str(self.as_str())
    }
}

impl<T> Serializable for &T
where
    T: Serializable,
{
    #[inline]
    fn serialize<A>(&self, archive: &mut A) -> Fallible<()>
    where
        A: WriteArchive, {
        (*self).serialize(archive)
    }
}

impl<T> Serializable for Box<T>
where
    T: Serializable,
{
    #[inline]
    fn serialize<A>(&self, archive: &mut A) -> Fallible<()>
    where
        A: WriteArchive, {
        self.deref().serialize(archive)
    }
}

// Std common types
// ==============================================================================================

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

impl Serializable for Ipv4Addr {
    #[inline]
    fn serialize<A>(&self, archive: &mut A) -> Fallible<()>
    where
        A: WriteArchive, {
        archive.write_u8(4u8)?;
        archive.write_all(&self.octets())?;
        Ok(())
    }
}

impl Serializable for Ipv6Addr {
    #[inline]
    fn serialize<A>(&self, archive: &mut A) -> Fallible<()>
    where
        A: WriteArchive, {
        archive.write_u8(6u8)?;
        for segment in &self.segments() {
            archive.write_u16(*segment)?;
        }
        Ok(())
    }
}

impl Serializable for IpAddr {
    fn serialize<A>(&self, archive: &mut A) -> Fallible<()>
    where
        A: WriteArchive, {
        match self {
            IpAddr::V4(ip4) => ip4.serialize(archive),
            IpAddr::V6(ip6) => ip6.serialize(archive),
        }
    }
}

use std::net::SocketAddr;
impl Serializable for SocketAddr {
    #[inline]
    fn serialize<A>(&self, archive: &mut A) -> Fallible<()>
    where
        A: WriteArchive, {
        self.ip().serialize(archive)?;
        archive.write_u16(self.port())
    }
}

// Standard collections
// ==============================================================================================

#[inline]
fn serialize_from_iterator<I, A, T>(iterator: I, archive: &mut A) -> Fallible<()>
where
    I: Iterator<Item = T>,
    T: Serializable,
    A: WriteArchive, {
    for v in iterator {
        v.serialize(archive)?;
    }
    Ok(())
}

impl<T, S: ::std::hash::BuildHasher> Serializable for HashSet<T, S>
where
    T: Serializable,
{
    #[inline]
    fn serialize<A>(&self, archive: &mut A) -> Fallible<()>
    where
        A: WriteArchive, {
        archive.write_u32(self.len() as u32)?;
        serialize_from_iterator(self.iter(), archive)
    }
}

impl<T> Serializable for Vec<T>
where
    T: Serializable,
{
    #[inline]
    fn serialize<A>(&self, archive: &mut A) -> Fallible<()>
    where
        A: WriteArchive, {
        archive.write_u32(self.len() as u32)?;
        serialize_from_iterator(self.iter(), archive)
    }
}

// Concordium-common
// ==============================================================================================

impl Serializable for UCursor {
    /// It makes a `deep-copy` of the `UCursor` into `Archive`.
    fn serialize<A>(&self, archive: &mut A) -> Fallible<()>
    where
        A: WriteArchive, {
        let mut self_from = self.sub(self.position())?;
        let self_from_len = self_from.len();

        archive.write_u64(self_from_len)?;
        std::io::copy(&mut self_from, archive)?;

        Ok(())
    }
}
