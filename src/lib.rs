//! reqrun — run JetBrains-style `.http` request files with assertions.
//!
//! The crate is organized as pure, unit-testable modules with a thin CLI on
//! top: `parser` reads the `.http` grammar, `vars` resolves variables and
//! environments, `http` speaks HTTP/1.1 over `std::net`, `script` interprets
//! the response-handler subset, `runner` orchestrates, `report` renders.

pub mod cli;
pub mod http;
pub mod json;
pub mod parser;
pub mod report;
pub mod runner;
pub mod script;
pub mod url;
pub mod vars;
