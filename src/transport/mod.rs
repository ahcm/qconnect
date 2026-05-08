pub mod config;
pub mod error;
pub mod protocol;
mod transport;

pub use config::WsTransportConfig;
pub use error::WsTransportError;
pub use transport::{NativeWsTransport, TransportEvent, WsTransport};

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QueueVersion
{
    pub major: u64,
    pub minor: u64,
}

impl QueueVersion
{
    pub const fn new(major: u64, minor: u64) -> Self
    {
        Self { major, minor }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem
{
    pub track_context_uuid: String,
    pub track_id: u64,
    pub queue_item_id: u64,
}

impl fmt::Display for QueueItem
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        write!(f, "#{0}", self.queue_item_id)
    }
}
