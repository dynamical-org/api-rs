//! Pure Python-compatible presentation and ForecastBatch validation.
pub mod json;
mod numeric;
pub mod output;
mod printable;
pub mod validation;
pub use json::Value;
pub use validation::{validate_forecast, ForecastBatch, ForecastQuery, Response, ValidationResult};

pub use speedate::{Date, DateTime, Time};

pub use validation::{
    validate_analysis, validate_forecast_runs, AnalysisBatch, AnalysisQuery, ForecastRunsQuery,
};
