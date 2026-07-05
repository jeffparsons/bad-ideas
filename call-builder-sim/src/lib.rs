//! A dependency-free sketch of the **`ValSource` provide surface** for dynamic Component
//! Model calls, worked through in [issue #16](https://github.com/jeffparsons/bad-ideas/issues/16)
//! (evolving [#13](https://github.com/jeffparsons/bad-ideas/issues/13) /
//! [`bytecodealliance/wasmtime#13788`](https://github.com/bytecodealliance/wasmtime/issues/13788)).
//!
//! The host provides each argument through a recursive
//! [`ValSource`](call_builder::ValSource) tree — every node independently picks how it
//! supplies its bytes:
//!
//! - [`Val`](fake_wasmtime::Val) — dynamic, owned (the simple case is the all-`Val` tree);
//! - [`Flat`](call_builder::ValSource::Flat) — proof-carrying
//!   [`ValidatedCabiBytes`](fake_wasmtime::ValidatedCabiBytes) (one memcpy);
//! - [`Record`](call_builder::ValSource::Record) / [`List`](call_builder::ListSource) —
//!   per-field / per-element sources (mix representations *within* one value);
//! - [`Lazy`](call_builder::LowerOnDemand) — a host-computed random-access range producer;
//! - [`Arena`](call_builder::Arena) — a slice of a pre-validated, flattened store;
//! - `Stream` — a distinct `stream<T>` node.
//!
//! # The load-bearing idea: same API, both worlds
//!
//! Every source is a **random-access range producer**, so the runtime can drive the *same*
//! tree eagerly (materialise up front — today) or lazily (guest pulls ranges — lazy value
//! lowering, [component-model#383]). Eager is the strict special case: "call every producer
//! once, fully." `src/main.rs` proves this — an archetype column driven both ways is
//! byte-identical, projected-away fields are never touched, and a lazy window pulls only
//! what it asks for.
//!
//! It is not wasmtime; [`fake_wasmtime`] is a tiny stand-in reproducing the borrow structure
//! a real embedding has, so this crate can *prove* — by compiling and running — that the
//! ergonomics compose and that the unsafe usages are compile errors.
//!
//! # Reuse compiles and runs
//!
//! A [`PreparedCall`](call_builder::PreparedCall) validates the shape once and holds no
//! source borrows, so it is reusable while the argument buffer is mutated between calls:
//!
//! ```
//! use call_builder_sim::call_builder::{PreparedCall, ValSpec};
//! use call_builder_sim::fake_wasmtime::{Func, Store, Ty, ValidatedCabiBytes};
//!
//! // A no-op guest taking one `list<u32>`.
//! let func = Func::new(vec![Ty::List(Box::new(Ty::U32))], vec![], |_caller, _core| vec![]);
//! let prepared = PreparedCall::new(&func, &[ValSpec::ListFlat]).unwrap();
//! let mut store = Store::new(1024);
//! let u32_ty = Ty::U32;
//!
//! let mut buf = vec![0u8; 16];
//! for i in 0..3u8 {
//!     let vb = ValidatedCabiBytes::checked(&buf, &u32_ty).unwrap();
//!     prepared.bind().arg_list_flat(vb).invoke(&mut store).unwrap();
//!     buf[0] = i; // freely mutate the buffer *between* calls — the binding is gone
//! }
//! ```
//!
//! # A borrowed result view cannot escape its scope
//!
//! `invoke_scoped` yields zero-copy views valid only for the duration of the closure:
//!
//! ```compile_fail
//! use call_builder_sim::call_builder::{PreparedCall, ValSpec};
//! use call_builder_sim::fake_wasmtime::{CoreVal, Func, Store, Ty};
//!
//! let func = Func::new(vec![], vec![Ty::List(Box::new(Ty::U32))], |caller, _core| {
//!     let s = caller.store();
//!     let ptr = s.realloc(4, 4);
//!     s.write_u32(ptr, 7);
//!     vec![CoreVal::I32(ptr as i32), CoreVal::I32(1)]
//! });
//! let prepared = PreparedCall::new(&func, &[]).unwrap();
//! let mut store = Store::new(1024);
//!
//! // ERROR: the view borrows the scope; it cannot be returned out of the closure.
//! let escaped = prepared.bind().invoke_scoped(&mut store, |r| r.view(0)).unwrap();
//! let _ = escaped;
//! ```
//!
//! # A bound source buffer cannot be mutated while the binding is alive
//!
//! A `Flat` source borrows its bytes (through [`ValidatedCabiBytes`](fake_wasmtime::ValidatedCabiBytes))
//! for the whole binding, so touching them before `invoke` does not compile:
//!
//! ```compile_fail
//! use call_builder_sim::call_builder::{PreparedCall, ValSpec};
//! use call_builder_sim::fake_wasmtime::{Func, Store, Ty, ValidatedCabiBytes};
//!
//! let func = Func::new(vec![Ty::List(Box::new(Ty::U32))], vec![], |_caller, _core| vec![]);
//! let prepared = PreparedCall::new(&func, &[ValSpec::ListFlat]).unwrap();
//! let mut store = Store::new(1024);
//! let u32_ty = Ty::U32;
//!
//! let mut buf = vec![0u8; 16];
//! let vb = ValidatedCabiBytes::checked(&buf, &u32_ty).unwrap();
//! let bound = prepared.bind().arg_list_flat(vb);
//! buf[0] = 1;                         // ERROR: `buf` is still borrowed by `bound`
//! bound.invoke(&mut store).unwrap();
//! ```
//!
//! # A `Lazy` source's producer is borrowed for the whole call
//!
//! A lazy source borrows its [`LowerOnDemand`](call_builder::LowerOnDemand) `&mut` for the
//! whole binding — just like a flat arg — so touching it while the binding is alive does not
//! compile:
//!
//! ```compile_fail
//! use call_builder_sim::call_builder::{LowerOnDemand, PreparedCall, ValSpec};
//! use call_builder_sim::fake_wasmtime::{Func, Store, Ty};
//!
//! struct Col;
//! impl LowerOnDemand for Col {
//!     fn len(&self) -> usize { 0 }
//!     fn produce_range(&mut self, _: usize, _: usize, _: &mut Vec<u8>) {}
//! }
//!
//! let func = Func::new(vec![Ty::List(Box::new(Ty::U32))], vec![], |_caller, _core| vec![]);
//! let prepared = PreparedCall::new(&func, &[ValSpec::Lazy]).unwrap();
//! let mut store = Store::new(1024);
//!
//! let mut col = Col;
//! let bound = prepared.bind().arg_lazy(&mut col);
//! col.produce_range(0, 0, &mut Vec::new());   // ERROR: `col` already mutably borrowed by `bound`
//! bound.invoke(&mut store).unwrap();
//! ```

pub mod call_builder;
pub mod fake_wasmtime;
