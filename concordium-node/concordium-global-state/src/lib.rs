#[macro_use]
extern crate log;

// (de)serialization macros

macro_rules! check_serialization {
    ($target:expr, $cursor:expr) => {
        debug_assert_eq!(
            $cursor.position(),
            $cursor.get_ref().len() as u64,
            "Invalid deserialization of {:?}",
            $target
        );

        debug_assert_eq!(
            &&*$target.serialize(),
            $cursor.get_ref(),
            "Invalid serialization of {:?}",
            $target
        );
    };
}

macro_rules! read_const_sized {
    ($source:expr, $size:expr) => {{
        let mut buf = [0u8; $size as usize];
        $source.read_exact(&mut buf)?;

        buf
    }};
}

macro_rules! read_sized {
    ($source:expr, $size:expr) => {{
        let mut buf = vec![0u8; $size as usize];
        $source.read_exact(&mut buf)?;

        buf.into_boxed_slice()
    }};
}

macro_rules! safe_get_len {
    ($source:expr, $object:expr) => {{
        let raw_len = NetworkEndian::read_u64(&read_const_sized!($source, 8)) as usize;
        failure::ensure!(
            raw_len <= ALLOCATION_LIMIT,
            "The {} ({}) exceeds the safety limit!",
            $object,
            raw_len
        );
        raw_len
    }};
}

pub mod block;
pub mod common;
pub mod finalization;
pub mod parameters;
pub mod transaction;
pub mod tree;
