#[doc(hidden)]
pub const DEFAULT_FALKOR_URL: &str = "redis://127.0.0.1:6380";
#[doc(hidden)]
pub const DEFAULT_GRAPH_KEY: &str = "cih";

pub mod analyze;
pub mod cmd;
pub mod db;
pub mod decompile;
pub mod decompile_config;
pub mod discover;
pub mod embed;
pub mod feature_strategy;
pub mod file_cache;
pub mod llm;
pub mod node_prefix;
pub mod registry;
pub mod runtime;
pub mod scan;
pub mod scope;
pub mod settings;
pub mod ui;
pub mod versioning;
pub mod wiki;
