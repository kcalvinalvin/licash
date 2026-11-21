//! A Licash observatory: the hubs and swap providers of one network, found
//! on Nostr, probed over Tor, measured by moving a small balance between
//! them within a monthly budget, and shown on a web page anyone can read.
//!
//! - [`observer`] finds services and probes them, without money.
//! - [`mover`] moves the observatory's balance between them, as often as
//!   [`budget`] allows, and measures each move.
//! - [`view`] computes what is shown from the records in [`store`];
//!   [`web`] serves it as HTML and JSON.

pub mod budget;
pub mod format;
pub mod mover;
pub mod observer;
pub mod probe;
pub mod store;
pub mod terms;
pub mod view;
pub mod web;
