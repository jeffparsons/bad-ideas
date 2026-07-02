//! The **host -> guest** ergonomic surface: quadrants 1 (params, bulk lower) and 2
//! (results, bulk lift) of the transfer matrix.
//!
//! The design splits into two objects, and that split is the whole answer to "how do you
//! reuse a call while still mutating its argument buffers between invocations?" — and it
//! is the split issue #13 wants named clearly:
//!
//! * [`PreparedCall`] — the *shape*: validated once (arity, per-arg tier), holds **no**
//!   buffer borrows and has no lifetime parameter. Reused across a whole loop. (SQL:
//!   the prepared statement. Vulkan: the pipeline layout.)
//! * [`BoundCall`] — the *binding*: created fresh per call via [`PreparedCall::bind`];
//!   borrows the concrete argument buffers for `'a`, and the `invoke*` methods
//!   **consume** it, ending those borrows before the next statement. (SQL: the statement
//!   with parameters bound, about to `execute`.)
//!
//! Between calls the buffers are unborrowed and freely mutable. The safety is enforced by
//! the type system — see the `compile_fail` doctests in the crate root.
//!
//! ## Receiving results
//!
//! Guest-owned result memory is only valid between "the guest returned" and "canonical
//! post-return cleanup". So results are offered two ways, and the caller picks per call:
//!
//! * [`BoundCall::invoke_scoped`] — a Wasmtime-controlled scope yields borrowed,
//!   zero-copy [`Results`] views that cannot escape (the ergonomic + fast path);
//! * [`BoundCall::invoke_collect`] — eagerly copies each result list out into host-owned
//!   `Vec<u8>` for callers who want to keep the bytes.

use std::rc::Rc;

use crate::fake_wasmtime::{
    core_slot_offsets, lift_flat, lift_flat_view, lower_flat, lower_val, Caller, CoreVal, Func,
    Store, Ty, Val,
};

#[derive(Debug)]
pub enum Error {
    /// Wrong number of arguments for the function's parameter list.
    Arity { expected: usize, got: usize },
    /// An argument's representation didn't match the spec chosen at `prepare` time.
    SpecMismatch { index: usize },
    /// A flat spec was requested for a non-flat (out-of-line / ownership-bearing) type.
    NotFlat { index: usize },
}

/// The representation *choice* for one argument — declared at prepare time, carrying no
/// data. This is where "mix `Val` for some args, flat bytes for others" lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgSpec {
    /// General, per-element `Val` lowering. Convenient; slow.
    Val,
    /// Read-only flat bytes: one memcpy in, nothing back.
    FlatIn,
    /// Read-write flat bytes: memcpy in, guest mutates, memcpy back into the same buffer.
    FlatInOut,
}

/// The lowering strategy chosen for an argument when the plan is prepared — the "checked"
/// part of "checked-with-a-fast-path".
///
/// A future `LazyMemcpy` variant belongs here too: with lazy value lowering
/// ([WebAssembly/component-model#383]) the bytes would be pulled *during* the guest call
/// rather than copied up front. Crucially that changes only *when* [`Tier::Memcpy`] does
/// its work, not this plan's shape or the borrow structure — the source buffer is already
/// borrowed for the whole `invoke`, which is exactly what lazy pulling requires.
///
/// [WebAssembly/component-model#383]: https://github.com/WebAssembly/component-model/issues/383
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Pre-encoded bytes copied in one shot (eagerly today; lazily in a lazy-lowering world).
    Memcpy,
    /// The type is walked element-by-element.
    ValWalk,
}

struct ArgPlan {
    ty: Ty,
    spec: ArgSpec,
    tier: Tier,
}

/// The core results of a call, plus the in-out regions whose bytes must be copied back
/// into the caller's buffers once the guest returns.
type LoweredCall<'a> = (Vec<CoreVal>, Vec<(usize, &'a mut [u8])>);

/// A validated, reusable **call shape**. Borrows no argument buffers; holds an `Rc<Func>`
/// and the per-argument plan. Prepare once, [`bind`](Self::bind) many times.
pub struct PreparedCall {
    func: Rc<Func>,
    plan: Vec<ArgPlan>,
}

