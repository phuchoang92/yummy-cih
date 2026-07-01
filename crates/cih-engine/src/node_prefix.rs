//! Node ID prefix constants used throughout the codebase to tag and strip
//! graph node type labels (e.g. `"Method:com.example.Foo#bar/0"`).

pub const METHOD: &str = "Method:";
pub const CONSTRUCTOR: &str = "Constructor:";
pub const CLASS: &str = "Class:";
pub const INTERFACE: &str = "Interface:";
pub const DB_TABLE: &str = "DbTable:";
pub const DB_QUERY: &str = "DbQuery:";
pub const ROUTE: &str = "Route:";
pub const COMMUNITY: &str = "Community:";
