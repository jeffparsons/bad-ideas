//! A dependency-free sketch of a heterogeneous **call-builder** API for *dynamic*
//! component calls — modelling the design discussed for
//! [`bytecodealliance/wasmtime#13788`](https://github.com/bytecodealliance/wasmtime/issues/13788).
//!
//! It is not wasmtime; [`fake_wasmtime`] is a tiny stand-in that reproduces the same
//! borrow structure a real embedding has, so this crate can *prove* — by compiling and
//! running — that the ergonomics compose. In particular:
//!
//! 1. A [`Prepared`](call_builder::Prepared) call plan is validated once and holds no
//!    argument-buffer borrows, so it can be reused across a whole loop.
//! 2. Each [`CallBuilder`](call_builder::CallBuilder) borrows its argument buffers only
//!    for the single `invoke`, so the buffers are freely mutable between calls.
//!
//! # Reuse compiles and runs
//!
//! ```
//! use call_builder_sim::call_builder::{ArgSpec, Prepared};
//! use call_builder_sim::fake_wasmtime::{Func, Store, Ty};
//!
//! // A no-op guest taking one read-write `list<f32>`.
//! let func = Func::new(vec![Ty::List(Box::new(Ty::F32))], |_store, _core| {});
//! let prepared = Prepared::new(&func, &[ArgSpec::FlatInOut]).unwrap();
//! let mut store = Store::new(1024);
//!
//! let mut buf = vec![0u8; 16];
//! for i in 0..3u8 {
//!     prepared.call().arg_flat_inout(&mut buf).invoke(&mut store).unwrap();
//!     buf[0] = i; // freely mutate the buffer *between* calls — the builder is gone
//! }
//! ```
//!
//! # Misuse does not compile
//!
//! Holding the builder (and thus the borrow) across a mutation of the same buffer is a
//! compile error — the safety is enforced by the type system, not by convention:
//!
//! ```compile_fail
//! use call_builder_sim::call_builder::{ArgSpec, Prepared};
//! use call_builder_sim::fake_wasmtime::{Func, Store, Ty};
//!
//! let func = Func::new(vec![Ty::List(Box::new(Ty::F32))], |_store, _core| {});
//! let prepared = Prepared::new(&func, &[ArgSpec::FlatInOut]).unwrap();
//! let mut store = Store::new(1024);
//!
//! let mut buf = vec![0u8; 16];
//! let builder = prepared.call().arg_flat_inout(&mut buf);
//! buf[0] = 1;                         // ERROR: `buf` is already mutably borrowed by `builder`
//! builder.invoke(&mut store).unwrap();
//! ```

pub mod call_builder;
pub mod fake_wasmtime;
