//! `aether-rpc`: the typed ends of the ring.
//!
//! [`Client`] sends a [`Request`](aether_protocol::Request) and returns its
//! [`Reply`](aether_protocol::Reply): what the shim uses. [`Director`] is a
//! server's answers, one method per opcode, and [`serve_one`] and [`serve`]
//! take requests off a ring and answer them from one: what every Rust-side
//! server uses. Both follow `docs/wire.md` 3.1 and 3.3: a reply payload is
//! meaningful only on `Status::Ok`, an error status goes with an empty
//! payload, and a reserved or unknown opcode or a payload that does not
//! decode is answered `Status::BadRequest`.
//!
//! A [`Tap`] on the serve loop ([`serve_tapped`]) sees each request the ring
//! hands over, and its reply, as bytes and may answer early or rewrite the
//! reply: the hook the shim's tests record and corrupt answers through.
//! Untapped, it is `()` and costs nothing.
//!
//! Neither end adds an allocation to the ring's own: the client encodes the
//! request straight into its slot and decodes the reply from a buffer the
//! caller keeps, and the server reuses the buffers in a [`Scratch`].

#![forbid(unsafe_code)]

mod client;
mod serve;

pub use client::{CallError, Client};
pub use serve::{
    serve, serve_one, serve_one_tapped, serve_one_with, serve_one_with_arena, serve_tapped,
    serve_with_arena, Director, Listing, ReadSlot, Scratch, Tap, TapAction,
};
