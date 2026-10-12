//! Runtime object model for Catallaxy: NaN-boxed [`Value`]s, class descriptors, heap object
//! layouts, the three ownership states (owned, frozen, shared) and structural operations.
//!
//! All `unsafe` code in the language implementation lives here and in `catallaxy-gc`.

pub mod class;
pub mod dict;
pub mod mutator;
pub mod object;
pub mod ops;
pub mod ownership;
pub mod symbol;
pub mod value;

pub use class::{class_of, Class, ObjKind, BUILTINS};
pub use mutator::{note_external, panic, Handle, Mutator, RtResult, Trap};
pub use ops::Interop;
pub use symbol::{intern, Sym};
pub use value::Value;
