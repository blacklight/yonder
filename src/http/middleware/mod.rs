//! HTTP middleware: authentication, client-IP resolution, rate
//! limiting, CORS. Request tracing comes from tower-http and is
//! assembled in [`crate::http::build_router`].

pub mod auth;
pub mod client_ip;
pub mod cors;
pub mod ratelimit;
