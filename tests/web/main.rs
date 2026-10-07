//! `uscope web` scenarios: the real server driven over HTTP and `WebSockets`
//! the way the page drives it.

#[path = "../support/mod.rs"]
mod support;
#[path = "../support/web.rs"]
mod web;

mod access;
mod sessions;
