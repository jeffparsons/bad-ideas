//! A deliberately tiny stand-in for the parts of `wasmtime::component` that matter for
//! modelling the call-builder API.
//!
//! This is **not** wasmtime. It exists only to reproduce the same *borrow structure* a
//! real embedding has — a `&mut Store` that owns linear memory, plus argument buffers
//! that live *outside* the store and are borrowed only for the duration of one call —
//! so that we can prove (by actually compiling and running) that the call-builder's
//! prepare-once / borrow-per-call ergonomics compose in real Rust.

use std::rc::Rc;

pub(crate) fn align_up(offset: usize, align: usize) -> usize {
    (offset + align - 1) & !(align - 1)
}

/// A component-model-ish interface type — only the shapes we need.
#[derive(Clone, Debug)]
pub enum Ty {
    F32,
    U32,
    Record(Vec<Ty>),
    List(Box<Ty>),
}

impl Ty {
    /// Canonical-ABI-ish byte size.
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
            // A `list` stored in memory is a (ptr, len) pair of i32s.
            Ty::List(_) => 8,
        }
    }

    pub fn align(&self) -> usize {
        match self {
            Ty::F32 | Ty::U32 => 4,
            Ty::Record(fields) => fields.iter().map(Ty::align).max().unwrap_or(1),
            Ty::List(_) => 4,
        }
    }

    /// "Flat" = memcpy-transferable element image: scalars and records/lists thereof, no
    /// out-of-line storage. Everything in this lean model qualifies, but the method is
    /// kept so the lowering branches on it exactly as a real implementation would.
    pub fn is_flat(&self) -> bool {
        match self {
            Ty::F32 | Ty::U32 => true,
            Ty::Record(fields) => fields.iter().all(Ty::is_flat),
            Ty::List(elem) => elem.is_flat(),
        }
    }

    /// Byte size of one element of a `list<T>`.
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

/// A flattened core value (post-canonicalisation param). A `list` flattens to an
/// `I32` pointer plus an `I32` length.
#[derive(Clone, Copy, Debug)]
pub enum CoreVal {
    I32(i32),
    F32(f32),
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

    /// Reset the arena — each call re-lowers its arguments into fresh memory.
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

/// A guest function: its parameter interface types plus a Rust closure standing in for
/// the compiled wasm body. The body receives the flattened core params and reads/mutates
/// `Store::mem`.
pub struct Func {
    pub params: Vec<Ty>,
    pub body: Box<dyn Fn(&mut Store, &[CoreVal])>,
}

impl Func {
    pub fn new(params: Vec<Ty>, body: impl Fn(&mut Store, &[CoreVal]) + 'static) -> Rc<Func> {
        Rc::new(Func {
            params,
            body: Box::new(body),
        })
    }
}

/// Lower a flat argument: a single memcpy of the pre-encoded element image into freshly
/// allocated linear memory. Returns `(ptr, element_count)`. **This is the fast path.**
pub(crate) fn lower_flat(store: &mut Store, ty: &Ty, bytes: &[u8]) -> (usize, usize) {
    let count = bytes.len() / ty.elem_size();
    let ptr = store.realloc(bytes.len(), ty.elem_align());
    store.write(ptr, bytes); // <-- the memcpy
    (ptr, count)
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
