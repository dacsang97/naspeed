//! Language-neutral action model: `ActionSpec` describes one cacheable
//! execution, `ActionResult` its recorded outcome, and `action_key` the
//! canonical encoding and domain-separated key derivation both sides agree
//! on. Nothing here is Rust- or cargo-specific.

pub mod action;
pub mod action_key;
pub mod action_result;
pub mod digest_memo;
pub mod graph;
pub mod output_view;
pub mod protocol;
pub(crate) mod rust;
pub mod service;
pub mod staging;
