//! Read HEVC frames from stock `fetch_av` sockets, inspect and packetize NAL units,
//! or share a stream between bounded subscribers with automatic reconnection.

mod capture;
pub mod hevc;
mod hub;

pub use capture::{Capture, Frame};
pub use hub::Hub;
