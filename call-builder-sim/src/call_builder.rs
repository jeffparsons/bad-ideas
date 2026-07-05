//! The **host → guest** provide surface, rebuilt around a recursive **`ValSource`** tree
//! (issue #16). Every node independently picks how it supplies its bytes; the simple
//! dynamic case is the degenerate all-[`Val`] tree, so nothing regresses.
//!
//! The load-bearing idea is that every source is a **random-access range producer**
//! ([`produce_list_range`](ValSource::produce_list_range)). That single shape lets the
//! runtime drive the *same* `ValSource` tree two ways:
//!
//! * **eagerly** (today): [`drive_eager`] / [`BoundCall::invoke`] materialise the whole
//!   value up front, then call the guest — the runtime calls every producer once, fully.
//! * **lazily** (lazy value lowering, [WebAssembly/component-model#383]): [`drive_lazy`]
//!   models the guest pulling ranges on demand (`value.lower`), so unpulled data is never
//!   produced.
//!
//! The embedder writes the identical tree either way; eager is the strict special case.
//!
//! Two objects still split *shape* from *binding*:
//! * [`PreparedCall`] — validated once against the function's parameter types (Q1: the
//!   reusable `ValSpec` shape), holds no buffer borrows.
//! * [`BoundCall`] — per call; borrows the argument sources for `'a`, consumed by `invoke*`.
//!
//! [WebAssembly/component-model#383]: https://github.com/WebAssembly/component-model/issues/383

use std::rc::Rc;

use crate::fake_wasmtime::{
    align_up, flatten_image, lower_flat, Caller, CoreVal, Func, Lifted, OwnedResults, Producer,
    Store, Ty, Val, ValidatedCabiBytes, ValidatedCabiBytesBuf, ValidityError,
};

#[derive(Debug)]
pub enum Error {
    Arity { expected: usize, got: usize },
    /// A source's representation didn't match the `ValSpec` shape fixed at prepare time.
    SpecMismatch { index: usize },
    /// A flat/lazy/arena spec was requested for a non-inline (out-of-line / ownership) type.
    NotInline { index: usize },
    /// A `Record` spec's field count didn't match the record type.
    RecordArity { index: usize },
    /// Bytes offered somewhere failed canonical validation.
    Validity,
}

impl From<ValidityError> for Error {
    fn from(_: ValidityError) -> Self {
        Error::Validity
    }
}

// ---------------------------------------------------------------------------------------
// LowerOnDemand — the host callback behind a `Lazy` source.
// ---------------------------------------------------------------------------------------

/// A **host-computed** value source: the general form of a lazy node. The runtime calls
/// [`produce_range`](LowerOnDemand::produce_range) with an element range and the impl writes
/// that range's canonical images — random access, so it works whether the runtime drains it
/// eagerly (once, `0..len`) or the guest pulls windows lazily.
///
/// Per the issue-#16 confirmation: **the impl owns the caching policy.** It may compute
/// fresh each call, gather-and-memoize on first touch, or pre-materialise — the runtime
/// mandates nothing and only ever calls `produce_range`.
pub trait LowerOnDemand {
    /// Number of elements this source yields.
    fn len(&self) -> usize;
    /// Whether this source yields no elements.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Append the canonical images of elements `[start, start + count)` to `out`.
    fn produce_range(&mut self, start: usize, count: usize, out: &mut Vec<u8>);
}

// ---------------------------------------------------------------------------------------
// Arena — a pre-validated, host-native flattened store (issue #16, Q2 = host-native).
// ---------------------------------------------------------------------------------------

/// A pre-validated container of canonical inline element images. Validation (the
/// [`are_all_bit_patterns_valid`](Ty::are_all_bit_patterns_valid) sweep) is paid **once**,
/// at [`absorb`](Arena::absorb) time; slices replayed into later calls re-check nothing.
///
/// Q2: stored **host-native flattened** — here a single contiguous run, which needs no
/// overlay offset table (element `k` is at `k * stride`, pure arithmetic). Overlay tables
/// only arise at *depth* (multi-list-deep), which this single level deliberately doesn't model.
pub struct Arena {
    store: ValidatedCabiBytesBuf,
    elem: Ty,
}

