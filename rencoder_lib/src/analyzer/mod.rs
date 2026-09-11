//! Video analyzer: probe each file → `AnalysisRecord`, then aggregate
//! into `FpsStats` for the FPS distribution. Serial + rayon-backed
//! parallel probe paths share the same `AnalysisRecord` shape so the
//! CLI's analyze subcommand can format either through `records_to_*`.

pub mod records;
