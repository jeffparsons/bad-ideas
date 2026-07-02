//! A dependency-free sketch of the **canonical-ABI transfer surfaces** discussed in
//! [`bytecodealliance/wasmtime#13788`](https://github.com/bytecodealliance/wasmtime/issues/13788)
//! and issue #13. It models the full 2x2 matrix — host↔guest × params/results — on top of
//! two primitives, *bulk lower* (host bytes → guest memory) and *bulk lift* (guest memory
//! → host bytes/view):
//!
//! | | params (in) | results (out) |
//! | --- | --- | --- |
//! | **host → guest** (export) | [`call_builder`]: `PreparedCall` / `BoundCall` — bulk **lower** | [`call_builder`]: `invoke_scoped` / `invoke_collect` — bulk **lift** |
//! | **guest → host** (import) | [`fake_wasmtime::ImportCall::arg_list_view`] — bulk **lift** | [`fake_wasmtime::ImportCall::return_flat_list`] — bulk **lower** |
//!
//! It is not wasmtime; [`fake_wasmtime`] is a tiny stand-in that reproduces the same
//! borrow structure a real embedding has, so this crate can *prove* — by compiling and
//! running — that the ergonomics compose. In particular:
//!
//! 1. A [`PreparedCall`](call_builder::PreparedCall) call shape is validated once and
//!    holds no argument-buffer borrows, so it can be reused across a whole loop.
//! 2. Each [`BoundCall`](call_builder::BoundCall) borrows its argument buffers only for
//!    the single `invoke`, so the buffers are freely mutable between calls.
//! 3. Borrowed *result* views (host→guest results, and guest→host params) are confined to
//!    a Wasmtime-controlled scope and cannot escape it.
//!
//! # Reuse compiles and runs
//!
//! ```
//! use call_builder_sim::call_builder::{ArgSpec, PreparedCall};
//! use call_builder_sim::fake_wasmtime::{Func, Store, Ty};
//!
//! // A no-op guest taking one read-write `list<f32>`, returning nothing.
//! let func = Func::new(vec![Ty::List(Box::new(Ty::F32))], vec![], |_caller, _core| vec![]);
//! let prepared = PreparedCall::new(&func, &[ArgSpec::FlatInOut]).unwrap();
//! let mut store = Store::new(1024);
//!
//! let mut buf = vec![0u8; 16];
//! for i in 0..3u8 {
//!     prepared.bind().arg_flat_inout(&mut buf).invoke(&mut store).unwrap();
//!     buf[0] = i; // freely mutate the buffer *between* calls — the binding is gone
//! }
//! ```
//!
//! # Misuse does not compile
//!
//! Holding the binding (and thus the borrow) across a mutation of the same buffer is a
//! compile error — the safety is enforced by the type system, not by convention:
//!
//! ```compile_fail
//! use call_builder_sim::call_builder::{ArgSpec, PreparedCall};
//! use call_builder_sim::fake_wasmtime::{Func, Store, Ty};
//!
//! let func = Func::new(vec![Ty::List(Box::new(Ty::F32))], vec![], |_caller, _core| vec![]);
//! let prepared = PreparedCall::new(&func, &[ArgSpec::FlatInOut]).unwrap();
//! let mut store = Store::new(1024);
//!
//! let mut buf = vec![0u8; 16];
//! let bound = prepared.bind().arg_flat_inout(&mut buf);
//! buf[0] = 1;                         // ERROR: `buf` is already mutably borrowed by `bound`
//! bound.invoke(&mut store).unwrap();
//! ```
//!
//! # A borrowed result view cannot escape its scope
//!
//! `invoke_scoped` yields zero-copy views valid only for the duration of the closure.
//! Trying to return one out of the scope does not compile:
//!
//! ```compile_fail
//! use call_builder_sim::call_builder::{ArgSpec, PreparedCall};
//! use call_builder_sim::fake_wasmtime::{CoreVal, Func, Store, Ty};
//!
//! // A guest that returns a one-element `list<f32>`.
//! let func = Func::new(vec![], vec![Ty::List(Box::new(Ty::F32))], |caller, _core| {
//!     let s = caller.store();
//!     let ptr = s.realloc(4, 4);
//!     s.write_f32(ptr, 1.0);
//!     vec![CoreVal::I32(ptr as i32), CoreVal::I32(1)]
//! });
//! let prepared = PreparedCall::new(&func, &[]).unwrap();
//! let mut store = Store::new(1024);
//!
//! // ERROR: the view borrows the scope; it cannot be returned out of the closure.
//! let escaped = prepared.bind().invoke_scoped(&mut store, |r| r.flat_list_view(0)).unwrap();
//! let _ = escaped;
//! ```

pub mod call_builder;
pub mod fake_wasmtime;
