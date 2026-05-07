#![allow(unused_imports)]

pub mod auth;
pub mod bundle;
pub mod client;
pub mod cmaf;
pub mod endpoints;
pub mod error;
pub mod link_resolver;
pub mod performers;

// Re-export main types
pub use bundle::BundleTokens;
pub use client::QobuzClient;
pub use cmaf::{
    decrypt_segments_into, download_full as cmaf_download_full,
    download_full_with_progress as cmaf_download_full_with_progress,
    download_raw as cmaf_download_raw,
    download_raw_with_progress as cmaf_download_raw_with_progress,
    setup_streaming as cmaf_setup_streaming, CmafProgressCallback, CmafProgressUpdate,
    CmafRawBundle, CmafStreamingInfo, CMAF_PREFETCH_CONCURRENCY,
};
pub use error::{ApiError, Result};
pub use link_resolver::{resolve_link, LinkResolverError, ResolvedLink};
