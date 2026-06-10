//! In-process cost estimator: fetch + crunch witness data and execute SP1 ranges,
//! with on-disk caching of both `WitnessData` and `SP1Stdin`.

pub mod error;

pub use error::EstimatorError;
