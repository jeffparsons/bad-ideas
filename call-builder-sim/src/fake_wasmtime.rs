//! A deliberately tiny stand-in for the parts of `wasmtime::component` that matter for
//! modelling the *transfer* surfaces discussed in
//! [`bytecodealliance/wasmtime#13788`](https://github.com/bytecodealliance/wasmtime/issues/13788)
//! and issue #13 (the "2x2 matrix" of canonical-ABI transfers).
//!
//! This is **not** wasmtime. It exists only to reproduce the same *borrow structure* a
//! real embedding has — a `Store` that owns linear memory, argument/result buffers that
//! live *outside* the store, and host imports that the guest can call back into — so we
//! can prove (by actually compiling and running) that the proposed API ergonomics
//! compose in real Rust across all four quadrants of the matrix:
//!
//! ```text
//!                     params (in)                 results (out)
//!   host -> guest     bulk LOWER  (host bytes ->  bulk LIFT   (guest mem ->
//!   (export call)      guest mem)                  host view/bytes)
//!
//!   guest -> host     bulk LIFT   (guest mem ->   bulk LOWER  (host bytes ->
//!   (import call)      host view/bytes)            guest mem)
//! ```
//!
//! The two primitives underneath are [`lower_flat`] (host bytes -> guest memory) and
//! [`lift_flat`] / [`lift_flat_view`] (guest memory -> host bytes/view). The direction of
//! the *call* is orthogonal to the direction of a given slot's *data transfer*.

use std::rc::Rc;

pub(crate) fn align_up(offset: usize, align: usize) -> usize {
    (offset + align - 1) & !(align - 1)
}

/// A component-model-ish interface type. Includes a couple of deliberately *non-inline*
/// shapes (`String`, `Handle`) so the transferability gate has something to reject.
#[derive(Clone, Debug)]
pub enum Ty {
    F32,
    U32,
    Record(Vec<Ty>),
    List(Box<Ty>),
    /// (ptr, len) into memory with out-of-line character storage — *not* inline.
    String,
    /// An index into a per-instance table with ownership semantics — *not* inline.
    Handle,
}

impl Ty {
    /// Canonical-ABI-ish byte size of this type when stored in memory.
    pub fn size(&self) -> usize {
        match self {
            Ty::F32 | Ty::U32 => 4,
            Ty::Record(fields) => {
                let mut off = 0;
                let mut align = 1;
                for f in fields {
                    let a = f.align();
                    off = align_up(off, a) + f.size();
                    align = align.max(a);
                }
                align_up(off, align)
            }
            // A `list`/`string` stored in memory is a (ptr, len) pair of i32s.
            Ty::List(_) | Ty::String => 8,
            Ty::Handle => 4,
        }
    }

    pub fn align(&self) -> usize {
        match self {
            Ty::F32 | Ty::U32 | Ty::Handle => 4,
            Ty::Record(fields) => fields.iter().map(Ty::align).max().unwrap_or(1),
            Ty::List(_) | Ty::String => 4,
        }
    }

    /// "Inline" = a fixed-size, memcpy-transferable byte image with **no out-of-line
    /// storage and no ownership/pointer-bearing values**: scalars and records thereof.
    ///
    /// This is exactly the gate #13788 relies on: Wasmtime must be able to prove from
    /// reflected type info that `T` in `list<T>` is inline before the bulk fast path is
    /// legal. `string`, nested `list`, `resource`/handle all fail it.
    pub fn is_inline(&self) -> bool {
        match self {
            Ty::F32 | Ty::U32 => true,
            Ty::Record(fields) => fields.iter().all(Ty::is_inline),
            Ty::List(_) | Ty::String | Ty::Handle => false,
        }
    }

    /// Byte size of one element of a `list<T>` (or of a scalar treated as its own image).
    pub fn elem_size(&self) -> usize {
        match self {
            Ty::List(elem) => elem.size(),
            _ => self.size(),
        }
    }

    pub fn elem_align(&self) -> usize {
        match self {
            Ty::List(elem) => elem.align(),
            _ => self.align(),
        }
    }

