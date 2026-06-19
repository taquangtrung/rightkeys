//! Backend-shared helpers for window actions. Currently the portable
//! pick-window overlay logic (hint generation, the prefix navigator, title
//! parsing, and chip placement); rendering and window enumeration stay
//! per-backend.

pub mod pickelement;
pub mod pickwindow;
