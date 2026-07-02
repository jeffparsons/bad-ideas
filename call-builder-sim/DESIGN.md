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
Each slot independently picks how it is lowered or lifted, so a single call mixes flat and
dynamic in both directions (the `demo_mixed_pipeline` does exactly this). Two dual
vocabularies capture the choice:

- **provide** (lower) a value from a **`Source`**:
  - `Flat(&[u8])` — the caller already holds the canonical (CABI-encoded) bytes; copied in
    as one `memcpy`. This covers a whole `list<flat T>` **and a single pre-lowered
    value/record** — arity comes from the slot type, so it is not list-only.
  - `Val` — a dynamic value walked element-by-element (general/slow).
  - *(future)* `Lazy` — defer the copy to a pull *during* the call (§5); slots in here.
- **receive** (lift) a value from a **`Lifted`** accessor, choosing *at the pull site*:
  `view` (zero-copy borrow), `copy` (owned bytes), or `val` (dynamic).

Note the deliberate asymmetry between the two vocabularies: providing is an **enum bound up
front** (you must commit to how you supply the bytes), while receiving is an **accessor with
methods** (you decide how to take the value at the moment you take it — and may take the
same slot more than one way). That mirrors reality: you can't lower what you haven't
described, but you can lift an already-materialised value however you like.

The **flat** path is offered only where the type is provably flat: a fixed CABI layout with
no transitive strings, lists, resources, handles, or other pointer/ownership-bearing values.
(In the sim, `Ty::is_flat` is that gate; `Ty::String`/`Ty::Handle` exist purely to be
rejected by it.) Anything else falls back to `Val`, per slot — which is why "combine bulk
AND dynamic" is the default, not a special case.

So the four cells reduce to **one `Source`/`Lifted` pair reused everywhere**; the call
direction only decides which slots are provided and which are received. The call builder is
the export-side surface; `ImportCall` is its import-side mirror, speaking the same two
vocabularies.

---

## 2. Where the costs fall (the table #13 asked for)

The "nontrivial operations" involved in moving a `list<flat T>` of *M* elements across the
boundary, and **who pays them and how often**, under three schemes:

- **A — dynamic `Val`s:** `Func::call(&[Val])`, everything reflected at run time.
- **B — `bindgen!`:** static Rust types generated at host compile time.
- **C — prepared shape + bound flat buffers:** the proposal (this crate).

| Nontrivial operation | A: dynamic `Val`s | B: `bindgen!` | C: prepared + bound flat |
| --- | --- | --- | --- |
| **Shape validation** (arg matches the param interface type) | Wasmtime, **per element, per call** — walks the `Val` tree | compiler, **compile time** — types encode it | Wasmtime, **once at `prepare`** — validate shape + prove `T` flat; per call only a cheap len/stride check |
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
**lower** / **lift** (with "bulk" as the *flat representation* of each), so the docs can
point at one `Source`/`Lifted` pair for all four cells.

---

## 4. The four surfaces, with directions and recommendations

### Q1 — host → guest params (lower) — `PreparedCall` / `BoundCall`

The original prototype. Each arg picks its representation (`Val`, `flat_in`, `flat_inout`),
validated once; the bound call borrows the buffers for exactly one `invoke`, which consumes
it. This is settled; the borrow proof is the two `compile_fail` doctests. The only open
sub-direction is **arg supply style**: fluent builder (`.arg_flat_in(..).arg_val(..)`) vs
positional array (`.bind(&[Src::FlatIn(a), Src::Val(v)])`). Recommendation: keep the fluent
builder — it reads well and lets each `arg_*` method carry its own borrow, which is what
makes the per-arg lifetimes fall out correctly.

Since the merge of the `Source`/`Lifted` vocabularies (§1), Q1's provide side is literally
the same `Source` enum the import result side uses, and its `flat_in`/`val` sources go
through one `lower_source` choke point — so a single pre-lowered value/record flattens
by-value identically on both sides. `flat_inout` stays export-only, because "guest mutates a
host buffer in place" only makes sense for a param the host owns across the call.

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
both reuse and async. Because the receive vocabulary is an accessor, `view`/`copy`/`val` are
all available per result at the pull site; no separate result-spec is needed.

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
fast path, not a replacement: you still want a flat representation so that when the bytes
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
   a call mixes bulk and dynamic freely; the flat path is gated by the reflection check.
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