/// A borrowed, still-validated slice of an [`Arena`] — usable as a [`ValSource`].
pub struct ArenaSlice<'a> {
    bytes: &'a [u8],
    stride: usize,
}

impl Arena {
    /// Create an arena for element type `elem`, seeded from an already-validated buffer.
    pub fn new(seed: ValidatedCabiBytesBuf, elem: Ty) -> Self {
        Arena { store: seed, elem }
    }

    /// Absorb a run of raw bytes as `elem` images, validating **once** here (the sweep).
    /// Returns an arena holding the validated bytes. (Kept simple: one absorb per arena.)
    pub fn absorb(bytes: Vec<u8>, elem: Ty) -> Result<Arena, ValidityError> {
        let store = ValidatedCabiBytesBuf::checked(bytes, elem.clone())?;
        Ok(Arena { store, elem })
    }

    fn all_bytes(&self) -> &[u8] {
        self.store.as_ref().bytes()
    }

    pub fn elem_count(&self) -> usize {
        self.all_bytes().len() / self.elem.size()
    }

    /// A sub-range of elements `[start, start + count)` as a replayable, still-validated slice.
    pub fn slice(&self, start: usize, count: usize) -> ArenaSlice<'_> {
        let stride = self.elem.size();
        ArenaSlice {
            bytes: &self.all_bytes()[start * stride..(start + count) * stride],
            stride,
        }
    }
}

impl<'a> ArenaSlice<'a> {
    fn len(&self) -> usize {
        self.bytes.len() / self.stride
    }

    fn produce_range(&self, start: usize, count: usize, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.bytes[start * self.stride..(start + count) * self.stride]);
    }
}

// ---------------------------------------------------------------------------------------
// ValSource / ListSource — the recursive provide tree.
// ---------------------------------------------------------------------------------------

