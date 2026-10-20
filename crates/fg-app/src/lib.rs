//! The fastgrid desktop app.
//!
//! * [`model`]: UI logic with no GPUI dependency (cell formatting, paging, sorting, fuzzy search,
//!   tab bookkeeping, the object tree). Plain Rust, unit-tested without a window.
//! * [`views`]: GPUI views that render those models and call `fg-service`.
//!
//! Keeping the logic out of the views is what lets it be tested headlessly.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod app_state;
pub mod controller;
pub mod model;
pub mod views;
