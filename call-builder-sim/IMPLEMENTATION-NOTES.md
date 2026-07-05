# Implementation notes — ValSource rewrite (issue #16)

Working notes for the rewrite of `call-builder-sim` from the old top-level `ArgSource`
enum to the recursive **`ValSource`** tree agreed on in issue #16. Kept as I go so the
final summary comment is faithful. (Untracked scratch — not committed unless asked.)

## Locked decisions (from Jeff, issue #16)
- **Q1** validate a reusable *shape* (`ValSpec`) once at `prepare`; reuse across calls.
- **Q2** arena is **host-native flattened** + on-demand overlay tables (not a faithful CABI image).
- **Q3** provide **both** owning + borrowing `ValidatedCabiBytes`.
- Property-B predicate is named **`are_all_bit_patterns_valid`**.
- **`Stream` stays a distinct node** (distinct CM type), not folded into a list backing.
- `trusted: bool` is gone — replaced by the **`ValidatedCabiBytes`** proof-wrapper; guest
  results come back already wrapped (runtime validated them on lift).
- A concrete `LowerOnDemand` impl owns the decision of *whether/when* to materialise/cache
  its CABI bytes; the runtime only ever calls `produce_range`.
- **Same API, both worlds:** sources are modelled as *random-access range producers*, so
  the runtime can drain them eagerly (today) OR hand them to guest-driven `value.lower`
  pulls (#383). Eager is the strict special case: "call every producer once, fully, up front."

## Plan / target surface
`ValSource<'a>`: `Val | Flat(ValidatedCabiBytes) | Record(Vec) | List(ListSource) | Stream | Lazy | Arena`
(`Resource` modelled-but-deferred, like the old sim deferred typed buffers — handle tables
are a big side-quest and not load-bearing for the three claims Jeff asked to prove).
`ListSource<'a>`: `Flat(ValidatedCabiBytes) | Elems(Vec<ValSource>)`.

Reused unchanged: the **receive** side (`Lifted`/`FromResult`/`OwnedResults`), the import
round-trip machinery, `Store`, `Val`, `CoreVal`.

Added to `Ty`: `Enum(u32)` — a 4-byte discriminant that is **inline but not**
`are_all_bit_patterns_valid` (valid iff `< cases`). Gives the validity sweep something real
to do and makes "validate the arena once" concrete.

## Claims the demos must prove
1. Degenerate all-`Val` tree == today's simple call (nothing regressed).
2. Per-node mixing *within one composite* (Flat + Val fields in a record / list elements).
3. Guest-obliviousness: same guest, `list` provided as `Flat` bytes vs `Elems([Val…])` →
   byte-identical guest memory.
4. **Both worlds:** ONE `ValSource` tree, driven eager vs lazy, byte-identical guest data;
   archetype AoS `Lazy` columns with `velocity` projected out (never touched in either
   world); lazy windowed pull gathers *only* the window (unneeded values free).
5. Arena: validate `Enum` discriminants **once** at fill time; replay slices with no
   re-validation (conduit A→B).

## Progress log
- [done] fake_wasmtime.rs: added `Ty::Enum(u32)`, `are_all_bit_patterns_valid`, per-image
  validation, `ValidatedCabiBytes` (borrow) + `ValidatedCabiBytesBuf` (own), `Val::Enum`,
  `Store::read_u32/write_u32`. Kept the receive side (`Lifted`/`FromResult`/`OwnedResults`)
  and the import round-trip untouched; import result side still uses the old `Source`
  vocab (bounded scope — noted as a follow-up).
- [done] call_builder.rs: `ValSource`/`ListSource` tree, `LowerOnDemand`, `Arena`/`ArenaSlice`
  (host-native, single level), `ValSpec` (validated once), `PreparedCall`/`BoundCall` with
  eager `invoke`/`invoke_scoped`, the shared `produce_list_range`/`produce_value_image`
  range-producer, and `drive_eager`/`drive_lazy` proof harness.
- [done] main.rs: 6 demos (simple / two-backings / composite / both-worlds / arena-validity
  / stream) — all match native, `cargo run` green.
- [done] lib.rs: rewrote module doc + 4 doctests (1 reuse pass + 3 `compile_fail` borrow
  proofs: buffer mutation, view escape, lazy-producer mutation). `cargo test` green.
- [done] `cargo clippy --all-targets` clean.

## Design decisions made while implementing (worth surfacing to Jeff)
- **Range-producer is the unification.** Every list-capable source implements one op,
  "produce elements [start,count)". Eager `invoke` calls it once `(0,len)`; `drive_lazy`
  calls it per pull. Same code path — the both-worlds claim isn't two implementations.
- **Host granularity ⟂ guest granularity, shown concretely.** `List(Elems([Val…]))` on an
  inline `list<u32>` produces byte-identical output to `List(Flat(bytes))` (demo 2).
- **Validity lives at the wrapper.** `ValidatedCabiBytes::checked` is O(1) for
  `are_all_bit_patterns_valid` types, a sweep for `Enum`; an out-of-range discriminant is
  rejected there, so it can't reach the memcpy path. Arena pays it once at `absorb`.
- **Scope cuts (deliberate, not blocking):** `Resource` node modelled-but-deferred (handle
  tables are a side-quest, not load-bearing for the three claims); arena is single-level
  (overlay tables only matter at depth, per Ex 3 — noted, not built); import/result side
  kept on the old `Source` vocab. All flagged for a later pass.

## Verification
`cargo run` (6 demos vs native), `cargo test` (4 doctests incl. 3 compile_fail),
`cargo clippy --all-targets` — all clean. Working tree left uncommitted for Jeff's review.
