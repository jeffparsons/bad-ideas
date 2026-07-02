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
item. ("*flat*" is defined in the footnote.[^flat])

**Dynamic, mixed-representation transfers**

- **No static type knowledge, no required codegen.** The host receives and provides values
  without *statically* knowing the specific component-model types involved, and without
  per-type code generation — so it can drive guests whose types it never saw at build time;
  everything is driven by reflected type information at run time. Where the host *does* have
  `bindgen`-generated values, it may mix those into the same call alongside dynamic ones.
- **Host → guest, arguments.** Provide each argument independently, mixing freely within one
  call: as a dynamic `Val`, or from a byte buffer holding a CABI-encoded *flat* value **or a
  contiguous array/list** of them.
- **Host → guest, return values.** Receive each return value independently, choosing per
  value: a borrowed zero-copy view, an owned copy, or a dynamic `Val`.
- **Guest → host, arguments.** When a guest calls a host function, the host receives each
  argument with that same per-value choice (view / copy / `Val`).
- **Guest → host, return values.** The host provides each return value with that same
  per-value choice (`Val`, or a CABI-encoded flat value / array).
- **The choice is per value.** The convenient dynamic (`Val`) path is always available; the
  faster representations are opt-in, only where wanted.

**Performance**

- **Bulk fast path.** A flat value or list moves as a single `memcpy`, not per-element work.
- **Validation paid once.** For a call made repeatedly, type/shape validation is hoisted out
  of the per-invocation path and reused across invocations.
- **Zero-copy reads** *(if we can — not strictly required).* When the host only needs to
  read guest-produced flat data, it can do so in place, without copying it out.

**Correctness**

- **Checked, never blind.** A fast path is taken only when the runtime can *prove*, from
  reflected type info, that the type is flat; anything else falls back to `Val`.
- **Representation-independent results.** The representation chosen for a value never changes
  the result — flat and `Val` are interchangeable for the same logical value.

**Safety** *(table stakes — expected of any safe Rust API, listed for completeness)*

- **Borrow-safe reuse.** A prepared call is reusable across invocations while its argument
  buffers are mutated between calls, enforced by the type system.
- **Views can't outlive their validity.** Borrowed views into guest memory cannot escape the
  window in which the runtime guarantees them valid.
- **Misuse fails to compile.** The unsound usages are compile errors, not runtime UB.

**Forward-compatibility** *(anticipated; not necessarily implemented yet)*

- **Lazy-lowering-ready.** The surfaces and borrow structure work unchanged if lazy value
  lowering becomes the norm.
- **Async-ready.** The design extends to async calls — borrows held across `await` — without
  reshaping the surfaces.
- **Room for streaming.** A `stream<T>` argument or result can slot in as another
  representation (see [#12]).
- **Room for caller-supplied buffers** *(if it fits — not blocking).* The host can write
  results directly into guest memory, or reuse allocations, with no intermediate copy.

[^flat]: *flat* here means a component-model type with a fixed canonical-ABI layout and no
    out-of-line storage or ownership — numbers and records/tuples of them, but not strings,
    nested lists, resources, or handles. (The word is provisional: "flat" already describes
    ABI *flattening* to core params, so we may prefer *plain*, *POD*, or *fixed-layout*.)

## The matrix

Each slot is either **lowered** (a value the host *provides* → guest memory) or **lifted**
(a value the host *receives* ← guest memory). Which one it is depends on who owns the
memory, *not* on the direction of the call:

| | params (in) | results (out) |
| --- | --- | --- |
| **host → guest** (export call) | host **provides** → *lower* | host **receives** → *lift* |
| **guest → host** (import call) | host **receives** → *lift* | host **provides** → *lower* |

Crucially, each slot **independently picks its representation** — nothing forces a call to
be all-flat or all-dynamic; one call mixes them freely, in both directions:

- **provide** (lower) from a `Source`: `Flat(&[u8])` — a pre-encoded CABI image copied as
  one `memcpy`, covering a whole `list<flat T>` **or a single value/record** — or `Val`,
  walked element-by-element. (A future `Lazy` slots in here without changing anything else.)
- **receive** (lift) from a `Lifted` accessor, choosing *at the pull site*: `view`
  (zero-copy borrow), `copy` (owned bytes), or `val` (dynamic).

The flat fast path is offered only where the type is provably flat (fixed CABI layout, no
strings, nested lists, resources, or handles); everything else falls back to `Val`. So
"bulk" is one *representation*, not the definition of a transfer.

The two vocabularies are reused across all four cells:

| | provide (lower) | receive (lift) |
| --- | --- | --- |
| **host → guest** (export) | `BoundCall::arg_flat_in` / `arg_val` / `arg_flat_inout` | `invoke_scoped(\|r\| …)` / `invoke_collect` |
| **guest → host** (import) | `ImportCall::set(i, Source)` | `ImportCall::args()` → `Lifted` |

`src/main.rs` exercises every cell against a native reference; the final pipeline mixes
flat and `Val` on *both* the lower and lift sides, in *both* directions, in a single call.

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
| `Source` (`Flat`/`Val`) / `Lifted` (`view`/`copy`/`val`) | the provide / receive vocabularies, reused in all four cells |
| `Val` / `lower_val` (walk element-by-element) | `component::Val` / today's `Func::call(&[Val])` |
| `lower_flat` / `lift_flat_view` (one memcpy) | the bulk fast paths #13788 asks for |
| `ArgSpec` / `Tier` | the export-side "checked-with-a-fast-path" decision, made once |

## Run it

```sh
cargo run     # all four quadrants; each matches a native reference
cargo test    # doctests, incl. the two compile_fail borrow-safety proofs
```

## Scope

The whole matrix, with `Flat` + `Val` mixable on both the provide and receive sides in
both directions, single pre-lowered values as well as `list<flat T>`, in/inout export
args, scoped + eager + `val` result reading, and a guest↔host import round trip — plus the
reuse/borrow proofs. Deliberately *modelled but not implemented* (see `DESIGN.md`): a lazy
(`value.lower`) lowering tier, async `invoke`, a streaming arg/result source ([#12]), and a
tier-2 validation sweep for `bool`/`enum`. These are noted where they would slot in.

[#12]: https://github.com/jeffparsons/bad-ideas/issues/12
