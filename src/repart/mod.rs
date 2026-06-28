//! `repart.d` definition model and generation.
//!
//! [`model`] renders a single partition definition to INI text; [`generate`]
//! assembles the full fixed + configurable set under `/run`.
//!
//! Consumed by the Sizing/Review screens in a later phase; the model and
//! generation API is complete now ahead of that wiring.
#![allow(dead_code)]

pub mod generate;
pub mod model;
pub mod runner;