impl PreparedCall {
    /// Validate a per-argument representation choice against a function's parameters and
    /// pick each argument's lowering tier — once.
    pub fn new(func: &Rc<Func>, specs: &[ArgSpec]) -> Result<PreparedCall, Error> {
        if specs.len() != func.params.len() {
            return Err(Error::Arity {
                expected: func.params.len(),
                got: specs.len(),
            });
        }
        let mut plan = Vec::with_capacity(specs.len());
        for (index, (ty, &spec)) in func.params.iter().zip(specs).enumerate() {
            let tier = match spec {
                ArgSpec::Val => Tier::ValWalk,
                ArgSpec::FlatIn | ArgSpec::FlatInOut => {
                    if !ty.is_bulk_transferable() {
                        return Err(Error::NotFlat { index });
                    }
                    Tier::Memcpy
                }
            };
            plan.push(ArgPlan {
                ty: ty.clone(),
                spec,
                tier,
            });
        }
        Ok(PreparedCall {
            func: Rc::clone(func),
            plan,
        })
    }

    /// The lowering tier chosen per argument — handy for showing the "checked fast path"
    /// decision that was made once.
    pub fn tiers(&self) -> Vec<Tier> {
        self.plan.iter().map(|p| p.tier).collect()
    }

    /// Begin **binding** concrete arguments for one invocation. The returned builder
    /// borrows nothing yet.
    pub fn bind(&self) -> BoundCall<'_> {
        BoundCall {
            prepared: self,
            slots: Vec::with_capacity(self.plan.len()),
        }
    }
}

/// Convenience mirror of [`PreparedCall::new`] so call sites can read
/// `Func::prepare(&func, …)`.
impl Func {
    pub fn prepare(func: &Rc<Func>, specs: &[ArgSpec]) -> Result<PreparedCall, Error> {
        PreparedCall::new(func, specs)
    }
}

/// One argument's data, supplied per call. The flat variants borrow the caller's buffer
/// for the lifetime of the binding only.
pub enum ArgSource<'a> {
    Val(Val),
    FlatIn(&'a [u8]),
    FlatInOut(&'a mut [u8]),
}

/// The ephemeral, per-call **binding**. Its lifetime `'a` ties it to the argument buffers
/// it borrows; each `invoke*` consumes it, releasing them.
pub struct BoundCall<'a> {
    prepared: &'a PreparedCall,
    slots: Vec<ArgSource<'a>>,
}

impl<'a> BoundCall<'a> {
    pub fn arg(mut self, src: ArgSource<'a>) -> Self {
        self.slots.push(src);
        self
    }

    pub fn arg_val(self, v: Val) -> Self {
        self.arg(ArgSource::Val(v))
    }

    pub fn arg_flat_in(self, bytes: &'a [u8]) -> Self {
        self.arg(ArgSource::FlatIn(bytes))
    }

    pub fn arg_flat_inout(self, bytes: &'a mut [u8]) -> Self {
        self.arg(ArgSource::FlatInOut(bytes))
    }

    /// Lower args, run the guest, copy `FlatInOut` regions back. For calls whose results
    /// you don't need to read (or that return nothing). Consumes `self`.
    pub fn invoke(self, store: &mut Store) -> Result<(), Error> {
        let (_core_results, inout) = self.lower_and_run(store)?;
        copy_back(store, inout);
        Ok(())
    }

    /// **Quadrant 2 (host -> guest results, bulk lift), scoped.** Lower args, run the
    /// guest, then hand a borrowed [`Results`] view into `f`. The views are valid only
    /// for the duration of `f` — they cannot escape it, and cleanup / inout copy-back
    /// happens after `f` returns. This is the zero-copy fast path for reading results.
    pub fn invoke_scoped<R>(
        self,
        store: &mut Store,
        f: impl FnOnce(&Results) -> R,
    ) -> Result<R, Error> {
        // Result type shapes are needed to interpret the core results; grab them before
        // `self` is torn apart by `lower_and_run`.
        let result_tys = self.prepared.func.results.clone();
        let (core_results, inout) = self.lower_and_run(store)?;
        let out = {
            let results = Results {
                store,
                result_tys: &result_tys,
                slots: core_slot_offsets(&result_tys),
                core: core_results,
            };
            f(&results)
        }; // `results` (and its borrowed views) dropped here, before copy-back.
        copy_back(store, inout);
        Ok(out)
    }