/// A node in the value-source tree. Borrows live here; [`Val`] stays owned *inside* the
/// dynamic leaf, so `Val` never grows a lifetime.
pub enum ValSource<'a> {
    /// Dynamic, owned — the always-available fallback (the whole simple API).
    Val(Val),
    /// A single inline value/record as proof-carrying canonical bytes (one memcpy).
    Flat(ValidatedCabiBytes<'a>),
    /// An inline record, each field independently sourced.
    Record(Vec<ValSource<'a>>),
    /// A `list<T>`: flat bytes or per-element sources.
    List(ListSource<'a>),
    /// A `stream<T>` — a distinct CM type, kept as its own node ([#12]).
    Stream(&'a mut dyn Producer),
    /// A host-computed value (e.g. an archetype column), pulled on demand.
    Lazy(&'a mut dyn LowerOnDemand),
    /// A slice of a pre-validated [`Arena`].
    Arena(ArenaSlice<'a>),
}

/// How a `list<T>`'s elements are supplied.
pub enum ListSource<'a> {
    /// `list<inline T>` as one contiguous validated image — bulk memcpy. Legal only when the
    /// element type is inline (checked at prepare) — mirroring #383's single-`validx` path.
    Flat(ValidatedCabiBytes<'a>),
    /// Per-element sources (heterogeneous / dynamic) — mirroring #383's per-element `validx`es
    /// for out-of-line elements, but usable for inline elements too (host-side granularity is
    /// decoupled from the guest's).
    Elems(Vec<ValSource<'a>>),
}

impl<'a> ValSource<'a> {
    /// Number of elements this source yields when it backs a `list<elem_ty>`.
    fn list_len(&self, elem_ty: &Ty) -> usize {
        match self {
            ValSource::Val(Val::List(items)) => items.len(),
            ValSource::List(ListSource::Flat(vb)) => vb.len(),
            ValSource::List(ListSource::Elems(items)) => items.len(),
            ValSource::Lazy(col) => col.len(),
            ValSource::Arena(slice) => slice.len(),
            other => panic!("{} is not a list source for {elem_ty:?}", other.kind()),
        }
    }

    /// **The range producer.** Append canonical images of list elements `[start, start+count)`
    /// to `out`. This is the one operation the eager and lazy drivers share.
    fn produce_list_range(&mut self, elem_ty: &Ty, start: usize, count: usize, out: &mut Vec<u8>) {
        let stride = elem_ty.size();
        match self {
            ValSource::Val(Val::List(items)) => {
                for item in &items[start..start + count] {
                    encode_inline_val(elem_ty, item, out);
                }
            }
            ValSource::List(ListSource::Flat(vb)) => {
                out.extend_from_slice(&vb.bytes()[start * stride..(start + count) * stride]);
            }
            ValSource::List(ListSource::Elems(items)) => {
                for item in &mut items[start..start + count] {
                    item.produce_value_image(elem_ty, out);
                }
            }
            ValSource::Lazy(col) => col.produce_range(start, count, out),
            ValSource::Arena(slice) => slice.produce_range(start, count, out),
            other => panic!("{} is not a list source", other.kind()),
        }
    }

    /// Produce the canonical image of a single inline value (scalar / enum / record).
    fn produce_value_image(&mut self, ty: &Ty, out: &mut Vec<u8>) {
        match self {
            ValSource::Val(v) => encode_inline_val(ty, v, out),
            ValSource::Flat(vb) => out.extend_from_slice(vb.bytes()),
            ValSource::Record(fields) => {
                let field_tys = match ty {
                    Ty::Record(f) => f,
                    _ => panic!("Record source for non-record type {ty:?}"),
                };
                let base = out.len();
                for (fty, fsrc) in field_tys.iter().zip(fields.iter_mut()) {
                    // Pad to the field's alignment relative to the record's start.
                    let want = base + align_up(out.len() - base, fty.align());
                    out.resize(want, 0);
                    fsrc.produce_value_image(fty, out);
                }
                let full = base + align_up(out.len() - base, ty.align());
                out.resize(full, 0);
            }
            other => panic!("{} cannot produce a single inline value", other.kind()),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            ValSource::Val(_) => "Val",
            ValSource::Flat(_) => "Flat",
            ValSource::Record(_) => "Record",
            ValSource::List(_) => "List",
            ValSource::Stream(_) => "Stream",
            ValSource::Lazy(_) => "Lazy",
            ValSource::Arena(_) => "Arena",
        }
    }
}

/// Encode a dynamic inline [`Val`] to its canonical byte image (scalar / enum / record).
fn encode_inline_val(ty: &Ty, val: &Val, out: &mut Vec<u8>) {
    match (ty, val) {
        (Ty::F32, Val::F32(x)) => out.extend_from_slice(&x.to_le_bytes()),
        (Ty::U32, Val::U32(x)) => out.extend_from_slice(&x.to_le_bytes()),
        (Ty::Enum(cases), Val::Enum(d)) => {
            assert!(d < cases, "enum discriminant {d} out of range (< {cases})");
            out.extend_from_slice(&d.to_le_bytes());
        }
        (Ty::Record(fields), Val::Record(vals)) => {
            let base = out.len();
            for (fty, fval) in fields.iter().zip(vals) {
                let want = base + align_up(out.len() - base, fty.align());
                out.resize(want, 0);
                encode_inline_val(fty, fval, out);
            }
            let full = base + align_up(out.len() - base, ty.align());
            out.resize(full, 0);
        }
        _ => panic!("val/type mismatch encoding inline image: {ty:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// ValSpec — the reusable, once-validated shape (Q1).
// ---------------------------------------------------------------------------------------

/// The representation *choice* for one argument, carrying no data. Validated against the
/// parameter type **once** at [`PreparedCall::new`] and reused across every `bind`/`invoke`.
#[derive(Clone, Debug)]
pub enum ValSpec {
    Val,
    /// A single inline value as flat bytes.
    Flat,
    /// An inline record, per field.
    Record(Vec<ValSpec>),
    /// `list<inline T>` as flat bytes.
    ListFlat,
    /// `list<T>` per element (uniform element spec).
    ListElems(Box<ValSpec>),
    /// Host-computed (`Lazy`).
    Lazy,
    /// From a pre-validated arena.
    Arena,
    /// A `stream<T>` node.
    Stream,
}

impl ValSpec {
    /// Is this representation legal for `ty`? The once-paid structural check.
    fn validate(&self, ty: &Ty, index: usize) -> Result<(), Error> {
        match self {
            ValSpec::Val | ValSpec::Stream => Ok(()),
            ValSpec::Flat => {
                if ty.is_inline() {
                    Ok(())
                } else {
                    Err(Error::NotInline { index })
                }
            }
            ValSpec::Record(specs) => match ty {
                Ty::Record(fields) if fields.len() == specs.len() => {
                    for (s, f) in specs.iter().zip(fields) {
                        s.validate(f, index)?;
                    }
                    Ok(())
                }
                Ty::Record(_) => Err(Error::RecordArity { index }),
                _ => Err(Error::SpecMismatch { index }),
            },
            ValSpec::ListFlat | ValSpec::Lazy | ValSpec::Arena => match ty {
                Ty::List(elem) if elem.is_inline() => Ok(()),
                Ty::List(_) => Err(Error::NotInline { index }),
                _ => Err(Error::SpecMismatch { index }),
            },
            ValSpec::ListElems(es) => match ty {
                Ty::List(elem) => es.validate(elem, index),
                _ => Err(Error::SpecMismatch { index }),
            },
        }
    }

    /// Does a bound [`ValSource`] match this shape? (Light structural check at bind time.)
    fn matches(&self, src: &ValSource) -> bool {
        matches!(
            (self, src),
            (ValSpec::Val, ValSource::Val(_))
                | (ValSpec::Flat, ValSource::Flat(_))
                | (ValSpec::Record(_), ValSource::Record(_))
                | (ValSpec::ListFlat, ValSource::List(ListSource::Flat(_)))
                | (ValSpec::ListElems(_), ValSource::List(ListSource::Elems(_)))
                | (ValSpec::Lazy, ValSource::Lazy(_))
                | (ValSpec::Arena, ValSource::Arena(_))
                | (ValSpec::Stream, ValSource::Stream(_))
                // Val is always an allowed fallback for a list/scalar position.
                | (ValSpec::ListFlat, ValSource::Val(_))
                | (ValSpec::Lazy, ValSource::Val(_))
                | (ValSpec::Arena, ValSource::Val(_))
        )
    }
}

// ---------------------------------------------------------------------------------------
// PreparedCall / BoundCall.
// ---------------------------------------------------------------------------------------

/// A validated, reusable **call shape**. Borrows no argument sources.
pub struct PreparedCall {
    func: Rc<Func>,
    specs: Vec<ValSpec>,
}

impl PreparedCall {
    /// Validate a per-argument [`ValSpec`] against the function's parameter types — once.
    pub fn new(func: &Rc<Func>, specs: &[ValSpec]) -> Result<PreparedCall, Error> {
        if specs.len() != func.params.len() {
            return Err(Error::Arity {
                expected: func.params.len(),
                got: specs.len(),
            });
        }
        for (index, (ty, spec)) in func.params.iter().zip(specs).enumerate() {
            spec.validate(ty, index)?;
        }
        Ok(PreparedCall {
            func: Rc::clone(func),
            specs: specs.to_vec(),
        })
    }

    pub fn bind(&self) -> BoundCall<'_> {
        BoundCall {
            prepared: self,
            slots: Vec::with_capacity(self.specs.len()),
        }
    }
}

impl Func {
    pub fn prepare(func: &Rc<Func>, specs: &[ValSpec]) -> Result<PreparedCall, Error> {
        PreparedCall::new(func, specs)
    }
}

/// The ephemeral, per-call binding. `'a` ties it to the sources it borrows; `invoke*`
/// consumes it, releasing them.
pub struct BoundCall<'a> {
    prepared: &'a PreparedCall,
    slots: Vec<ValSource<'a>>,
}

impl<'a> BoundCall<'a> {
    pub fn arg(mut self, src: ValSource<'a>) -> Self {
        self.slots.push(src);
        self
    }

    // Convenience wrappers (the "enum primitive + optional sugar" rule).
    pub fn arg_val(self, v: Val) -> Self {
        self.arg(ValSource::Val(v))
    }
    pub fn arg_flat(self, vb: ValidatedCabiBytes<'a>) -> Self {
        self.arg(ValSource::Flat(vb))
    }
    pub fn arg_list_flat(self, vb: ValidatedCabiBytes<'a>) -> Self {
        self.arg(ValSource::List(ListSource::Flat(vb)))
    }
    pub fn arg_lazy(self, col: &'a mut dyn LowerOnDemand) -> Self {
        self.arg(ValSource::Lazy(col))
    }
    pub fn arg_arena(self, slice: ArenaSlice<'a>) -> Self {
        self.arg(ValSource::Arena(slice))
    }

    /// **Run and own the results** (eager lowering). Materialises the whole `ValSource` tree
    /// into guest memory, runs the guest, and returns owned result bytes.
    pub fn invoke(self, store: &mut Store) -> Result<OwnedResults, Error> {
        let result_tys = self.prepared.func.results.clone();
        let core_results = self.lower_and_run(store)?;
        let mem = if result_tys.is_empty() {
            Vec::new()
        } else {
            store.mem.clone()
        };
        Ok(OwnedResults::new(mem, result_tys, core_results))
    }

    /// **Run and read the results zero-copy** (eager lowering).
    pub fn invoke_scoped<R>(
        self,
        store: &mut Store,
        f: impl FnOnce(&Lifted) -> R,
    ) -> Result<R, Error> {
        let result_tys = self.prepared.func.results.clone();
        let core_results = self.lower_and_run(store)?;
        let results = Lifted::new(&store.mem, &result_tys, &core_results);
        Ok(f(&results))
    }

    /// Eager driver: validate shape, materialise every param into `store.mem`, run the guest.
    fn lower_and_run(self, store: &mut Store) -> Result<Vec<CoreVal>, Error> {
        let prepared = self.prepared;
        if self.slots.len() != prepared.specs.len() {
            return Err(Error::Arity {
                expected: prepared.specs.len(),
                got: self.slots.len(),
            });
        }
        store.reset();
        let mut core: Vec<CoreVal> = Vec::new();
        let mut streams: Vec<&'a mut dyn Producer> = Vec::new();
        let param_tys = prepared.func.params.clone();

        for (index, ((spec, ty), mut src)) in prepared
            .specs
            .iter()
            .zip(&param_tys)
            .zip(self.slots)
            .enumerate()
        {
            if !spec.matches(&src) {
                return Err(Error::SpecMismatch { index });
            }
            if let ValSource::Stream(p) = src {
                // No up-front materialisation: hand the guest a handle, keep the producer.
                core.push(CoreVal::I32(streams.len() as i32));
                streams.push(p);
                continue;
            }
            match ty {
                Ty::List(elem) => {
                    let n = src.list_len(elem);
                    let mut buf = Vec::new();
                    src.produce_list_range(elem, 0, n, &mut buf); // materialise ALL (eager)
                    let (ptr, len) = lower_flat(store, ty, &buf);
                    core.push(CoreVal::I32(ptr as i32));
                    core.push(CoreVal::I32(len as i32));
                }
                _ => {
                    let mut buf = Vec::new();
                    src.produce_value_image(ty, &mut buf);
                    flatten_image(ty, &buf, 0, &mut core);
                }
            }
        }

        let core_results = {
            let mut caller = Caller::new(store, &prepared.func.imports, streams);
            (prepared.func.body)(&mut caller, &core)
        };
        Ok(core_results)
    }
}

// ---------------------------------------------------------------------------------------
// drive_eager / drive_lazy — the "same API, both worlds" proof harness.
//
// These drive a single list-typed `ValSource` the two ways the runtime could, so a test can
// assert the produced bytes are identical and that a lazy windowed pull touches only what it
// asks for. `invoke` above uses the very same `produce_list_range`, so this isn't a parallel
// implementation — it's the same producer path, exercised with different pull schedules.
// ---------------------------------------------------------------------------------------

/// **Eager drive:** produce the whole list once, up front (what the runtime memcpys into
/// guest memory before an eager call).
pub fn drive_eager(src: &mut ValSource, elem_ty: &Ty) -> Vec<u8> {
    let n = src.list_len(elem_ty);
    let mut out = Vec::new();
    src.produce_list_range(elem_ty, 0, n, &mut out);
    out
}

/// **Lazy drive:** model the guest pulling `value.lower` in a schedule of element ranges
/// (chunks, a window, reordered — whatever the guest's own control flow dictates). Only the
/// pulled ranges are ever produced; anything omitted is never touched (#383's free skips).
pub fn drive_lazy(src: &mut ValSource, elem_ty: &Ty, pulls: &[(usize, usize)]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(start, count) in pulls {
        src.produce_list_range(elem_ty, start, count, &mut out);
    }
    out
}