    /// Can this type participate in a *bulk* transfer? For `list<T>` that means the
    /// element type is inline; a bare scalar/record must itself be inline.
    pub fn is_bulk_transferable(&self) -> bool {
        match self {
            Ty::List(elem) => elem.is_inline(),
            other => other.is_inline(),
        }
    }
}

/// A dynamically-typed value — the general, per-element "source" (stands in for
/// `wasmtime::component::Val`). Lowering one walks the tree element-by-element.
#[derive(Clone, Debug)]
pub enum Val {
    F32(f32),
    U32(u32),
    Record(Vec<Val>),
    List(Vec<Val>),
}

/// How the host *provides* a value to be **lowered** into guest memory — the shared
/// "provide" vocabulary used for both host→guest params and guest→host results.
///
/// The representation is chosen per slot, so one call freely mixes them:
///
/// * [`Source::Flat`] — the caller already holds the canonical (CABI-encoded) bytes, and
///   they are copied in as one `memcpy`. This covers a whole `list<inline T>` (flat bytes) **and** a
///   single pre-lowered value/record; the arity comes from the slot's [`Ty`].
/// * [`Source::Val`] — a dynamic value walked element-by-element (the general/slow path).
///
/// (A future `Source::Lazy` would defer the copy to a pull *during* the call; see
/// `DESIGN.md`. It slots in here without changing the borrow structure.)
pub enum Source<'a> {
    Flat(&'a [u8]),
    Val(Val),
}

/// A flattened core value (post-canonicalisation param/result). A `list` flattens to an
/// `I32` pointer plus an `I32` length.
#[derive(Clone, Copy, Debug)]
pub enum CoreVal {
    I32(i32),
    F32(f32),
}

impl CoreVal {
    pub fn as_i32(self) -> i32 {
        match self {
            CoreVal::I32(x) => x,
            CoreVal::F32(_) => panic!("expected i32 core value"),
        }
    }
}

/// The guest instance's linear memory plus a bump allocator standing in for
/// `cabi_realloc`.
pub struct Store {
    pub mem: Vec<u8>,
    next: usize,
}

impl Store {
    pub fn new(size: usize) -> Self {
        Store {
            mem: vec![0u8; size],
            next: 0,
        }
    }

    /// Bump-allocate `size` bytes aligned to `align`, returning the offset ("pointer").
    pub fn realloc(&mut self, size: usize, align: usize) -> usize {
        let ptr = align_up(self.next, align);
        let end = ptr + size;
        assert!(end <= self.mem.len(), "fake linear memory exhausted");
        self.next = end;
        ptr
    }

    /// Reset the arena — each top-level call re-lowers its arguments into fresh memory.
    pub fn reset(&mut self) {
        self.next = 0;
    }

    pub fn write(&mut self, ptr: usize, bytes: &[u8]) {
        self.mem[ptr..ptr + bytes.len()].copy_from_slice(bytes);
    }

    pub fn read_f32(&self, ptr: usize) -> f32 {
        let mut b = [0u8; 4];
        b.copy_from_slice(&self.mem[ptr..ptr + 4]);
        f32::from_le_bytes(b)
    }

    pub fn write_f32(&mut self, ptr: usize, v: f32) {
        self.mem[ptr..ptr + 4].copy_from_slice(&v.to_le_bytes());
    }
}

/// A host function the guest can import and call back into. This is the *shape* half of
/// the import quadrants; the handler is invoked with an [`ImportCall`] that exposes the
/// guest-provided args (bulk lift) and accepts host result bytes (bulk lower).
pub struct HostImport {
    pub params: Vec<Ty>,
    pub results: Vec<Ty>,
    pub handler: Box<dyn Fn(&mut ImportCall)>,
}

impl HostImport {
    pub fn new(
        params: Vec<Ty>,
        results: Vec<Ty>,
        handler: impl Fn(&mut ImportCall) + 'static,
    ) -> HostImport {
        HostImport {
            params,
            results,
            handler: Box::new(handler),
        }
    }
}

