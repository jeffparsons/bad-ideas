# call-builder-sim

A tiny, **dependency-free, compiles-and-runs** sketch of the **canonical-ABI transfer
surfaces** for *dynamic* WebAssembly Component Model calls. It models the design being
discussed in [`bytecodealliance/wasmtime#13788`](https://github.com/bytecodealliance/wasmtime/issues/13788)
and worked through in [issue #13](https://github.com/jeffparsons/bad-ideas/issues/13).

It is **not** wasmtime. The point isn't performance — it's to answer, with real code the
compiler checks, whether the proposed API ergonomics *compose* across the full transfer
matrix (and whether the *unsafe* usages are compile errors).

## The 2×2 matrix

Every transfer is a **bulk lower** (host bytes → guest memory) or a **bulk lift** (guest
memory → host bytes/view) of a `list<flat T>` — where `T` is accepted only if Wasmtime can
prove from reflected type info that it is a fixed-layout POD with no strings, nested lists,
resources, or handles. Which primitive a slot uses depends on who owns the memory, *not* on
the direction of the call:

| | params (in) | results (out) |
| --- | --- | --- |
| **host → guest** (export call) | bulk **lower** — `PreparedCall` / `BoundCall` | bulk **lift** — `invoke_scoped` / `invoke_collect` |
| **guest → host** (import call) | bulk **lift** — `ImportCall::arg_list_view` | bulk **lower** — `ImportCall::return_flat_list` |

`src/main.rs` exercises all four quadrants against a native reference; the final pipeline
threads one value through every quadrant in a single call.

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
| `Val` / `lower_val` (walk element-by-element) | `component::Val` / today's `Func::call(&[Val])` |
| `lower_flat` (one memcpy) / `lift_flat[_view]` | the bulk fast paths #13788 asks for |
| `ArgSpec` / `Tier` | the "checked-with-a-fast-path" decision, made once |

## Run it

```sh
cargo run     # all four quadrants; each matches a native reference
cargo test    # doctests, incl. the two compile_fail borrow-safety proofs
```

## Scope

An initial pass at the whole matrix: `Val` + flat in/inout arg sources, scoped + eager
result reading, and a guest↔host import round trip — plus the reuse/borrow proofs.
Deliberately *modelled but not implemented* (see `DESIGN.md`): a lazy (`value.lower`)
lowering tier, async `invoke`, a streaming arg/result source ([#12]), and a tier-2
validation sweep for `bool`/`enum`. These are noted where they would slot in.

[#12]: https://github.com/jeffparsons/bad-ideas/issues/12
