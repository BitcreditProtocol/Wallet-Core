pub mod types;
pub mod util;

pub trait SendSync: Send + Sync {}

impl<T> SendSync for T where T: Send + Sync {}