/// A guest function: its parameter/result interface types, the imports it may call, plus
/// a Rust closure standing in for the compiled wasm body. The body receives a [`Caller`]
/// (linear memory + imports) and the flattened core params, and returns flattened core
/// results.
/// The compiled-guest stand-in: given a [`Caller`] and flattened core params, produce
/// flattened core results.
pub type GuestBody = dyn Fn(&mut Caller, &[CoreVal]) -> Vec<CoreVal>;

pub struct Func {
    pub params: Vec<Ty>,
    pub results: Vec<Ty>,
    pub imports: Vec<HostImport>,
    pub body: Box<GuestBody>,
}

impl Func {
    pub fn new(
        params: Vec<Ty>,
        results: Vec<Ty>,
        body: impl Fn(&mut Caller, &[CoreVal]) -> Vec<CoreVal> + 'static,
    ) -> Rc<Func> {
        Func::with_imports(params, results, Vec::new(), body)
    }

    pub fn with_imports(
        params: Vec<Ty>,
        results: Vec<Ty>,
        imports: Vec<HostImport>,
        body: impl Fn(&mut Caller, &[CoreVal]) -> Vec<CoreVal> + 'static,
    ) -> Rc<Func> {
        Rc::new(Func {
            params,
            results,
            imports,
            body: Box::new(body),
        })
    }
}

/// The execution context handed to a guest body: `&mut Store` (linear memory) plus the
/// imports it may call. Stands in for `wasmtime::StoreContextMut` / `Caller`.
pub struct Caller<'a> {
    store: &'a mut Store,
    imports: &'a [HostImport],
}

impl<'a> Caller<'a> {
    pub(crate) fn new(store: &'a mut Store, imports: &'a [HostImport]) -> Self {
        Caller { store, imports }
    }

    /// Direct access to linear memory (the guest body reads/writes its params here).
    pub fn store(&mut self) -> &mut Store {
        self.store
    }

    /// The guest calls a host import. This drives quadrants 3 (guest->host **params**,
    /// bulk lift) and 4 (guest->host **results**, bulk lower):
    ///
    /// * the guest's core args (ptr/len pairs into *its* memory) are wrapped in an
    ///   [`ImportCall`], which lets the host lift borrowed views of them;
    /// * whatever list results the host sets are lowered back into guest memory, and the
    ///   resulting (ptr, len) core values are handed back to the guest.
    pub fn call_import(&mut self, index: usize, core_args: &[CoreVal]) -> Vec<CoreVal> {
        // Copy the slice reference out so `import` doesn't keep `self` borrowed while we
        // reborrow `self.store` mutably below (references are `Copy`).
        let imports: &'a [HostImport] = self.imports;
        let import = &imports[index];
        let mut ic = ImportCall {
            store: self.store,
            param_tys: &import.params,
            result_tys: &import.results,
            args: core_args.to_vec(),
            outs: (0..import.results.len()).map(|_| None).collect(),
        };
        (import.handler)(&mut ic);
        ic.finish()
    }
}

/// The handle passed to a host import handler. It borrows the guest's `Store` for the
/// duration of the callback only, which is exactly the natural lifetime boundary #13
/// highlights: views handed out here cannot escape the callback.
///
/// Its two halves mirror the two dual vocabularies: [`args`](Self::args) hands back a
/// [`Lifted`] accessor to *receive* the guest's params (pick `view`/`copy`/`val` per
/// slot), and [`set`](Self::set) *provides* each result via a [`Source`] (pick
/// `Flat`/`Val` per slot). Reading args (`&self`) and setting results (`&mut self`) are
/// naturally sequenced by the borrow checker, so a borrowed arg view cannot straddle a
/// result write.
pub struct ImportCall<'s> {
    store: &'s mut Store,
    param_tys: &'s [Ty],
    result_tys: &'s [Ty],
    args: Vec<CoreVal>,
    outs: Vec<Option<Vec<CoreVal>>>,
}

