# call-builder-sim

A tiny, **dependency-free, compiles-and-runs** sketch of a heterogeneous *call-builder*
API for **dynamic** WebAssembly Component Model calls. It models the design being
discussed in [`bytecodealliance/wasmtime#13788`](https://github.com/bytecodealliance/wasmtime/issues/13788)
("bulk lowering for `list<flat T>` in dynamic component calls").

It is **not** wasmtime. The point isn't performance — it's to answer one specific
question with real code that the compiler checks:

> If a reusable "call" lets you supply some arguments as `Val`s and others as flat
> byte buffers, and you reuse it across many invocations while mutating those buffers
> in between — **does that actually compose in Rust, or do the lifetimes make it
> impossible?**

Answer: it composes, and the *unsafe* version is a compile error.

## The idea

Dynamic component calls today are all-or-nothing: either fully dynamic
(`Func::call(&[Val])`, per-element boxing, slow) or fully static (`bindgen!`, fast but
requires compile-time types). A **call builder** lets each argument pick its own
representation, so you pay the fast path only where it's hot:

```rust
prepared
    .call()
    .arg_val(Val::F32(dt))            // scalar: a Val is cheap + convenient
    .arg_flat_inout(&mut positions)   // hot column: one memcpy in, one back
    .arg_flat_inout(&mut velocities)  // hot column: memcpy
    .invoke(&mut store)?;
```

## How the borrow story works (the whole point)

Two objects, and the split is the answer:

- **`Prepared`** — validated once (arity, per-arg tier). Holds **no** buffer borrows and
  has no lifetime parameter, so you keep it across the whole loop.
- **`CallBuilder<'a>`** — created fresh per call; borrows the argument buffers for `'a`
  only, and `invoke(self, …)` **consumes** it, releasing the borrows before the next
  statement. So between calls the buffers are unborrowed and freely mutable.

`Store` (the fake linear memory) is a distinct object from the argument buffers, so
`&mut store` and `&mut positions` borrow independently with no aliasing conflict.

The safety is enforced, not assumed: `src/lib.rs` contains a `compile_fail` doctest
showing that holding the builder across a mutation of the same buffer does not compile.

## Mapping to real wasmtime

| here | real wasmtime |
| --- | --- |
| `fake_wasmtime::Store` (`mem` + bump `realloc`) | a `Store` + the instance's linear memory + `cabi_realloc` |
| `fake_wasmtime::Func` (`params` + closure `body`) | a `component::Func` + the compiled guest |
| `Val` | `component::Val` |
| `lower_flat` (one memcpy) | the fast path #13788 asks for |
| `lower_val` (walk element-by-element) | today's `Func::call(&[Val])` |
| `ArgSpec` / `Tier` | the "checked-with-a-fast-path" decision, made once |

## Run it

```sh
cargo run     # ECS-style demo; reuses one Prepared plan across steps, matches a native reference
cargo test    # doctests, incl. the compile_fail proof that misuse doesn't build
```

## Scope

A lean spike: `Val` + flat in/inout sources, memcpy vs per-element lowering, and the
reuse/borrow proof. Deliberately omitted (natural extensions): a streaming/producer arg
source, a tier-2 validation sweep for `bool`/`enum`, heterogeneous result binding, and an
eager-vs-lazy (`value.lower`) dual path.
