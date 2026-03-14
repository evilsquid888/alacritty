//! tmux control mode integration.
//!
//! This module provides support for running Alacritty as a tmux control mode
//! client (`tmux -CC`). Each tmux pane is represented by a virtual PTY
//! ([`TmuxPty`]) connected to a shared controller that manages the tmux
//! process.

pub mod protocol;
pub mod pty;

pub use pty::{PaneHandle, TmuxPty, TmuxPtyCmd, create_pane_pair};
