# call-builder-sim

A tiny, **dependency-free, compiles-and-runs** sketch of the **canonical-ABI transfer
surfaces** for *dynamic* WebAssembly Component Model calls. It models the design being
discussed in [`bytecodealliance/wasmtime#13788`](https://github.com/bytecodealliance/wasmtime/issues/13788)
and worked through in [issue #13](https://github.com/jeffparsons/bad-ideas/issues/13).

It is **not** wasmtime. The point isn't performance — it's to answer, with real code the
compiler checks, whether the proposed API ergonomics *compose* across the full transfer
matrix (and whether the *unsafe* usages are compile errors).

## Design goals

A living checklist — the design, and its future evolutions, should keep satisfying every
item. These goals describe a host that moves component-model values into, out of, and
*between* guest components generically — including values whose types the guests share but
the host was never compiled against — always free to pick an efficient representation per
value. (*inline* is defined [below](#terminology-inline).)

Keywords **MUST** / **SHOULD** / **MAY** follow [RFC 2119]: *MUST* = required; *SHOULD* =
strongly wanted, pursue unless there's a compelling reason not to; *MAY* = desirable if it
fits, explicitly not blocking. Each goal has a stable ID (`AREA-n`) for citing elsewhere.

**Dynamic, mixed-representation transfers**

- **CORE-1 — Dynamic, no codegen.** The host MUST be able to receive and provide values
  without *statically* knowing their component-model types and without per-type code
  generation — driven entirely by reflected type information, so it can call guests whose
  types it never saw at build time.
- **CORE-2 — Bindgen interop.** Where the host *does* have `bindgen`-generated values, it
  MAY mix them into the same call alongside dynamically-supplied ones.
- **CORE-3 — Host → guest arguments.** The host MUST be able to provide each argument
  independently, mixing within one call: as a dynamic `Val`, or from a byte buffer holding a
  CABI-encoded *inline* value **or a contiguous array/list** of them.
- **CORE-4 — Host → guest return values.** The host MUST be able to receive each return
  value independently, choosing per value: a borrowed zero-copy view, an owned copy, or a
  dynamic `Val`.
- **CORE-5 — Guest → host arguments.** When a guest calls a host function, the host MUST be
  able to receive each argument with that same per-value choice (view / copy / `Val`).
- **CORE-6 — Guest → host return values.** The host MUST be able to provide each return
  value with that same per-value choice (`Val`, or a CABI-encoded inline value / array).
- **CORE-7 — Per-value choice.** Representation MUST be selectable per value; the convenient
  dynamic (`Val`) path MUST always be available, with faster representations opt-in only
  where wanted.
- **CORE-8 — Guest-to-guest conduit.** The host SHOULD be able to pass a value of a shared
  inline type from one guest to another efficiently — without statically knowing the type, and
  ideally by moving its CABI bytes rather than rebuilding a host-native value.

**Performance**

- **PERF-1 — Bulk fast path.** An inline value or list MUST be transferable as a single
  `memcpy`, not per-element work.
- **PERF-2 — Validation paid once.** For a call made repeatedly, type/shape validation
  SHOULD be hoisted out of the per-invocation path and reused across invocations.
- **PERF-3 — Zero-copy reads.** When the host only needs to read guest-produced inline data,
  it MAY do so in place, without copying it out.
- **PERF-4 — Allocation reuse.** The design SHOULD permit reusing guest allocations across
  calls, to bound `cabi_realloc` churn.

**Correctness**

- **CORR-1 — Checked, never blind.** A fast path MUST be taken only when the runtime can
  *prove* from reflected type info that the type is inline; anything else MUST fall back to
  `Val`.
- **CORR-2 — Representation-independent results.** The result MUST NOT depend on which
  representation was chosen for a value — flat and `Val` are interchangeable for the same
  logical value.

**Safety** *(table stakes — expected of any safe Rust API, listed for completeness)*

- **SAFE-1 — Borrow-safe reuse.** A prepared call MUST be reusable across invocations while
  its argument buffers are mutated between calls, enforced by the type system.
- **SAFE-2 — Bounded views.** Borrowed views into guest memory MUST NOT be able to escape
  the window in which the runtime guarantees them valid.
- **SAFE-3 — Misuse fails to compile.** Unsound usages (holding a borrow too long, escaping
  a view) MUST be compile errors, not runtime UB.
- **SAFE-4 — No UB on bad input.** Invalid input MUST NOT cause undefined behaviour or
  silent memory corruption. Failures then split by kind: **structural mismatches** (wrong
  arity, a flat-bytes spec on a non-inline type) MUST be caught as errors at *prepare/bind*
  time and validated once, so the hot path need not recheck them; **data-dependent
  failures** (a byte buffer whose length isn't a whole number of elements, or contents that
  fail validation) MUST be returned as recoverable errors at *invoke* time; only a contract
  violation these checks were meant to prevent MAY panic — a programmer error, like an
  out-of-bounds index.

**Forward-compatibility** *(anticipated; not necessarily implemented yet)*

- **FWD-1 — Lazy-lowering-ready.** The surfaces and borrow structure SHOULD work unchanged
  if lazy value lowering becomes the norm.
- **FWD-2 — Async-ready.** The design SHOULD extend to async calls — borrows held across
  `await` — without reshaping the surfaces.
- **FWD-3 — Streaming.** A `stream<T>` argument or result MAY slot in as another
  representation (see [#12]).
- **FWD-4 — Caller-supplied buffers.** The host MAY be able to write results directly into
  guest memory, or reuse allocations, with no intermediate copy.

### Non-goals

- **Beating `bindgen!` on raw throughput** for types the host statically knows — where
  generated code applies, it is expected to win; this is about the *dynamic* case.
- **A fast path for non-inline types** — strings, nested lists, resources, and handles
  always go through `Val`; only inline types get bulk transfer.
- **A new wire format** — the bytes are exactly the canonical-ABI image, nothing bespoke.
- **Replacing `Val` or `bindgen`** — this composes with them; it does not supersede them.

### Terminology: "inline"

A component-model type is **inline** when its whole value is stored *inline* — a fixed
canonical-ABI layout with no *out-of-line* storage or ownership: numbers and records/tuples
of them, but not strings, nested lists, resources, or handles. "inline" pairs naturally with
the canonical ABI's own "out-of-line" and, unlike *flat*, doesn't collide with ABI
*flattening* (lowering a value to core params). Prior art for the concept: Java Valhalla
inline/value classes, .NET *blittable*, Swift's `BitwiseCopyable`, C++ *trivially-copyable*.

Kept distinct on purpose is the nearby phrase **flat bytes** — a *flat (contiguous) byte
buffer* holding a value's pre-encoded canonical image (the `Source::Flat` variant). An
*inline type* is precisely what makes handing its value over as *flat bytes* legal: the two
words name the type and the buffer, and are used together deliberately.

[RFC 2119]: https://www.rfc-editor.org/rfc/rfc2119

## The matrix

Each slot is either **lowered** (a value the host *provides* → guest memory) or **lifted**
(a value the host *receives* ← guest memory). Which one it is depends on who owns the
memory, *not* on the direction of the call:

| | params (in) | results (out) |
| --- | --- | --- |
| **host → guest** (export call) | host **provides** → *lower* | host **receives** → *lift* |
| **guest → host** (import call) | host **receives** → *lift* | host **provides** → *lower* |

Crucially, each slot **independently picks its representation** — nothing forces a call to
be all-bulk or all-dynamic; one call mixes them freely, in both directions:

- **provide** (lower) from a `Source`: `Flat(&[u8])` — a pre-encoded CABI image (flat bytes)
  copied as one `memcpy`, covering a whole `list<inline T>` **or a single value/record** —
  or `Val`, walked element-by-element. (A future `Lazy` slots in here unchanged.)
- **receive** (lift) from a `Lifted` accessor, choosing *at the pull site*: `view`
  (zero-copy borrow), `copy` (owned bytes), or `val` (dynamic).

The bulk fast path is offered only where the type is provably *inline* (fixed CABI layout,
no strings, nested lists, resources, or handles); everything else falls back to `Val`. So
"bulk" is one *representation*, not the definition of a transfer.

The two vocabularies are reused across all four cells:

| | provide (lower) | receive (lift) |
| --- | --- | --- |
| **host → guest** (export) | `BoundCall::arg_flat_in` / `arg_val` / `arg_flat_inout` | `invoke(…)` (owns) / `invoke_scoped(\|r\| …)` (zero-copy) |
| **guest → host** (import) | `ImportCall::set(i, Source)` | `ImportCall::args()` → `Lifted` |

`src/main.rs` exercises every cell against a native reference; the final pipeline mixes
bulk and dynamic representations on *both* the lower and lift sides, in *both* directions,
in a single call.

The full reasoning — the cost table, naming options, and how this survives **lazy value
lowering** and **async** unchanged — is in [`DESIGN.md`](./DESIGN.md).

## The borrow story (the original question)

Two objects, and the split is the answer to "how do you reuse a call while mutating its
argument buffers between invocations?":

- **`PreparedCall`** — the *shape*: validated once (arity, per-arg tier). Holds **no**
  buffer borrows and no lifetime parameter, so you keep it across the whole loop.
- **`BoundCall<'a>`** — the *binding*: created fresh per call via `.bind()`; borrows the
  argument buffers for `'a` only, and `invoke*` **consumes** it, releasing them before the
  next statement. So between calls the buffers are unborrowed and freely mutable.

Borrowed *result* views (host→guest results, guest→host params) are confined to a
Wasmtime-controlled scope and cannot escape it. All of this is enforced, not assumed:
`src/lib.rs` has three doctests, two of them `compile_fail` — one for holding a binding
across a buffer mutation, one for letting a scoped result view escape its closure.

## Mapping to real wasmtime

| here | real wasmtime |
| --- | --- |
| `fake_wasmtime::Store` (`mem` + bump `realloc`) | a `Store` + the instance's linear memory + `cabi_realloc` |
| `fake_wasmtime::Func` (`params`/`results`/`imports` + closure `body`) | a `component::Func` + the compiled guest |
| `fake_wasmtime::HostImport` / `ImportCall` | a `Linker` import + its `Caller`-scoped invocation |
| `Source` (`Flat`/`Val`) provide · `Lifted::get::<T>` receive (`view`/`copy`/`val` sugar) | the two reused vocabularies, in all four cells |
| `Producer` / `ArgSource::Stream` / `Caller::pull` | a `stream<T>` argument, pulled in chunks during the call (#12) |
| `Val` / `lower_val` (walk element-by-element) | `component::Val` / today's `Func::call(&[Val])` |
| `lower_flat` / `lift_flat_view` (one memcpy) | the bulk fast paths #13788 asks for |
| `ArgSpec` / `Tier` | the export-side "checked-with-a-fast-path" decision, made once |

For a concrete proposal of these as real wasmtime types — `Func::prepare_call`, `BoundCall`,
`Results::get::<T>`, `Linker::func_new_transfer` — plus a worked ECS example, see
[`DESIGN.md` §8 (Wasmtime API sketch)](./DESIGN.md).

## Run it

```sh
cargo run     # all four quadrants + the streaming demo; each matches a native reference
cargo test    # doctests, incl. the three compile_fail borrow-safety proofs
```

## Scope

The whole matrix, with `Flat` + `Val` mixable on both the provide and receive sides in
both directions, single pre-lowered values as well as `list<inline T>`, in/inout export
args, owning + scoped + `val` result reading, a guest↔host import round trip, and a
**streaming argument source** ([#12] — `Producer` / `ArgSource::Stream` / `Caller::pull`,
pulled chunk by chunk) — plus the reuse/borrow proofs. Deliberately *modelled but not
implemented* (see `DESIGN.md` §9.2 and the notes elsewhere): typed buffers ([#15]), a lazy
(`value.lower`) lowering tier, async `invoke`, and a tier-2 validation sweep for `bool`/`enum`.

[#12]: https://github.com/jeffparsons/bad-ideas/issues/12
[#15]: https://github.com/jeffparsons/bad-ideas/issues/15
