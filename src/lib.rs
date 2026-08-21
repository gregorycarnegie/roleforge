pub mod config;
pub mod matching;
pub mod model;
pub mod sources;
pub mod storage;
pub mod telegram;
pub mod text;
pub mod workflow;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