impl ImportCall<'_> {
    /// **Guest → host params (lift).** A scoped accessor over the guest-provided
    /// arguments. The borrow is tied to `&self`, so the views it yields cannot be retained
    /// across a later [`set`](Self::set) (which needs `&mut self`) or escape the handler.
    pub fn args(&self) -> Lifted<'_> {
        Lifted::new(self.store, self.param_tys, &self.args)
    }

    /// **Guest → host results (lower).** Provide result `index` from any [`Source`] (flat
    /// bytes or a `Val`). It is lowered into guest memory immediately; the resulting core
    /// values are handed back to the guest when the handler returns.
    pub fn set(&mut self, index: usize, src: Source) {
        let ty = &self.result_tys[index];
        self.outs[index] = Some(lower_source(self.store, ty, src));
    }

    /// Assemble the flattened core results in slot order.
    fn finish(self) -> Vec<CoreVal> {
        let mut core = Vec::new();
        for (i, out) in self.outs.into_iter().enumerate() {
            core.extend(out.unwrap_or_else(|| panic!("host did not set result {i}")));
        }
        core
    }
}

/// Guest-owned values exposed for **lifting**. The receive primitive is [`Lifted::get`],
/// which yields a slot as whatever *sink type* you name: `&[u8]` (zero-copy view),
/// `Vec<u8>` (owned copy), or [`Val`] (dynamic). This is the dual of the provide primitive
/// `arg(ArgSource)` — there you commit to a source *value*, here you request a sink *type*.
/// `view` / `copy` / `val` are sugar over `get`, and the same slot may be pulled more than
/// one way.
pub struct Lifted<'a> {
    store: &'a Store,
    tys: &'a [Ty],
    core: &'a [CoreVal],
    slots: Vec<usize>,
}

impl<'a> Lifted<'a> {
    pub(crate) fn new(store: &'a Store, tys: &'a [Ty], core: &'a [CoreVal]) -> Self {
        Lifted {
            store,
            tys,
            core,
            slots: core_slot_offsets(tys),
        }
    }

    pub fn len(&self) -> usize {
        self.tys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tys.is_empty()
    }

    /// **The receive primitive.** Pull slot `index` as any type `T` that implements
    /// [`FromResult`] — `&[u8]`, `Vec<u8>`, or [`Val`]. Mirrors `arg(ArgSource)`: name a
    /// source *value* to provide, name a sink *type* to receive. A `&[u8]` sink borrows
    /// `&self`, so a view stays confined to the enclosing scope.
    pub fn get<'r, T: FromResult<'r>>(&'r self, index: usize) -> T {
        T::from_result(self, index)
    }

    /// Sugar for `get::<&[u8]>` — a zero-copy borrowed view (inline slots only).
    pub fn view(&self, index: usize) -> &[u8] {
        self.get(index)
    }

    /// Sugar for `get::<Vec<u8>>` — an owned copy of the slot's bytes.
    pub fn copy(&self, index: usize) -> Vec<u8> {
        self.get(index)
    }

    /// Sugar for `get::<Val>` — a dynamic value (works for any slot).
    pub fn val(&self, index: usize) -> Val {
        self.get(index)
    }

    fn list_ptr_count(&self, index: usize) -> (usize, usize) {
        let slot = self.slots[index];
        (
            self.core[slot].as_i32() as usize,
            self.core[slot + 1].as_i32() as usize,
        )
    }

    fn lift_view(&self, index: usize) -> &[u8] {
        let ty = &self.tys[index];
        assert!(ty.is_bulk_transferable(), "slot {index} is not an inline type");
        let (ptr, count) = self.list_ptr_count(index);
        lift_flat_view(self.store, ty, ptr, count)
    }

    fn lift_val(&self, index: usize) -> Val {
        let ty = &self.tys[index];
        let slot = self.slots[index];
        match ty {
            Ty::List(elem) => {
                let (ptr, count) = self.list_ptr_count(index);
                let stride = elem.size();
                let mut items = Vec::with_capacity(count);
                let mut off = ptr;
                for _ in 0..count {
                    items.push(lift_scalar_or_record(self.store, elem, off));
                    off += stride;
                }
                Val::List(items)
            }
            Ty::F32 => match self.core[slot] {
                CoreVal::F32(x) => Val::F32(x),
                CoreVal::I32(x) => Val::F32(f32::from_bits(x as u32)),
            },
            Ty::U32 => Val::U32(self.core[slot].as_i32() as u32),
            other => panic!("lifting {other:?} to a Val is not modelled"),
        }
    }
}

