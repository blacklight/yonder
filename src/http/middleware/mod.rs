//! HTTP middleware: authentication, client-IP resolution, rate
//! limiting. Request tracing and CORS come from tower-http and are
//! assembled in [`crate::http::build_router`].

pub mod auth;
pub mod client_ip;
pub mod ratelimit;
