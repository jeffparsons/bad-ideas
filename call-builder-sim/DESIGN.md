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
3. **Owning invoke** — `invoke(store) -> OwnedResults`; copies the results out, hands back
   an accessor over host-owned bytes.
   - *Pros:* trivial; host owns the bytes; no closure; no lifetimes to thread.
   - *Cons:* always copies, even when the host only wanted to fold a checksum.

**Recommendation:** ship **(1) and (3) as peers** along the *who owns the bytes* axis —
`invoke` (owns, the easy default) and `invoke_scoped` (borrows, zero-copy) — both handing
back the same `get`/`view`/`copy`/`val` accessor. `invoke` is internally `invoke_scoped` +
copy-out, but it's a first-class method because the no-closure owning default is the common
case; naming it `invoke` (not `invoke_scoped`) also gives the `_scoped` suffix a visible
counterpart to earn its keep. Avoid **(2)** (a returned guard) as the foundation — it is the
one shape that fights both reuse and async.

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
4. Results (Q2) and import args (Q3): two peers on the *ownership* axis — **`invoke` (owns,
   default) and `invoke_scoped` (borrows, zero-copy)**, same accessor from both. Avoid the
   returned-guard shape.
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

- Types: `ArgSpec`, `ArgSource`, `Source`, `PreparedCall`, `BoundCall`, `Results` /
  `OwnedResults`, `ImportCall`, and the `FromResult` trait.
- Reflection: `Type::is_cabi_inline`.
- Provide: `Func::prepare_call` → `PreparedCall::bind` → `BoundCall::arg` →
  `BoundCall::invoke` (owns) or `invoke_scoped` (zero-copy) (+ async mirrors).
- Receive: `Results::get::<T>` (+ `Results::len`, and the `FromResult` impls).
- Import: `Linker::func_new_transfer`, `ImportCall::args`, `ImportCall::set`.

Every other method below is a **convenience** that desugars to a core call (shown inline).
The two enums (`ArgSource`, `Source`) *are* core — they're the provide vocabulary, not sugar —
and `ArgSource`/`ArgSpec` are **`#[non_exhaustive]`**: that's what lets §9's new sources
(`Stream`, a future `Buffer`) land additively, without breaking any caller's match or
construction. `Stream` is already implemented (§9.1); `FlatInOut` is deferred (caveat below).

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

// Provide vocabulary (lower) — both enums are core, and `#[non_exhaustive]` so new sources
// (streaming, typed buffers, …) can be added additively without breaking callers (see §9).
#[non_exhaustive]
pub enum ArgSpec { Val, Flat, FlatInOut, Stream }  // representation, fixed at prepare time
#[non_exhaustive]
pub enum ArgSource<'a> {
    Val(Val),
    Flat(&'a [u8]),                                // pre-encoded CABI bytes: one value OR a list image
    FlatInOut(&'a mut [u8]),                       // read-write column, copied back — DEFERRED (§8 caveat)
    Stream(&'a mut dyn Producer),                  // fed incrementally, pulled during the call (#12, §9.1)
    // …future: Buffer(&'a TypedBuffer, Range<usize>) — a checked slice of a typed buffer (#15, §9.2)
    // …future: Lowerable(&'a dyn Lower) — any statically-typed value that lowers itself, i.e. a
    //   `bindgen!`-generated type or a primitive. The mechanism for CORE-2 (§9.3).
}
pub enum Source<'a> { Val(Val), Flat(&'a [u8]) }   // provide, import side (no in-out on results)

