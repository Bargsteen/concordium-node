#![recursion_limit = "1024"]

#[macro_use]
extern crate log;
#[cfg(not(target_os = "windows"))]
extern crate get_if_addrs;

cfg_if! {
    if #[cfg(feature = "elastic_logging")] {
        #[macro_use]
        extern crate elastic_derive;
    }
}

cfg_if! {
    if #[cfg(feature = "instrumentation")] {
        #[macro_use]
        extern crate prometheus;
        #[macro_use]
        extern crate gotham_derive;
        extern crate hyper;
        extern crate mime;
    }
}

#[macro_use]
extern crate cfg_if;
#[cfg(target_os = "windows")]
extern crate ipconfig;

#[macro_use]
extern crate failure;

#[macro_use]
#[cfg(all(test, not(feature = "s11n_capnp")))]
extern crate quickcheck;

#[macro_use]
extern crate concordium_common;

#[cfg(feature = "s11n_serde")]
#[macro_use]
extern crate serde_derive;

#[cfg(feature = "s11n_serde_cbor")]
extern crate serde_cbor;

#[cfg(feature = "s11n_capnp")]
extern crate capnp;

#[cfg(feature = "s11n_fbs")]
extern crate flatbuffers;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const APPNAME: &str = env!("CARGO_PKG_NAME");
const DEFAULT_DNS_PUBLIC_KEY: &str =
    "58C4FD93586B92A76BA89141667B1C205349C6C38CC8AB2F6613F7483EBFDAA3";
const ENV_DNS_PUBLIC_KEY: Option<&str> = option_env!("CORCORDIUM_PUBLIC_DNS_KEY");
pub fn get_dns_public_key() -> &'static str { ENV_DNS_PUBLIC_KEY.unwrap_or(DEFAULT_DNS_PUBLIC_KEY) }

pub mod common;
pub mod configuration;
pub mod connection;

pub mod network;
pub mod p2p;
pub mod plugins;

pub mod dumper;
pub mod rpc;
pub mod stats_engine;
pub mod stats_export_service;
pub mod utils;

pub mod test_utils;

#[cfg(feature = "s11n_capnp")]
pub mod p2p_capnp;

#[cfg(feature = "s11n_fbs")]
pub mod flatbuffers_shim;
