//! Platform-independent network policy. No peer transport or OS sockets live here.
pub mod config;
pub mod packet;
pub mod storage;
pub mod wire;

#[cfg(test)]
mod tests;
