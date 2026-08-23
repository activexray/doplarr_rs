//! Minimal hand-written client for the Sportarr API.
//!
//! Sportarr does not publish an OpenAPI document for its native API, so
//! unlike the generated `sonarr_api`/`radarr_api`/`seerr_api` crates this
//! is a small hand-written subset covering exactly what the request flow
//! needs: league catalog search, the already-added league list, quality
//! profiles, root folders, and adding a league.

pub mod apis;
pub mod models;
