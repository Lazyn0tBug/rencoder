//! Data storage: writers that consume a single canonical Polars
//! `DataFrame` derived from `Vec<EncodingRecord>`.
//!
//! Three writers share the same schema (defined in
//! `encoding::output::records_to_dataframe`):
//!
//! ```text
//! EncodingRecord -> records_to_dataframe (single canonical schema)
//!                          |
//!             +------------+------------+------------+
//!             |            |            |            |
//!       dataframe_to_csv  dataframe_to_jsonl  dataframe_to_parquet
//! ```
//!
//! Re-exports the writers from `encoding::output` so callers can use
//! the `data::store` namespace without breaking the public API.

pub use crate::encoding::output::{
    dataframe_to_csv, dataframe_to_jsonl, dataframe_to_parquet, records_to_csv,
    records_to_dataframe, records_to_jsonl, records_to_parquet,
};
