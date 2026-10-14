//! In-process cost estimator: fetch + crunch witness data and execute SP1 ranges,
//! with on-disk caching of both `WitnessData` and `SP1Stdin`.

pub mod cache;
pub mod error;
pub mod estimator;
pub mod memory;
pub mod retry;
pub mod stats;

pub use cache::{DaType, WitnessCache};
pub use error::EstimatorError;
pub use estimator::Estimator;
pub use retry::network_call_with_timeout;
pub use stats::aggregate_execution_stats;
