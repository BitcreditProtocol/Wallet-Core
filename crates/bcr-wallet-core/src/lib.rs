use thiserror::Error;

pub mod contact;
pub mod event;
pub mod name;
pub mod types;
pub mod util;

pub trait SendSync: Send + Sync {}

impl<T> SendSync for T where T: Send + Sync {}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ValidationError {
    #[error("Name can't be empty")]
    EmptyName,
    #[error("Name is invalid")]
    InvalidName,
}
