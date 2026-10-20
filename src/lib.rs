//! Plaza: a multiplayer pixel canvas server in the spirit of r/place.
//!
//! - [`protocol`]: the line-based request/reply protocol.
//! - [`canvas`]: pixels, authorship and the commit counter.
//! - [`persist`]: write-ahead log, snapshots and crash recovery.
//! - [`server`]: sessions, cooldowns, subscriptions and diff fan-out.
//! - `web` / `tcp`: the axum HTTP + WebSocket frontend and the raw TCP protocol.

pub mod canvas;
pub mod color;
pub mod geometry;
pub mod persist;
pub mod protocol;
pub mod server;
mod tcp;
mod web;

pub use server::{Config, MAX_STAMP_DIM, serve};
