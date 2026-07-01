//! The call-builder API itself: the interesting part.
//!
//! The design splits into two objects, and that split is the whole answer to "how do you
//! reuse a call while still mutating its argument buffers between invocations?":
//!
//! * [`Prepared`] — validated once, holds **no** buffer borrows (and no lifetime
//!   parameter). You keep it across the whole loop.
//! * [`CallBuilder`] — created fresh per call from `&Prepared`; it borrows the argument
//!   buffers for `'a`, and [`CallBuilder::invoke`] **consumes** it, ending those borrows
//!   before the next statement. So between calls the buffers are unborrowed and freely
//!   mutable.

use std::rc::Rc;

use crate::fake_wasmtime::{lower_flat, lower_val, CoreVal, Func, Store, Ty, Val};

#[derive(Debug)]
pub enum Error {
    /// Wrong number of arguments for the function's parameter list.
    Arity { expected: usize, got: usize },
    /// An argument's representation didn't match the spec chosen at `prepare` time.
    SpecMismatch { index: usize },
    /// A flat spec was requested for a non-flat (out-of-line) type.
    NotFlat { index: usize },
}

/// The representation *choice* for one argument — declared at `prepare` time, carrying no
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

/// The lowering tier chosen for an argument when the plan is prepared — the "checked" part
/// of "checked-with-a-fast-path".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Pre-encoded bytes copied in one shot.
    Memcpy,
    /// The type is walked element-by-element.
    ValWalk,
}

struct ArgPlan {
    ty: Ty,
    spec: ArgSpec,
    tier: Tier,
}

/// A validated, reusable call plan. **Borrows no argument buffers.**
pub struct Prepared {
    func: Rc<Func>,
    plan: Vec<ArgPlan>,
}

impl Prepared {
    /// Validate a per-argument representation choice against a function's parameters and
    /// pick each argument's lowering tier — once.
    pub fn new(func: &Rc<Func>, specs: &[ArgSpec]) -> Result<Prepared, Error> {
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
                    if !ty.is_flat() {
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
        Ok(Prepared {
            func: Rc::clone(func),
            plan,
        })
    }

    /// The lowering tier chosen per argument — handy for showing the "checked fast path"
    /// decision.
    pub fn tiers(&self) -> Vec<Tier> {
        self.plan.iter().map(|p| p.tier).collect()
    }

    /// Begin an invocation. The returned builder borrows nothing yet.
    pub fn call(&self) -> CallBuilder<'_> {
        CallBuilder {
            prepared: self,
            slots: Vec::with_capacity(self.plan.len()),
        }
    }
}

/// Convenience mirror of [`Prepared::new`] so call sites can read `Func::prepare(&func, …)`.
impl Func {
    pub fn prepare(func: &Rc<Func>, specs: &[ArgSpec]) -> Result<Prepared, Error> {
        Prepared::new(func, specs)
    }
}

/// One argument's data, supplied per call. The flat variants borrow the caller's buffer
/// for the lifetime of the builder only.
pub enum ArgSource<'a> {
    Val(Val),
    FlatIn(&'a [u8]),
    FlatInOut(&'a mut [u8]),
}

/// The ephemeral, per-call builder. Its lifetime `'a` ties it to the argument buffers it
/// borrows; `invoke` consumes it, releasing them.
pub struct CallBuilder<'a> {
    prepared: &'a Prepared,
    slots: Vec<ArgSource<'a>>,
}

impl<'a> CallBuilder<'a> {
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

    /// Lower each argument (memcpy for flat, per-element for `Val`), call the guest body,
    /// then copy every `FlatInOut` region back into its buffer. Consumes `self`.
    pub fn invoke(self, store: &mut Store) -> Result<(), Error> {
        let plan = &self.prepared.plan;
        if self.slots.len() != plan.len() {
            return Err(Error::Arity {
                expected: plan.len(),
                got: self.slots.len(),
            });
        }

        store.reset();
        let mut core: Vec<CoreVal> = Vec::new();
        // Regions to copy back after the call, holding the caller's `&mut` buffer.
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

        (self.prepared.func.body)(store, &core);

        for (ptr, buf) in inout {
            let len = buf.len();
            buf.copy_from_slice(&store.mem[ptr..ptr + len]);
        }
        Ok(())
    }
}
