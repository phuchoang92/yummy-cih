//! Thin shim: the sync core lives in the lib-layer [`crate::group_sync`]
//! module so the registry persistence hooks can call it without the CLI layer.

pub use crate::group_sync::*;