// Shape (reusable, borrow-free — validate once) and per-call binding.
impl component::Func {
    pub fn prepare_call(&self, store: impl AsContext, args: &[ArgSpec]) -> Result<PreparedCall>;
}
impl PreparedCall { pub fn bind(&self) -> BoundCall<'_>; }

impl<'a> BoundCall<'a> {
    pub fn arg(self, src: ArgSource<'a>) -> Self;                       // provide primitive
    // Two peers on the "who owns the result bytes" axis:
    pub fn invoke(self, store: impl AsContextMut) -> Result<OwnedResults>;  // owns; no closure (default)
    pub fn invoke_scoped<R>(self, store: impl AsContextMut,                 // borrows; zero-copy
        f: impl FnOnce(&Results<'_>) -> Result<R>) -> Result<R>;
    // async mirrors — futures/borrows held across await (FWD-2):
    pub async fn invoke_async(self, store: impl AsContextMut) -> Result<OwnedResults>;
    pub async fn invoke_scoped_async<R, Fut>(self, store: impl AsContextMut,
        f: impl FnOnce(&Results<'_>) -> Fut) -> Result<R>
        where Fut: Future<Output = Result<R>>;
}

// Receive vocabulary. Both accessors share the same surface — `Results<'_>` (borrowed, from
// invoke_scoped) and `OwnedResults` (owns its copied bytes, from invoke). `invoke` is
// `invoke_scoped` + copy-out; it's core because the no-closure owning default is the common
// case, and naming the pair `invoke` / `invoke_scoped` gives `_scoped` a visible counterpart.
pub trait FromResult<'a>: Sized {                  // impls: &[u8] (view), Vec<u8> (copy), Val
    fn from_result(results: &'a Results<'_>, index: usize) -> Result<Self>;
}
impl<'a> Results<'a> {
    pub fn get<'r, T: FromResult<'r>>(&'r self, index: usize) -> Result<T>; // receive primitive
    pub fn len(&self) -> usize;
}
impl OwnedResults { /* len + the same get/view/copy/val over its owned bytes */ }

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
    pub fn arg_flat_inout(self, bytes: &'a mut [u8]) -> Self; // arg(ArgSource::FlatInOut(bytes)) — DEFERRED
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

---

## 9. Growing the API: two stress tests

The point here is not to finalise these features, but to check that the vocabulary of §8
*extends* to new use cases without reworking the core surfaces — that "the API can grow."
Both are framed on the **host → guest** flow. Neither needs a redesign; both turn out to be
*additive*, and one of them (streaming) actively *validates* a choice we already made.

### 9.1 A streaming argument source ([#12])

**Use case.** An argument fed *incrementally* — events, or a column produced on the fly —
pulled in chunks as the guest reads it, rather than materialised into one buffer up front.
This models `stream<T>` and is the most lazy-lowering-native source (inherently pull-based).

**Growth — one new `ArgSource` variant + a trait:**

```rust
pub trait Producer {
    /// Next chunk of pre-encoded inline elements, or None at end of stream.
    fn next_chunk(&mut self) -> Option<&[u8]>;
}
pub enum ArgSource<'a> {
    Val(Val),
    Flat(&'a [u8]),
    FlatInOut(&'a mut [u8]),          // deferred (§8 caveat)
    Stream(&'a mut dyn Producer),     // NEW: runtime pulls chunks during the call
}
// sugar, as ever: fn arg_stream(self, p: &'a mut dyn Producer) -> Self { self.arg(ArgSource::Stream(p)) }
```

**The borrow story — the key deliverable, and the punchline.** A `Flat` arg is borrowed for
the *whole* `invoke` already (we adopted "borrows held for the whole call" for lazy/async in
§5–6). A `Stream` is borrowed for *exactly the same extent* — the runtime just calls
`next_chunk()` several times during the call instead of doing one `memcpy`. So:

- **The Prepared/Bound split is unchanged.** `BoundCall<'a>` borrows the producer for `'a`
  (a `&'a mut`, like `FlatInOut`), and `invoke` consumes the builder, releasing it. Between
  calls the producer is unborrowed and can be refilled/advanced.
- **Composition stays sound.** `.arg_val(dt).arg_flat(&pos).arg_stream(&mut events)` — the
  three borrow independently; no aliasing.
- Streaming therefore **validates** the whole-call borrow rule rather than straining it: the
  rule was chosen *so that* lazy/async/stream sources all fit the same shape.

**Sync vs async.** A synchronous sketch pulls all chunks during `invoke` (isolating the
lifetime question). Real `stream<T>` pulls across await points → `invoke_async`, with the
producer borrow carried by the future (FWD-2) — again, no new borrow shape.

**Open question (from #12), answered.** A stream *consumed within one call* fits `ArgSource`
cleanly. A stream that **outlives** the call (a standing channel the guest reads across many
calls) is a different lifetime — a separate long-lived binding, closer to how `stream<T>`
readers already work — and should *not* be shoehorned into the per-call builder. The API can
host both; `ArgSource::Stream` is the within-call one.

**Implemented, and what compiling it pinned down.** The sim now ships this
(`fake_wasmtime::Producer`, `ArgSource::Stream`, `Caller::pull`, a mixed `Val + flat + stream`
demo matching a native reference, and a `compile_fail` proving the producer can't be touched
while the binding holds it). Writing real code surfaced one thing prose glossed over: the
runtime context (`Caller`) needs **two** lifetimes — one for the store/imports borrow, one for
the producer — because `&'p mut dyn Producer` is *invariant* in `'p` and cannot be unified
with the shorter store borrow. That's not a wart; it's the borrow story made concrete — a
streamed arg's borrow is a genuinely distinct thing the runtime holds across the whole call,
exactly as the design claimed. The per-call builder and the `compile_fail` are unchanged in
spirit from the flat case.

**Multiple streams.** Two lifetimes separate the two *kinds* of borrow (producers vs. store),
not the *number* of streams: every producer shares the one `'p`, living in a
`Vec<&'p mut dyn Producer>`, and the guest addresses them by handle (`caller.pull(0)`,
`caller.pull(1)`). N distinct producers borrowed for the same `'p` is sound because they
alias nothing, and the borrow checker even rejects feeding the *same* producer twice
(`.arg_stream(&mut p).arg_stream(&mut p)` — double `&mut`). The `demo_zip_streams` demo zips
two independent `stream<vec2>` args into a dot product, matching a native reference. The only
case wanting genuinely-independent lifetimes is a stream that *outlives* the call — the
separate long-lived binding above, not multiple per-call streams.

### 9.2 Typed buffers: a guest → host → guest conduit ([#15])

**Use case (your framing).** Collect many results from one component instance into a host
buffer — validating the CM type *once*, then just capacity/append checks — then slice it and
feed some or all of those values as arguments to *another* instance's call, without
re-lowering or re-validating at each hop. This is **CORE-8 (guest-to-guest conduit)** and
**PERF-2 (pay validation once)** realised at the level of *data* rather than *call shape*.

**Growth — one new type that plugs into both vocabularies.** Under the hood it is, as you
say, "just a `Vec` and some bookkeeping":

```rust
pub struct TypedBuffer {
    ty: component::Type,   // element type, checked cabi-inline once at construction
    bytes: Vec<u8>,        // CABI-encoded elements, back to back
    len: usize,            // element count
}
impl TypedBuffer {
    pub fn new(ty: component::Type) -> Result<Self>;          // validates ty is cabi-inline, ONCE
    pub fn len(&self) -> usize;
    pub fn capacity_elems(&self) -> usize;
}

// As a RESULT SINK — collect a call's result into it (append; type-id check + capacity, then memcpy):
impl<'a> Results<'a> { pub fn append_to(&self, index: usize, buf: &mut TypedBuffer) -> Result<()>; }

// As an ARG SOURCE — slice it back into another call (already CABI bytes of the right type):
pub enum ArgSource<'a> {
    // …Val / Flat / …
    Buffer(&'a TypedBuffer, Range<usize>),   // NEW: a checked slice — carries the type-id
}
// desugars near-identically to Flat(buf.bytes_for(range)), but the type-id lets prepare/bind
// "explode on mismatch" if the buffer's element type ≠ the target parameter's element type.
```

Read the flow end to end:

```rust
let mut buf = TypedBuffer::new(vec2_ty)?;               // one type check, ever
for chunk_call in producer_instance_calls {
    chunk_call.invoke_scoped(&mut store, |r| r.append_to(0, &mut buf))?;   // collect (memcpy in)
}
consumer.prepare_call(&store, &[ArgSpec::Flat])?       // feed a slice to another instance
    .bind()
    .arg(ArgSource::Buffer(&buf, 0..n))                // memcpy out — no re-encode, no re-validate
    .invoke(&mut store)?;
```

**Why it's the data-level dual of `PreparedCall`.** `PreparedCall` validates a *shape* once
and reuses it across many invocations; a `TypedBuffer` validates a *type* once and reuses the
lowered *bytes* across many collect/feed operations. That answers #15's "what value in
pre-lowering into this buffer?": you can also lower `Val`s / native values into a
`TypedBuffer` up front and replay it into N calls — amortising encoding, not just validation.

**The "checked" part.** The buffer carries its element `Type` (or a cheap unique type-id, or
an `Arc<PreparedCall>` for the shape-scoped variant #15 muses about). Every `append_to` and
every `ArgSource::Buffer` checks that id against the slot it's used with, so a buffer built
for `vec2` can never be silently fed to a call expecting something else — it explodes early
(SAFE-4: a structural mismatch, caught at bind).

### 9.3 A generic lowerable source: bindgen interop (CORE-2)

**Use case.** Pass a *statically-typed* Rust value — a `bindgen!`-generated struct, or a
primitive — directly as an argument, letting its generated `Lower` impl do the encoding, mixed
with `Val` / `Flat` / `Stream` in one call.

**Is it a valid source? Yes.** `Lower` *is* "a value that knows how to lower itself into the
canonical ABI," so a `Lowerable` source is just a fourth point on the spectrum of **how
pre-processed the value is**:

| source | typing | who encodes | cost |
| --- | --- | --- | --- |
| `Val` | dynamic (runtime) | runtime, per element | slowest, most flexible |
| **`Lowerable`** | **static (`T: Lower`)** | **generated (monomorphised) lowering** | **no pre-encoding; bindgen-fast** |
| `Flat` | static (you) | you, ahead of time | one `memcpy` — fastest |
| `Stream` | static, incremental | you, in chunks | pull-based |

So `Lowerable` is the concrete **mechanism for CORE-2** (bindgen interop) — previously a
hand-wave, now a variant. It's the bridge between the dynamic call-builder and bindgen's static
world: hand a real Rust value for one arg, a flat column for another, a `Val` for a third.

**Borrow story.** `&'a dyn Lower` is a shared borrow held for the call and lowered during
`invoke` — the same shape as `Flat`, so it fits the whole-call rule with nothing new.

**Caveat (the honest bit).** wasmtime's `Lower` is generic over the store type, so it is *not
object-safe* — `&dyn Lower` won't compile as written. The real form is a generic builder method
that erases the concrete lowering into a thunk:

```rust
// convenience, monomorphised per T; captures a type-erased lowering closure into the variant:
pub fn arg_lowerable<T: Lower>(self, value: &'a T) -> Self;
// ArgSource::Lowerable then carries e.g. Box<dyn FnOnce(&mut LowerContext) -> Result<()> + 'a>
```

The enum variant is the erased form; the `arg_lowerable<T>` method is where monomorphisation
happens. That's a small wrinkle, not a blocker — and it's exactly the kind of variant that
`#[non_exhaustive]` (§8) exists to let us add later without breaking callers.

### What the stress tests tell us

- All three are **purely additive** — a new `ArgSource` variant (+ trait) for #12, a new type
  straddling the provide/receive vocabularies for #15, a new variant (+ generic method) for
  §9.3. The four core surfaces are untouched, and because `ArgSource`/`ArgSpec` are
  `#[non_exhaustive]` (§8), adding the variants is non-breaking for existing callers.
- #12 **validates** the whole-call borrow rule (§5–6): a stream is just an arg whose lowering
  is spread across the call, and the rule already covers it.
- #15 **realises** CORE-8 and PERF-2 at the data level, and slots a `TypedBuffer` in as both
  a `Buffer` arg source and an `append_to` result sink — the same "one vocabulary, reused"
  story as the four cells.
- §9.3 **upgrades CORE-2** from an aspiration to a mechanism: a `Lowerable` source gives
  bindgen's `Lower` a first-class seat at the dynamic call, alongside `Val`/`Flat`/`Stream`.

**#12 is now implemented in the sim** (§9.1), which pinned the borrow story down concretely
and even surfaced a detail prose missed (the two-lifetime `Caller`). **#15 is still a
rough-out** — a good next compiles-and-runs follow-up to exercise the collect → slice → feed
round trip across two instances.

[#12]: https://github.com/jeffparsons/bad-ideas/issues/12
[#15]: https://github.com/jeffparsons/bad-ideas/issues/15
