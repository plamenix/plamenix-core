//! `FirebirdConnection` implementation for the native fbclient

mod connection;
#[cfg(feature = "dynamic_loading")]
pub mod crypt;
pub(crate) mod ibase;
pub(crate) mod params;
pub(crate) mod row;
pub(crate) mod status;
pub(crate) mod varchar;
pub(crate) mod xsqlda;

pub use connection::{NativeFbAttachmentConfig, NativeFbClient, RemoteConfig};

pub use connection::{DynLink, DynLoad, LinkageMarker};
