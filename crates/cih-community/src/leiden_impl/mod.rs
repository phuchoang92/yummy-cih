//! Vendored Leiden community-detection implementation.
//!
//! This module keeps the full algorithm API (quality functions, partition
//! accessors, seeded runs) even where CIH only drives a subset of it, and
//! keeps literature naming (CPM, RBER) — hence the module-wide allows.
#![allow(dead_code)]
#![allow(clippy::upper_case_acronyms)]

pub(crate) mod algorithm;
pub(crate) mod builder;
pub(crate) mod error;
pub(crate) mod graph_data;
pub(crate) mod leiden;
pub(crate) mod move_components;
pub(crate) mod parallel;
pub(crate) mod partition;
pub(crate) mod quality;

pub(crate) use builder::GraphDataBuilder;
pub(crate) use leiden::{Leiden, LeidenConfig, QualityType};
