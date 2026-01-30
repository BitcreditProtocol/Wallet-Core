pub mod types;
pub mod utils;

pub type TStamp = chrono::DateTime<chrono::Utc>;

pub trait SendSync: Send + Sync {}

impl<T> SendSync for T where T: Send + Sync {}