    /// **Quadrant 2, eager.** Lower args, run the guest, then copy every `list<flat T>`
    /// result out into a host-owned `Vec<u8>`. For callers who want to keep the bytes
    /// past the call.
    pub fn invoke_collect(self, store: &mut Store) -> Result<Vec<Vec<u8>>, Error> {
        let result_tys = self.prepared.func.results.clone();
        let (core_results, inout) = self.lower_and_run(store)?;
        let slots = core_slot_offsets(&result_tys);
        let mut out = Vec::with_capacity(result_tys.len());
        for (i, ty) in result_tys.iter().enumerate() {
            let slot = slots[i];
            let ptr = core_results[slot].as_i32() as usize;
            let count = core_results[slot + 1].as_i32() as usize;
            out.push(lift_flat(store, ty, ptr, count));
        }
        copy_back(store, inout);
        Ok(out)
    }

    /// Shared machinery: validate, lower every argument into `store.mem`, run the guest
    /// body, and return `(core results, in-out regions to copy back)`.
    fn lower_and_run(self, store: &mut Store) -> Result<LoweredCall<'a>, Error> {
        let prepared = self.prepared;
        let plan = &prepared.plan;
        if self.slots.len() != plan.len() {
            return Err(Error::Arity {
                expected: plan.len(),
                got: self.slots.len(),
            });
        }

        store.reset();
        let mut core: Vec<CoreVal> = Vec::new();
        let mut inout: Vec<(usize, &'a mut [u8])> = Vec::new();

        for (index, (argplan, slot)) in plan.iter().zip(self.slots).enumerate() {
            match (argplan.spec, slot) {
                (ArgSpec::Val, ArgSource::Val(v)) => {
                    lower_val(store, &argplan.ty, &v, &mut core);
                }
                (ArgSpec::FlatIn, ArgSource::FlatIn(bytes)) => {
                    let (ptr, len) = lower_flat(store, &argplan.ty, bytes);
                    core.push(CoreVal::I32(ptr as i32));
                    core.push(CoreVal::I32(len as i32));
                }
                (ArgSpec::FlatInOut, ArgSource::FlatInOut(bytes)) => {
                    // Reborrow as shared for the copy-in, then keep the `&mut` for copy-back.
                    let (ptr, len) = lower_flat(store, &argplan.ty, &*bytes);
                    core.push(CoreVal::I32(ptr as i32));
                    core.push(CoreVal::I32(len as i32));
                    inout.push((ptr, bytes));
                }
                _ => return Err(Error::SpecMismatch { index }),
            }
        }

        let core_results = {
            let mut caller = Caller::new(store, &prepared.func.imports);
            (prepared.func.body)(&mut caller, &core)
        };
        Ok((core_results, inout))
    }
}

/// Copy every in-out region out of guest memory back into the caller's `&mut` buffer.
fn copy_back(store: &Store, inout: Vec<(usize, &mut [u8])>) {
    for (ptr, buf) in inout {
        let len = buf.len();
        buf.copy_from_slice(&store.mem[ptr..ptr + len]);
    }
}

/// Guest-owned result memory, exposed for the extent of an [`BoundCall::invoke_scoped`]
/// closure. Views borrow `&Store`, so they cannot outlive the scope.
pub struct Results<'a> {
    store: &'a Store,
    result_tys: &'a [Ty],
    slots: Vec<usize>,
    core: Vec<CoreVal>,
}

impl Results<'_> {
    /// Number of results.
    pub fn len(&self) -> usize {
        self.result_tys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.result_tys.is_empty()
    }

    /// **Zero-copy** borrowed view of the flat bytes of `list<flat T>` result `index`.
    /// The borrow is tied to `&self`, i.e. to the enclosing `invoke_scoped` closure.
    pub fn flat_list_view(&self, index: usize) -> &[u8] {
        let ty = &self.result_tys[index];
        let slot = self.slots[index];
        let ptr = self.core[slot].as_i32() as usize;
        let count = self.core[slot + 1].as_i32() as usize;
        lift_flat_view(self.store, ty, ptr, count)
    }

    /// Copy result `index` out into host-owned bytes (available inside the scope too).
    pub fn copy_out(&self, index: usize) -> Vec<u8> {
        self.flat_list_view(index).to_vec()
    }
}
