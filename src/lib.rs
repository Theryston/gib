//! GIB backup library.
//!
//! The library target is intentionally independent from the command-line
//! interface. It performs no terminal I/O, prompting, process termination, or
//! global output configuration. Applications can observe typed events by
//! installing a callback on [`GibBuilder`].

pub mod api;

mod config;
mod core;
mod storage;
mod utils;

pub use api::{FS, Gib, GibBuilder, GibError, GibEvent, MemoryFS};
