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

/// A component-model-ish interface type. Includes a couple of deliberately *non-flat*
/// shapes (`String`, `Handle`) so the transferability gate has something to reject.
#[derive(Clone, Debug)]
pub enum Ty {
    F32,
    U32,
    Record(Vec<Ty>),
    List(Box<Ty>),
    /// (ptr, len) into memory with out-of-line character storage — *not* flat.
    String,
    /// An index into a per-instance table with ownership semantics — *not* flat.
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

    /// "Flat" = a fixed-size, memcpy-transferable byte image with **no out-of-line
    /// storage and no ownership/pointer-bearing values**: scalars and records thereof.
    ///
    /// This is exactly the gate #13788 relies on: Wasmtime must be able to prove from
    /// reflected type info that `T` in `list<T>` is flat before the bulk fast path is
    /// legal. `string`, nested `list`, `resource`/handle all fail it.
    pub fn is_flat(&self) -> bool {
        match self {
            Ty::F32 | Ty::U32 => true,
            Ty::Record(fields) => fields.iter().all(Ty::is_flat),
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

    /// Can this type participate in a *bulk* flat transfer? For `list<T>` that means the
    /// element type is flat; a bare scalar/record must itself be flat.
    pub fn is_bulk_transferable(&self) -> bool {
        match self {
            Ty::List(elem) => elem.is_flat(),
            other => other.is_flat(),
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
    /// * whatever flat lists the host sets are lowered back into guest memory, and the
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
            arg_slots: core_slot_offsets(&import.params),
            outs: (0..import.results.len()).map(|_| None).collect(),
        };
        (import.handler)(&mut ic);
        ic.finish()
    }
}

/// The handle passed to a host import handler. It borrows the guest's `Store` for the
/// duration of the callback only, which is exactly the natural lifetime boundary #13
/// highlights: views handed out here cannot escape the callback.
pub struct ImportCall<'s> {
    store: &'s mut Store,
    param_tys: &'s [Ty],
    result_tys: &'s [Ty],
    args: Vec<CoreVal>,
    arg_slots: Vec<usize>,
    outs: Vec<Option<Vec<u8>>>,
}

impl<'s> ImportCall<'s> {
    /// **Quadrant 3 (guest -> host params, bulk lift).** Borrow the flat bytes of the
    /// guest-provided `list<flat T>` argument `index` as a callback-scoped view. The
    /// borrow is tied to `&self`, so it cannot be retained across a later
    /// [`return_flat_list`](Self::return_flat_list) (which needs `&mut self`) or escape
    /// the handler.
    pub fn arg_list_view(&self, index: usize) -> &[u8] {
        let ty = &self.param_tys[index];
        assert!(ty.is_bulk_transferable(), "arg {index} is not flat-transferable");
        let slot = self.arg_slots[index];
        let ptr = self.args[slot].as_i32() as usize;
        let count = self.args[slot + 1].as_i32() as usize;
        lift_flat_view(self.store, ty, ptr, count)
    }

    /// **Quadrant 4 (guest -> host results, bulk lower).** Provide host-owned bytes as
    /// the flat `list<flat T>` result `index`. They are copied into guest memory when the
    /// handler returns.
    pub fn return_flat_list(&mut self, index: usize, bytes: &[u8]) {
        let ty = &self.result_tys[index];
        assert!(ty.is_bulk_transferable(), "result {index} is not flat-transferable");
        self.outs[index] = Some(bytes.to_vec());
    }

    /// Lower every host-provided result list into guest memory and hand back the core
    /// (ptr, len) values the guest will receive.
    fn finish(self) -> Vec<CoreVal> {
        let mut core = Vec::new();
        for (i, out) in self.outs.into_iter().enumerate() {
            let ty = &self.result_tys[i];
            let bytes = out.unwrap_or_else(|| panic!("host did not set result {i}"));
            let (ptr, count) = lower_flat(self.store, ty, &bytes);
            core.push(CoreVal::I32(ptr as i32));
            core.push(CoreVal::I32(count as i32));
        }
        core
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

/// **bulk LOWER.** Copy a flat argument's pre-encoded element image into freshly
/// allocated linear memory in one shot. Returns `(ptr, element_count)`. This is the fast
/// path #13788 asks for, and the shared primitive behind *host->guest params* and
/// *guest->host results*.
pub(crate) fn lower_flat(store: &mut Store, ty: &Ty, bytes: &[u8]) -> (usize, usize) {
    let count = bytes.len() / ty.elem_size();
    let ptr = store.realloc(bytes.len(), ty.elem_align());
    store.write(ptr, bytes); // <-- the memcpy
    (ptr, count)
}

/// **bulk LIFT (owned).** Copy `count` flat elements out of guest memory into a fresh
/// host-owned byte image. The shared primitive behind *host->guest results* (eager
/// copy-out) and *guest->host params* (when the host wants to keep the bytes).
pub(crate) fn lift_flat(store: &Store, ty: &Ty, ptr: usize, count: usize) -> Vec<u8> {
    lift_flat_view(store, ty, ptr, count).to_vec()
}

/// **bulk LIFT (borrowed view).** Borrow `count` flat elements of guest memory in place —
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
        _ => panic!("val/type mismatch during lowering"),
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
