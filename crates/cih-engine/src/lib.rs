#[doc(hidden)]
pub const DEFAULT_FALKOR_URL: &str = "redis://127.0.0.1:6380";
#[doc(hidden)]
pub const DEFAULT_GRAPH_KEY: &str = "cih";

pub mod analyze;
pub mod db;
pub mod discover;
pub mod embed;
pub mod feature_strategy;
pub mod features_cmd;
pub mod file_cache;
pub mod group_cmd;
pub mod group_sync;
pub mod llm;
pub mod registry;
pub mod runtime;
pub mod scan;
pub mod scope;
pub mod start;
pub mod start_env;
pub mod tui;
pub mod ui;
pub mod versioning;
pub mod wiki;
