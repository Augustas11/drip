//! Library surface for the percolator cranker.
//!
//! The binary (`src/main.rs`) and the integration tests under `tests/` link
//! this library, so the crank cycle (`cycle::run_one_cycle`) is one piece of
//! code everywhere.

pub mod binding;
pub mod cycle;
pub mod discovery;
pub mod observation;
pub mod plan_gate;
pub mod quarantine;
pub mod resolved;
pub mod scheduler;
pub mod setup;
pub mod submit;
pub mod work;
