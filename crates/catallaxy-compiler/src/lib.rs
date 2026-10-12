//! The Catallaxy compiler: lexer, parser, type checker and bytecode emitter.

pub mod ast;
pub mod check;
pub mod diag;
pub mod lexer;
pub mod parser;
pub mod types;
pub mod util;
pub mod codegen;
pub mod driver;

pub use driver::{compile_path, compile_source, Compilation, Options};
