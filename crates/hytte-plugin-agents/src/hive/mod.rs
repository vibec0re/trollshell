//! The hive client: a typed mirror of `host.sock`'s wire ([`wire`]) plus the
//! one-connection-per-request round trip over it ([`client`]).
//!
//! Nothing in here links a hyperhive crate — see [`wire`]'s module docs for
//! why, and for the `file:line` map back to the structs it mirrors.

pub mod client;
pub mod wire;

pub use client::{HiveError, request};
pub use wire::{
    AgentStatusRow, DEFAULT_SOCKET, HOST_SOCK_VERSION, HiveUrls, Request, Response, Scope,
    VersionMismatch, check_version,
};
