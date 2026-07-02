# Design notes: the canonical-ABI transfer matrix

Working notes for [issue #13]. The original call-builder prototype covered one quadrant
(host → guest **params**). This is an initial pass at the *whole* matrix, with the
naming, the cost table, and the forward-looking (lazy + async) parts #13 asked for.
The companion crate compiles and runs all four quadrants; this doc is the reasoning.

[issue #13]: https://github.com/jeffparsons/bad-ideas/issues/13

---

## 1. The reframe: two operations, four surfaces

The call builder is one ergonomic surface over a more general operation. The two
underlying operations are just **lower** (a value the host *provides* → guest memory) and
**lift** (a value the host *receives* ← guest memory). Which one a slot uses is determined
by **who owns the memory**, and that is *orthogonal to the direction of the call*:

| | params (in) | results (out) |
| --- | --- | --- |
| **host → guest** (export call) | host **provides** → *lower* | host **receives** → *lift* |
| **guest → host** (import call) | host **receives** → *lift* | host **provides** → *lower* |

The diagonal symmetry: the two "host provides" cells are both *lower*; the two "host
receives" cells are both *lift*. Two operations, four surfaces.

### Representation is a separate, per-slot choice — mix freely

"Bulk" is **not** the definition of a transfer; it is one *representation* a slot can use.
Each slot independently picks how it is lowered or lifted, so a single call mixes bulk and
dynamic in both directions (the `demo_mixed_pipeline` does exactly this). Two dual
vocabularies capture the choice:

- **provide** (lower) a value from a **`Source`**:
  - `Flat(&[u8])` — the caller already holds the canonical (CABI-encoded) bytes; copied in
    as one `memcpy`. This covers a whole `list<inline T>` **and a single pre-lowered
    value/record** — arity comes from the slot type, so it is not list-only.
  - `Val` — a dynamic value walked element-by-element (general/slow).
  - *(future)* `Lazy` — defer the copy to a pull *during* the call (§5); slots in here.
- **receive** (lift) a value from a **`Lifted`** accessor, choosing *at the pull site*:
  `view` (zero-copy borrow), `copy` (owned bytes), or `val` (dynamic).

Both sides are **one primitive + optional sugar** — the arg decision (§4 Q1) applied to
results too. You *provide* by naming a source **value**, `arg(ArgSource)`, and *receive* by
naming a sink **type**, `get::<T>()` for `T ∈ {&[u8], Vec<u8>, Val}`, with `arg_val`/… and
`view`/`copy`/`val` as sugar over each. The one residual difference is **forced, not
chosen**: a source is a *value* because every source lowers to the same thing (bytes in guest
memory), whereas a sink is a *type* because the three receive modes produce genuinely
different Rust types (`&[u8]` / `Vec<u8>` / `Val`) that can't share one return. It also
mirrors reality — you can't lower what you haven't described, but you can lift an
already-materialised value however you like, even more than one way.

The **bulk** path is offered only where the type is provably *inline*: a fixed CABI layout with
no transitive strings, lists, resources, handles, or other pointer/ownership-bearing values.
(In the sim, `Ty::is_inline` is that gate; `Ty::String`/`Ty::Handle` exist purely to be
rejected by it.) Anything else falls back to `Val`, per slot — which is why "combine bulk
AND dynamic" is the default, not a special case.

So the four cells reduce to **one `Source`/`Lifted` pair reused everywhere**; the call
direction only decides which slots are provided and which are received. The call builder is
the export-side surface; `ImportCall` is its import-side mirror, speaking the same two
vocabularies.

---

## 2. Where the costs fall (the table #13 asked for)

The "nontrivial operations" involved in moving a `list<inline T>` of *M* elements across the
boundary, and **who pays them and how often**, under three schemes:

- **A — dynamic `Val`s:** `Func::call(&[Val])`, everything reflected at run time.
- **B — `bindgen!`:** static Rust types generated at host compile time.
- **C — prepared shape + bound flat buffers:** the proposal (this crate).

| Nontrivial operation | A: dynamic `Val`s | B: `bindgen!` | C: prepared + bound flat |
| --- | --- | --- | --- |
| **Shape validation** (arg matches the param interface type) | Wasmtime, **per element, per call** — walks the `Val` tree | compiler, **compile time** — types encode it | Wasmtime, **once at `prepare`** — validate shape + prove `T` inline; per call only a cheap len/stride check |
| **Host-side encode** of each element into canonical bytes | embedder, **per element, per call** — build a `Val` tree (alloc/box) | generated lowering, **per element, per call** (often optimises toward memcpy) | embedder, **~zero** — the host column already *is* the canonical image (e.g. an ECS `&[Vec2]`) |
| **Content validation** (bool/char/enum discriminants) | Wasmtime, per element, per call | compiler + per element | Wasmtime, **once**: pure-POD `T` ⇒ none; constrained `T` ⇒ a single validation sweep at bind |
| **Guest allocation** (`cabi_realloc`) | Wasmtime → guest, per call | per call | per call (or a reused/caller-supplied buffer) |
| **Byte copy** across the boundary | Wasmtime, **per element** (interleaved with encode) | per-element loop (may vectorise) | Wasmtime, **one bulk `memcpy`** per call |
| **Monomorphisation** | none — fully dynamic ✅ | compiler, **per concrete type** — *precludes types unknown at host build time* ❌ | none — fully dynamic ✅ |

The punchline, and the reason the API is worth splitting into two objects:

- Scheme **C moves shape validation from per-call/per-element to _once_** — that is the
  **prepared shape** (`PreparedCall`, no buffers bound).
- It **moves per-element encode+copy to a single `memcpy` at the invocation** — that is the
  **bound call** (`BoundCall`, buffers bound for exactly one call).
- And it keeps A's fully-dynamic property (no monomorphisation), so a generic ECS host can
  drive guest systems whose types it never saw at compile time.

The two cost frequencies (once vs per-call) are *why* "prepared shape" and "bound call"
are distinct nouns rather than one object.

---

## 3. Naming (issue #13's first ask)

The symmetry to preserve: a **shape** layer (validated, no concrete args) and a **bound**
layer (concrete args, one invocation), echoed on both the export and import sides. #13's
two specific gripes were that "prepared" doesn't say *shape-without-args*, and that
starting a *builder* with `.call()` reads oddly.

The prototype now uses SQL-flavoured verbs — **prepare → bind → invoke** — which are the
most widely understood prepare-then-execute vocabulary and match the "bind" #13 suggested:

| slot | recommended | alternatives considered |
| --- | --- | --- |
| export shape (reusable) | `PreparedCall` | `CallShape`, `CallSignature`, `CallPlan` |
| export bound (per-call) | `BoundCall<'a>` | `CallBinding`, `Invocation` |
| shape → bound verb | `.bind()` | `.with()`, `.begin()` |
| run verb | `.invoke()` | `.execute()`, `.call()` |
| provide vocabulary (lower) | `Source` (`Flat` \| `Val`) | `Arg`, `Lowerable` |
| receive vocabulary (lift) — shared by results **and** import args | `Lifted<'a>` (`view`/`copy`/`val`) | `Results`, `ResultView`, `Received` |
| import shape (registered) | `PreparedImport` *(sim: `HostImport`)* | `ImportShape`, `HostFn` |
| import active handle | `ImportCall<'s>` | `ImportInvocation`, `Incoming` |

**Directions on the shape noun.** `PreparedCall` is familiar but a touch generic. If the
"this is only the *shape*" reading matters more than SQL familiarity, `CallShape` /
`CallSignature` say it outright and still pair cleanly (`CallShape` → `BoundCall`).
Recommendation: keep **`PreparedCall` + `.bind()` + `BoundCall`** as the default (verbs win
the most clarity for the least novelty), and treat `CallShape` as the fallback if
"prepared" keeps reading as ambiguous. The sim uses `HostImport` for the import shape as a
placeholder; `PreparedImport` would restore the symmetry if we want it.

Whatever the nouns, the honest operations underneath should be named on both sides as
**lower** / **lift** (with "bulk" as the single-`memcpy` representation of each), so the docs can
point at one `Source`/`Lifted` pair for all four cells.

---

## 4. The four surfaces, with directions and recommendations

### Q1 — host → guest params (lower) — `PreparedCall` / `BoundCall`

The original prototype. Each arg picks its representation (`Val`, `flat_in`, `flat_inout`),
validated once; the bound call borrows the buffers for exactly one `invoke`, which consumes
it. This is settled; the borrow proof is the two `compile_fail` doctests.

**Arg supply style — settled.** The primitive is the explicit enum method,
`bound.arg(ArgSource::Val(v))`; the `arg_val` / `arg_flat_in` / `arg_flat_inout` methods are
*optional convenience* over it (`arg_val(v)` = `arg(ArgSource::Val(v))`). Rationale:

- The enum keeps the representation choice **explicit at the call site** — which is the point
  of "you commit to how you provide up front" (§1). An `Into`-style `arg(123i32)` /
  `arg(&buf)` is rejected: `&[u8]` can't disambiguate *flat bytes to memcpy* from *a list to
  walk as a `Val`*, and that intent must stay visible.
- The enum is the natural surface for **programmatic construction** — a generic dispatcher
  building calls from reflected type info loops over `ArgSource`s (and can take a slice/
  iterator form), where per-arg fluent methods would force a `match`-and-reassign per step.
- The sugar is worth keeping for **hand-written, fixed-arity** calls: it strips a nesting
  layer (`arg_val(Val::i32(1))` vs `arg(ArgSource::Val(Val::i32(1)))`), and the names mirror
  the variants 1:1 (`ArgSource::FlatIn` ↔ `arg_flat_in`), which aids discovery/learnability.
  Prefix-first (`arg_*`) is deliberate so the methods cluster in autocomplete.

The same convention applies symmetrically to the receive side (`Lifted::view/copy/val`) and
to the import surface (`ImportCall::set(i, Source)` with any future `set_val`/`set_flat`
sugar), so all four cells read the same way.

Since the merge of the `Source`/`Lifted` vocabularies (§1), Q1's provide side is literally
the same `Source` enum the import result side uses, and its `flat_in`/`val` sources go
through one `lower_source` choke point — so a single pre-lowered value/record flattens
by-value identically on both sides. `flat_inout` stays export-only, because "guest mutates a
host buffer in place" only makes sense for a param the host owns across the call — and it is
**deferred from a first implementation**: the Component Model has no in-out params, so it is
not expressible without caller-supplied buffers ([component-model#369]) or an
arg-plus-returned-list rewrite. See the caveat in §8.

### Q2 — host → guest results (lift)

Guest result memory is valid only between "guest returned" and "post-return cleanup", so a
freely-returned borrowed result is unsound. Three directions:

1. **Scoped callback** — `invoke_scoped(store, |results| …)`. Wasmtime owns the validity
   window; views can't escape (proven by a `compile_fail`); cleanup runs after the closure.
   - *Pros:* safe by construction; zero-copy; the scope generalises to async (§6) and to
     lazy post-return timing; matches Alex's sketch in #13.
   - *Cons:* inversion of control; the view can't be stored for later.
2. **Returned guard** — `let out = bound.invoke(store)?; out.view(0)`; a guard holding the
   store borrow, cleanup on `Drop`.
   - *Pros:* natural control flow, no closure.
   - *Cons:* the guard pins `&mut Store` for as long as it's held, blocking the next call;
     the lifetime threads through caller code; unsound-prone under async (holding across an
     `.await` that runs other guest code).
3. **Eager copy-out** — `invoke_collect(store) -> Vec<Vec<u8>>`.
   - *Pros:* trivial; host owns the bytes; no lifetimes.
   - *Cons:* always copies, even when the host only wanted to fold a checksum.

**Recommendation:** make **(1) the primitive** and layer **(3) on top** for convenience
(the sim ships both, via the `Lifted` accessor: `invoke_scoped` yields it, `invoke_collect`
folds `.copy()` over it). Avoid **(2)** as the foundation — it is the one shape that fights
both reuse and async.

The receive vocabulary is **one primitive + optional sugar**, mirroring the arg decision (Q1
above): `get::<T>(i)` is the primitive — name the sink *type* — with `view` (`&[u8]`),
`copy` (`Vec<u8>`), and `val` (`Val`) as sugar over it. The choice is made **per result at
the pull site**, so no shape-time `ResultSpec` exists — and none is wanted: a `View` spec
would still force a scope, and mixing it with owned results in one `invoke` gets awkward, for
no gain over deciding at the pull site (the value already exists by then; §1's
producer/consumer point).

### Q3 — guest → host params (lift) — `ImportCall::args() -> Lifted`

Inside a host import handler, the guest's args are already lifetime-bounded by the callback —
the *most defensible* place to expose borrowed guest memory. The import side now hands back
the **same `Lifted` accessor** as export results, so the host picks `view` (zero-copy),
`copy` (owned), or `val` (dynamic) per arg. `args()` borrows `&self`, so any view it yields
cannot be retained across a later `set` (which needs `&mut self`) or escape the handler —
the natural read-inputs-then-write-outputs ordering falls straight out of the borrow checker.

### Q4 — guest → host results (lower) — `ImportCall::set(i, Source)`

The mirror of Q1's provide side, from inside the import handler, taking the **same `Source`**
(so results can be `Flat` *or* `Val`, per slot). One remaining sub-direction on how the flat
bytes reach guest memory:

- **(a) Hand over host bytes** — `set(i, Source::Flat(&bytes))`; Wasmtime allocs in guest
  memory and copies. Simple; one intermediate host buffer. *(What the sim implements.)*
- **(b) Fill a guest buffer** — a future `result_writer(i, count) -> &mut [u8]`; the host
  writes the canonical image *directly* into guest memory, no intermediate host `Vec`.

**Recommendation:** (a) is the shipped default; offer (b) as the zero-intermediate-copy path
— the natural sibling of caller-supplied result buffers ([component-model#369]), paying off
when the host computes results in place.

[component-model#369]: https://github.com/WebAssembly/component-model/issues/369

### Why the two sides are still not *perfectly* symmetric — and why that's right

The export side keeps a **prepare-time spec** (`ArgSpec` → validated `Tier`), because a host
loop reuses one `PreparedCall` thousands of times and wants shape validation hoisted out of
the hot path (§2). The import side chooses representation **per call, inside the handler**,
because there is no host-driven reuse loop — the *guest* drives, and the handler already runs
per call. So "provide/receive" is fully symmetric, but "validate once vs decide per call" is
deliberately not: it tracks who is looping. (A `PreparedImport` could hoist the import-result
validation too if a hot guest→host path ever wants it.)

---

## 5. Anticipating lazy value lowering

Lazy value lowering ([component-model#383], likely to become the default) changes **when**
argument bytes are materialised — pulled on demand *during* the guest call rather than
copied up front — not the **per-element cost model**. So it is *complementary* to the bulk
fast path, not a replacement: you still want a bulk representation so that when the bytes
are pulled, they move as one `memcpy` rather than an element walk.

What this means for the API, concretely:

- **The borrow structure is already lazy-ready.** With eager lowering the arg buffer *could*
  be released the instant `invoke` starts (bytes already copied). With lazy lowering the
  guest reads the source *during* execution, so the buffer must stay borrowed and unmodified
  for the **whole call**. `BoundCall` already holds the borrow across the entire `invoke` —
  so **the same API supports both**; only the `Tier` implementation swaps (`Memcpy` eager
  vs a `LazyMemcpy` puller). No signature changes.
- **The plan should choose per-arg.** Bulk-eager wins for hot, fully-consumed columns (an
  ECS system reads every element); lazy wins for args the guest *might not touch*. That
  per-arg choice is exactly what the `Tier` enum is for — add `LazyMemcpy` alongside
  `Memcpy`/`ValWalk` and pick it at `prepare` time.
- **`inout` is unaffected:** copy-back still happens after the guest returns but before the
  borrow is released, inside `invoke`.

[component-model#383]: https://github.com/WebAssembly/component-model/issues/383

---

## 6. Anticipating async

Async calls (`Store::run_concurrent`, `stream<T>`) push in the same direction as lazy, and
reinforce one design rule.

- **`invoke` becomes a future that carries the borrows.** `async fn invoke(self, …) ->
  Result<…>` returns `impl Future + 'a`; awaiting it holds the argument borrows across
  suspension — which is *required* once the guest can suspend mid-call. The prepared/bound
  split maps cleanly: `PreparedCall` stays reusable/`Send`, the future owns `'a`.
- **Scoped results extend to async scopes.** `invoke_scoped` with an async closure
  (`FnOnce(&Lifted) -> impl Future`) lets the borrowed view span `.await` points, with
  Wasmtime keeping guest memory valid and deferring post-return cleanup until the scope
  completes. A freely-returned guard (Q2 direction 2) would be *unsound* here — another
  guest call during the await could move or free the memory. This is the strongest argument
  for standardising on scoped results now.
- **Streams are just sources/sinks with a longer lifetime.** A `stream<T>` argument is a
  source whose borrow spans the *whole consumption*, not a single `memcpy`; it slots in as
  another `ArgSource`/`Tier`, and result streams mirror it on the lift side. (Tracked in
  [#12].) Again the borrow-for-the-whole-call structure already accommodates it.

[#12]: https://github.com/jeffparsons/bad-ideas/issues/12

**The design rule that falls out of both §5 and §6:**

> Never design a transfer surface that *requires* a borrow to end before the guest call
> completes.

Eager synchronous lowering *could* release an arg borrow early — but lazy lowering and async
both forbid it. So we standardise on "the bound call / import handle holds its borrows for
the whole call" today. It costs nothing in the current eager/sync world and is
forward-compatible with the world we expect.

---

## 7. Summary of recommendations

1. Treat **lower / lift** as the two operations and **`Source` / `Lifted`** as the two
   reused vocabularies; the four cells are surfaces over that one pair. *Flat* (bulk memcpy,
   incl. a single pre-lowered value) vs *`Val`* is a **per-slot representation choice**, so
   a call mixes bulk and dynamic freely; the bulk path is gated by the reflection check.
2. Keep the **prepared-shape / bound-call split** — it exactly matches the once-vs-per-call
   cost frequencies in §2 — but only on the export side, where the host loops; the import
   side chooses representation per call (§4).
3. Names: **`PreparedCall` → `.bind()` → `BoundCall` → `.invoke()`**, `CallShape` as the
   fallback if "prepared" stays ambiguous; add **`PreparedImport`** for import-side symmetry.
4. Results (Q2) and import args (Q3): **scoped borrowed view as the primitive, eager
   copy-out layered on top**. Avoid the returned-guard shape.
5. Import results (Q4): ship **hand-over-bytes**, add **fill-a-guest-buffer** for the
   zero-intermediate-copy path.
6. Design for **borrows held for the whole call** so the same surfaces survive lazy lowering
   and async unchanged; leave room in `Tier` for a `LazyMemcpy` strategy.
7. Give both sides **one primitive + optional sugar**: provide with `arg(ArgSource)` (name a
   source *value*), receive with `get::<T>()` (name a sink *type*), sugar over each. The
   value-vs-type split is the one forced asymmetry (homogeneous lower, heterogeneous lift).

---

## 8. Wasmtime API sketch

A concrete proposal against `wasmtime::component`, mapping the sim's surfaces to real types.
Everything fallible returns `Result` per **SAFE-4**: `prepare_call` validates structure once
(arity + the inline gate); builder methods are infallible; `invoke*` and `get` surface
data-dependent errors; only a genuine contract violation panics.

**Core surface at a glance** — the minimal set everything else is built from:

- Types: `ArgSpec`, `ArgSource`, `Source`, `PreparedCall`, `BoundCall`, `Results`,
  `ImportCall`, and the `FromResult` trait.
- Reflection: `Type::is_cabi_inline`.
- Provide: `Func::prepare_call` → `PreparedCall::bind` → `BoundCall::arg` →
  `BoundCall::invoke_scoped` (+ `invoke_scoped_async`).
- Receive: `Results::get::<T>` (+ `Results::len`, and the `FromResult` impls).
- Import: `Linker::func_new_transfer`, `ImportCall::args`, `ImportCall::set`.

Every other method below is a **convenience** that desugars to a core call (shown inline).
The two enums (`ArgSource`, `Source`) *are* core — they're the provide vocabulary, not sugar.

```rust
use wasmtime::component::{self, Val};

// ═══ CORE ═══════════════════════════════════════════════════════════════════

// Reflection gate.
impl component::Type {
    /// Fixed CABI layout, no out-of-line storage or ownership (no strings, lists,
    /// resources, handles). The gate that makes the bulk path legal. Named for the
    /// canonical ABI to disambiguate from unrelated senses of "inline".
    pub fn is_cabi_inline(&self) -> bool;
}

// Provide vocabulary (lower) — both enums are core.
pub enum ArgSpec { Val, Flat, FlatInOut }          // representation, fixed at prepare time
pub enum ArgSource<'a> {                           //   (FlatInOut is DEFERRED — see caveat)
    Val(Val),
    Flat(&'a [u8]),                                // pre-encoded CABI bytes: one value OR a list image
    FlatInOut(&'a mut [u8]),                       // read-write column, copied back after the call — DEFERRED
}
pub enum Source<'a> { Val(Val), Flat(&'a [u8]) }   // provide, import side (no in-out on results)

// Shape (reusable, borrow-free — validate once) and per-call binding.
impl component::Func {
    pub fn prepare_call(&self, store: impl AsContext, args: &[ArgSpec]) -> Result<PreparedCall>;
}
impl PreparedCall { pub fn bind(&self) -> BoundCall<'_>; }

impl<'a> BoundCall<'a> {
    pub fn arg(self, src: ArgSource<'a>) -> Self;                       // provide primitive
    pub fn invoke_scoped<R>(self, store: impl AsContextMut,             // the general invoke
        f: impl FnOnce(&Results<'_>) -> Result<R>) -> Result<R>;
    pub async fn invoke_scoped_async<R, Fut>(self, store: impl AsContextMut,
        f: impl FnOnce(&Results<'_>) -> Fut) -> Result<R>              // borrows held across await (FWD-2)
        where Fut: Future<Output = Result<R>>;
}

// Receive vocabulary (results and import args).
pub trait FromResult<'a>: Sized {                  // impls: &[u8] (view), Vec<u8> (copy), Val
    fn from_result(results: &'a Results<'_>, index: usize) -> Result<Self>;
}
impl<'a> Results<'a> {
    pub fn get<'r, T: FromResult<'r>>(&'r self, index: usize) -> Result<T>; // receive primitive
    pub fn len(&self) -> usize;
}

// Import side (guest → host): args lift, results lower.
impl<T> component::Linker<T> {
    pub fn func_new_transfer(&mut self, name: &str,
        f: impl Fn(StoreContextMut<T>, ImportCall<'_>) -> Result<()> + Send + Sync + 'static,
    ) -> Result<&mut Self>;
}
impl ImportCall<'_> {
    pub fn args(&self) -> Results<'_>;                                  // receive (lift)
    pub fn set(&mut self, index: usize, src: Source<'_>) -> Result<()>; // provide primitive (lower)
}

// ═══ CONVENIENCE — each desugars to the core call shown ══════════════════════

impl<'a> BoundCall<'a> {
    pub fn arg_val(self, v: Val) -> Self;                     // arg(ArgSource::Val(v))
    pub fn arg_flat(self, bytes: &'a [u8]) -> Self;           // arg(ArgSource::Flat(bytes))
    pub fn arg_flat_inout(self, bytes: &'a mut [u8]) -> Self; // arg(ArgSource::FlatInOut(bytes))

    pub fn invoke(self, store: impl AsContextMut) -> Result<()>;        // invoke_scoped(|_| Ok(()))
    pub fn invoke_collect(self, store: impl AsContextMut)              // scoped: copy every result
        -> Result<Vec<Vec<u8>>>;                                       //   = |r| (0..r.len()).map(get::<Vec<u8>>)
    pub async fn invoke_async(self, store: impl AsContextMut)          // invoke_scoped_async(|_| async { Ok(()) })
        -> Result<()>;
}

impl<'a> Results<'a> {
    pub fn view(&self, i: usize) -> Result<&[u8]>;   // get::<&[u8]>
    pub fn copy(&self, i: usize) -> Result<Vec<u8>>; // get::<Vec<u8>>
    pub fn val(&self, i: usize) -> Result<Val>;      // get::<Val>
}

impl ImportCall<'_> {
    pub fn set_val(&mut self, index: usize, v: Val) -> Result<()>;        // set(i, Source::Val(v))
    pub fn set_flat(&mut self, index: usize, bytes: &[u8]) -> Result<()>; // set(i, Source::Flat(bytes))
}
```

### Caveat — `FlatInOut` is deferred, and not in a first implementation

The Component Model has **no in-out parameters**: arguments are lowered *by value* into the
guest's memory, and after the call there is no ABI guarantee that region is still valid,
unmoved, or unfreed. So "mutate the argument in place and read it back" — exactly what
`FlatInOut` promises — is **not expressible with today's canonical ABI**. A first cut of this
API would therefore ship only `Val` and `Flat` arguments (plus the whole receive and import
surface); `FlatInOut` is **omitted until there is a way to use it**. It waits on one of:

- **caller-supplied buffers** ([component-model#369]) — a guest region the host hands in and
  reads back, the true zero-extra-allocation in-out (dovetails with the "fill-a-guest-buffer"
  direction in §4 Q4); or
- expressing the same intent *today* as a normal arg **plus a returned list**
  (`advance(cols) -> cols`), where the host copies the result back over its source buffer —
  sound now, but the guest interface must declare the result and the guest typically
  allocates a fresh result region (the alloc + copy `FlatInOut` exists to avoid).

The sim implements the aspirational (caller-supplied-buffer) flavor so the ergonomics can be
exercised end-to-end — so read the `arg_flat_inout` lines in the example below as the
intended *end-state*, not part of v1.

### Worked example

An ECS-style host driving a physics guest — hot-loop reuse, a zero-copy result read, and a
host import — all without the host statically knowing the guest's component types. It uses
the **convenience** methods (`arg_val`, `arg_flat_inout`, `arg_flat`, `set`) where they read
best; each desugars to the core surface above (the one exception, `r.get(0)`, is the core
receive primitive itself). Note `arg_flat_inout` is the deferred surface (caveat above).

```rust
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vec2 { x: f32, y: f32 }

// Guest world:
//   advance:   func(dt: f32, pos: list<vec2>, vel: list<vec2>);
//   summarise: func(pos: list<vec2>) -> list<f32>;
//   import gravity: func(pos: list<vec2>) -> list<vec2>;

let advance = instance.get_func(&mut store, "advance").unwrap();

// Prepare ONCE, outside the loop: dt as a Val, the two columns as read-write flat buffers.
let step = advance.prepare_call(&store, &[ArgSpec::Val, ArgSpec::FlatInOut, ArgSpec::FlatInOut])?;

let mut pos: Vec<Vec2> = initial_positions();
let mut vel: Vec<Vec2> = initial_velocities();

for _ in 0..STEPS {
    step.bind()
        .arg_val(Val::Float32(DT))
        .arg_flat_inout(bytemuck::cast_slice_mut(&mut pos))   // one memcpy in, one back
        .arg_flat_inout(bytemuck::cast_slice_mut(&mut vel))
        .invoke(&mut store)?;
    // pos/vel are unborrowed and freely mutable again here (SAFE-1).
}

// Read a result in place — no copy out of guest memory (name the sink type `&[u8]`):
let summarise = instance.get_func(&mut store, "summarise").unwrap();
let plan = summarise.prepare_call(&store, &[ArgSpec::Flat])?;
let total: f32 = plan.bind()
    .arg_flat(bytemuck::cast_slice(&pos))
    .invoke_scoped(&mut store, |r| {
        let mags: &[u8] = r.get(0)?;                          // zero-copy view (PERF-3)
        Ok(bytemuck::cast_slice::<u8, f32>(mags).iter().sum())
    })?;

// A host import the guest calls — lift its args, lower our result, mixing per slot:
linker.root().func_new_transfer("gravity", |_store, mut call| {
    let field: &[u8] = call.args().get(0)?;                   // lift guest bytes (borrowed)
    let out: Vec<Vec2> = gravity_of(bytemuck::cast_slice(field));
    call.set(0, Source::Flat(bytemuck::cast_slice(&out)))?;   // lower host bytes back
    Ok(())
})?;
```

The host only ever names *representations* (`ArgSpec`, `ArgSource`, sink types) and moves
raw canonical bytes; it is never generated against the guest's `vec2`/`list` types (CORE-1),
yet a hot column still crosses as a single `memcpy` (PERF-1) with its shape validated once
(PERF-2). The same `Results` accessor and `Source` enum serve results and imports, so all
four cells read identically.
