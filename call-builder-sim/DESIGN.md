# Design notes: the canonical-ABI transfer matrix

Working notes for [issue #13]. The original call-builder prototype covered one quadrant
(host → guest **params**). This is an initial pass at the *whole* matrix, with the
naming, the cost table, and the forward-looking (lazy + async) parts #13 asked for.
The companion crate compiles and runs all four quadrants; this doc is the reasoning.

[issue #13]: https://github.com/jeffparsons/bad-ideas/issues/13

---

## 1. The reframe: one primitive, four quadrants

The call builder is one ergonomic surface over a more general operation. The underlying
primitive is a **checked bulk transfer of `list<flat T>`**, in two directions:

- **bulk lower** — host-owned `&[u8]` → canonical-ABI image in guest memory
- **bulk lift** — canonical-ABI image in guest memory → host `&[u8]` / borrowed view

`T` qualifies only if Wasmtime can prove, from reflected component type info, that it has
a **fixed canonical-ABI layout with no transitive strings, lists, resources, handles, or
other pointer/ownership-bearing values.** (In the sim, `Ty::is_flat` is that gate;
`Ty::String`/`Ty::Handle` exist purely to be rejected by it.)

Which primitive a slot uses is determined by **who owns the memory and who reads it**, and
that is *orthogonal to the direction of the call*:

| | params (in) | results (out) |
| --- | --- | --- |
| **host → guest** (export call) | bulk **lower** — host bytes into guest mem | bulk **lift** — guest mem into host view |
| **guest → host** (import call) | bulk **lift** — guest mem into host view | bulk **lower** — host bytes into guest mem |

Note the diagonal symmetry: the two "host provides the bytes" cells are both *lower*; the
two "host receives the bytes" cells are both *lift*. So there are really only two
operations, exposed on four surfaces. The call builder is the surface for the top-left
cell; the other three want sibling surfaces, not the same builder.

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
| result view holder | `Results<'scope>` | `ResultView`, `Lifted` |
| import shape (registered) | `PreparedImport` *(sim: `HostImport`)* | `ImportShape`, `HostFn` |
| import active handle | `ImportCall<'s>` | `ImportInvocation`, `Incoming` |

**Directions on the shape noun.** `PreparedCall` is familiar but a touch generic. If the
"this is only the *shape*" reading matters more than SQL familiarity, `CallShape` /
`CallSignature` say it outright and still pair cleanly (`CallShape` → `BoundCall`).
Recommendation: keep **`PreparedCall` + `.bind()` + `BoundCall`** as the default (verbs win
the most clarity for the least novelty), and treat `CallShape` as the fallback if
"prepared" keeps reading as ambiguous. The sim uses `HostImport` for the import shape as a
placeholder; `PreparedImport` would restore the symmetry if we want it.

Whatever the nouns, the honest primitive underneath should be named on both sides as
**bulk lower** / **bulk lift**, so the docs can point at one concept for all four cells.

---

## 4. The four surfaces, with directions and recommendations

### Q1 — host → guest params (bulk lower) — `PreparedCall` / `BoundCall`

The original prototype. Each arg picks its representation (`Val`, `flat_in`, `flat_inout`),
validated once; the bound call borrows the buffers for exactly one `invoke`, which consumes
it. This is settled; the borrow proof is the two `compile_fail` doctests. The only open
sub-direction is **arg supply style**: fluent builder (`.arg_flat_in(..).arg_val(..)`) vs
positional array (`.bind(&[Src::FlatIn(a), Src::Val(v)])`). Recommendation: keep the fluent
builder — it reads well and lets each `arg_*` method carry its own borrow, which is what
makes the per-arg lifetimes fall out correctly.

### Q2 — host → guest results (bulk lift)

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
(the sim ships both). Avoid **(2)** as the foundation — it is the one shape that fights both
reuse and async. A caller who genuinely wants a stored handle can `invoke_collect`.

### Q3 — guest → host params (bulk lift) — `ImportCall::arg_list_view`

Inside a host import handler, the guest's args are already lifetime-bounded by the callback.
This is the *most defensible* place to expose borrowed guest memory. Direction: borrowed
view (`arg_list_view(i) -> &[u8]`, zero copy, bounded by `&self`) vs eager copy
(`arg_list_copied(i) -> Vec<u8>`). Recommendation: **view as primitive, copy as sugar** —
same split as Q2. The sim's `arg_list_view` borrows `&self`, so it cannot be retained across
the result-setting call (which needs `&mut self`) or escape the handler.

### Q4 — guest → host results (bulk lower) — `ImportCall::return_flat_list`

The mirror of Q1's lowering, from inside the import handler. Two directions:

- **(a) Hand over host bytes** — `return_flat_list(i, &[u8])`; Wasmtime allocs in guest
  memory and copies. Simple; one intermediate host buffer.
- **(b) Fill a guest buffer** — `flat_list_writer(i, count) -> &mut [u8]`; the host writes
  the canonical image *directly* into guest memory, no intermediate host `Vec`.

**Recommendation:** ship **(a)** first (it's the obvious mirror and what the sim models),
and offer **(b)** as the zero-intermediate-copy path. (b) is the natural sibling of
caller-supplied result buffers ([component-model#369]) and pays off when the host computes
results in place.

[component-model#369]: https://github.com/WebAssembly/component-model/issues/369

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
  (`FnOnce(Results) -> impl Future`) lets the borrowed view span `.await` points, with
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

1. Treat **bulk lower / bulk lift of `list<flat T>`** as *the* primitive; the four
   quadrants are four surfaces over it, gated by the flat-type reflection check.
2. Keep the **prepared-shape / bound-call split** — it exactly matches the once-vs-per-call
   cost frequencies in §2.
3. Names: **`PreparedCall` → `.bind()` → `BoundCall` → `.invoke()`**, `CallShape` as the
   fallback if "prepared" stays ambiguous; add **`PreparedImport`** for import-side symmetry.
4. Results (Q2) and import args (Q3): **scoped borrowed view as the primitive, eager
   copy-out layered on top**. Avoid the returned-guard shape.
5. Import results (Q4): ship **hand-over-bytes**, add **fill-a-guest-buffer** for the
   zero-intermediate-copy path.
6. Design for **borrows held for the whole call** so the same surfaces survive lazy lowering
   and async unchanged; leave room in `Tier` for a `LazyMemcpy` strategy.