/// The **receive** vocabulary: a sink type produced by [`Lifted::get`]. The dual of
/// [`ArgSource`](crate::call_builder::ArgSource) on the provide side — one enum of source
/// *values* there, one set of sink *types* here. The split (value vs type parameter) is
/// forced: every source lowers to the same thing (bytes in guest memory), but the three
/// receive modes produce genuinely different Rust types.
pub trait FromResult<'a>: Sized {
    fn from_result(results: &'a Lifted<'_>, index: usize) -> Self;
}

/// Zero-copy view — borrows the `Lifted` (hence the enclosing scope).
impl<'a> FromResult<'a> for &'a [u8] {
    fn from_result(results: &'a Lifted<'_>, index: usize) -> Self {
        results.lift_view(index)
    }
}

/// Owned copy of the slot's canonical bytes.
impl<'a> FromResult<'a> for Vec<u8> {
    fn from_result(results: &'a Lifted<'_>, index: usize) -> Self {
        results.lift_view(index).to_vec()
    }
}

/// Dynamic value, walked element-by-element (the only mode that works for non-inline slots).
impl<'a> FromResult<'a> for Val {
    fn from_result(results: &'a Lifted<'_>, index: usize) -> Self {
        results.lift_val(index)
    }
}

/// The starting core-slot index of each parameter, given that a `list`/`string` occupies
/// two core slots (ptr, len) and every other type occupies one.
pub(crate) fn core_slot_offsets(tys: &[Ty]) -> Vec<usize> {
    let mut offs = Vec::with_capacity(tys.len());
    let mut n = 0;
    for ty in tys {
        offs.push(n);
        n += match ty {
            Ty::List(_) | Ty::String => 2,
            _ => 1,
        };
    }
    offs
}

/// **bulk LOWER.** Copy an inline argument's pre-encoded element image into freshly
/// allocated linear memory in one shot. Returns `(ptr, element_count)`. This is the fast
/// path #13788 asks for, and the shared primitive behind *host->guest params* and
/// *guest->host results*.
pub(crate) fn lower_flat(store: &mut Store, ty: &Ty, bytes: &[u8]) -> (usize, usize) {
    let count = bytes.len() / ty.elem_size();
    let ptr = store.realloc(bytes.len(), ty.elem_align());
    store.write(ptr, bytes); // <-- the memcpy
    (ptr, count)
}

/// **bulk LIFT (borrowed view).** Borrow `count` inline elements of guest memory in place —
/// zero copy. The caller controls the dynamic extent in which the view is valid; here
/// that is enforced structurally by the borrow of `&Store`.
pub(crate) fn lift_flat_view<'s>(store: &'s Store, ty: &Ty, ptr: usize, count: usize) -> &'s [u8] {
    let len = count * ty.elem_size();
    &store.mem[ptr..ptr + len]
}

/// Lower a `Val` argument by walking it element-by-element. **This is the slow path** we
/// are trying to give callers an alternative to; kept here for the contrast.
pub(crate) fn lower_val(store: &mut Store, ty: &Ty, val: &Val, core: &mut Vec<CoreVal>) {
    match (ty, val) {
        (Ty::F32, Val::F32(x)) => core.push(CoreVal::F32(*x)),
        (Ty::U32, Val::U32(x)) => core.push(CoreVal::I32(*x as i32)),
        (Ty::List(elem), Val::List(items)) => {
            let stride = elem.size();
            let ptr = store.realloc(items.len() * stride, elem.align());
            let mut off = ptr;
            for item in items {
                store_scalar_or_record(store, elem, item, off);
                off += stride;
            }
            core.push(CoreVal::I32(ptr as i32));
            core.push(CoreVal::I32(items.len() as i32));
        }
        // A record passed *by value* flattens its fields straight into core values.
        (Ty::Record(fields), Val::Record(vals)) => {
            for (fty, fval) in fields.iter().zip(vals) {
                lower_val(store, fty, fval, core);
            }
        }
        _ => panic!("val/type mismatch during lowering"),
    }
}

