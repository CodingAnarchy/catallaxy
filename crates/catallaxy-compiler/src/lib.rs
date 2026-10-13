//! The Catallaxy compiler: lexer, parser, type checker, bytecode emitter and formatter.

// Parse failures are reported through the diagnostics list; `Err(())` just unwinds.
#![allow(clippy::result_unit_err)]

pub mod ast;
pub mod check;
pub mod codegen;
pub mod diag;
pub mod driver;
pub mod fmt;
pub mod lexer;
pub mod parser;
pub mod query;
pub mod types;
pub mod util;

pub use driver::{compile_path, compile_source, Compilation, Options};
