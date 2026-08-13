//! Independently implemented terminal-emulation state for bed.
//!
//! This crate parses output produced by a child attached to a PTY or ConPTY.
//! It deliberately does not own processes, encode user input, or render a UI.

#![forbid(unsafe_code)]

mod parser;
mod screen;

pub use parser::TerminalEmulator;
pub use screen::{Attributes, Cell, Color, Cursor, Row, Screen, TerminalModes};