/// Lower one value from any [`Source`], returning its flattened core representation. The
/// single choke point behind both *host→guest params* and *guest→host results*.
pub(crate) fn lower_source(store: &mut Store, ty: &Ty, src: Source) -> Vec<CoreVal> {
    let mut core = Vec::new();
    match src {
        Source::Flat(bytes) => lower_flat_image(store, ty, bytes, &mut core),
        Source::Val(v) => lower_val(store, ty, &v, &mut core),
    }
    core
}

/// Lower pre-encoded canonical bytes. A `list<inline T>` image is copied into memory as one
/// `memcpy` and represented by (ptr, len); a **single** pre-lowered value/record is
/// flattened straight into core values, no allocation. Either way the caller skips
/// building a [`Val`] tree.
fn lower_flat_image(store: &mut Store, ty: &Ty, bytes: &[u8], core: &mut Vec<CoreVal>) {
    match ty {
        Ty::List(_) => {
            let (ptr, len) = lower_flat(store, ty, bytes);
            core.push(CoreVal::I32(ptr as i32));
            core.push(CoreVal::I32(len as i32));
        }
        _ => {
            flatten_image(ty, bytes, 0, core);
        }
    }
}

/// Read a single value's canonical memory image out of `bytes` at `off`, flattening it to
/// core values. Returns the offset just past the value.
fn flatten_image(ty: &Ty, bytes: &[u8], off: usize, core: &mut Vec<CoreVal>) -> usize {
    match ty {
        Ty::F32 => {
            core.push(CoreVal::F32(f32::from_le_bytes(
                bytes[off..off + 4].try_into().unwrap(),
            )));
            off + 4
        }
        Ty::U32 => {
            core.push(CoreVal::I32(i32::from_le_bytes(
                bytes[off..off + 4].try_into().unwrap(),
            )));
            off + 4
        }
        Ty::Record(fields) => {
            let mut o = off;
            for f in fields {
                o = align_up(o, f.align());
                o = flatten_image(f, bytes, o, core);
            }
            align_up(o, ty.align())
        }
        other => panic!("flattening {other:?} from an image is not modelled"),
    }
}

fn store_scalar_or_record(store: &mut Store, ty: &Ty, val: &Val, off: usize) {
    match (ty, val) {
        (Ty::F32, Val::F32(x)) => store.write(off, &x.to_le_bytes()),
        (Ty::U32, Val::U32(x)) => store.write(off, &x.to_le_bytes()),
        (Ty::Record(fields), Val::Record(vals)) => {
            let mut o = off;
            for (fty, fval) in fields.iter().zip(vals) {
                o = align_up(o, fty.align());
                store_scalar_or_record(store, fty, fval, o);
                o += fty.size();
            }
        }
        _ => panic!("val/type mismatch during element store"),
    }
}

/// Lift one in-memory value (scalar or record) into a [`Val`] — the inverse of
/// [`store_scalar_or_record`], used to walk `list<T>` elements into a `Val::List`.
fn lift_scalar_or_record(store: &Store, ty: &Ty, off: usize) -> Val {
    match ty {
        Ty::F32 => Val::F32(store.read_f32(off)),
        Ty::U32 => Val::U32(u32::from_le_bytes(
            store.mem[off..off + 4].try_into().unwrap(),
        )),
        Ty::Record(fields) => {
            let mut o = off;
            let mut vals = Vec::with_capacity(fields.len());
            for f in fields {
                o = align_up(o, f.align());
                vals.push(lift_scalar_or_record(store, f, o));
                o += f.size();
            }
            Val::Record(vals)
        }
        other => panic!("lifting {other:?} to a Val is not modelled"),
    }
}
