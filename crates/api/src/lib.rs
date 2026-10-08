//! api: `POST /search` and `GET /health` (spec §7).

pub mod query;
pub mod server;

pub use server::{router, AppState};
