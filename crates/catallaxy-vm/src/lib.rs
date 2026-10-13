//! The Catallaxy virtual machine: a register-based bytecode interpreter, one instance per OS
//! thread, all sharing frozen code and one heap.

pub mod bytecode;
pub mod interp;
pub mod program;
pub mod slow;

pub use bytecode::Program;
/// Format specs, shared with the compiler (which parses and checks them).
pub use catallaxy_runtime::fmtspec;
pub use interp::Vm;
pub use program::{load, NativeFn};
#[cfg(feature = "op-profile")]
pub mod profile;
