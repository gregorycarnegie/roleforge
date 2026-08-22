pub mod card;
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

/// Serializes the tests that mutate process environment. `env::set_var` is only
/// sound while no other thread touches the environment, and the test harness
/// runs these modules' tests in parallel.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
