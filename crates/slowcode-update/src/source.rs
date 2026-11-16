//! The release transport seam.
//!
//! `slowcode-update` never calls the network directly from its logic. Callers
//! pass a [`UpdateTransport`]; production uses [`crate::client::HttpTransport`]
//! and tests use a fake that records calls and returns canned bytes.

use std::{future::Future, pin::Pin};

use crate::{error::UpdateError, version::Version};

/// A boxed future used to keep the transport object-safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One published release.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Release {
    /// The raw tag, used to build asset URLs.
    pub tag: String,
    /// The parsed tag.
    pub version: Version,
}

/// Read the latest release and download its assets.
pub trait UpdateTransport: Send + Sync {
    /// The latest published release.
    fn latest_release(&self) -> BoxFuture<'_, Result<Release, UpdateError>>;

    /// Download one release asset by URL.
    fn download(&self, url: &str) -> BoxFuture<'_, Result<Vec<u8>, UpdateError>>;

    /// Build the URL of one asset for a tag.
    fn asset_url(&self, tag: &str, file: &str) -> Result<String, UpdateError>;
}
