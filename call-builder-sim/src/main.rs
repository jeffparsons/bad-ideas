//! Demos for the **`ValSource`** provide surface (issue #16). Each proves one claim from
//! the design discussion against a plain native reference:
//!
//! 1. `demo_simple` — the degenerate all-`Val` tree still works (nothing regressed).
//! 2. `demo_two_backings` — same guest, `list` as flat bytes vs `Elems([Val…])` →
//!    **byte-identical** guest data (guest-obliviousness).
//! 3. `demo_composite` — per-node mixing *within one composite* (Flat + Val fields).
//! 4. `demo_both_worlds` — ONE `ValSource` tree driven **eager vs lazy**, byte-identical;
//!    archetype AoS `Lazy` columns with `velocity` projected out (never touched); a lazy
//!    window pulls *only* the window.
//! 5. `demo_arena_validity` — `ValidatedCabiBytes` (free for total types, sweep for `Enum`,
//!    rejects bad discriminants) + an arena validated **once**.

use std::cell::Cell;

use call_builder_sim::call_builder::{
    drive_eager, drive_lazy, Arena, ListSource, LowerOnDemand, PreparedCall, ValSpec, ValSource,
};
use call_builder_sim::fake_wasmtime::{
    Caller, CoreVal, Func, Producer, Store, Ty, Val, ValidatedCabiBytes,
};

const N: usize = 1_000;

fn vec2_ty() -> Ty {
    Ty::Record(vec![Ty::F32, Ty::F32])
}
fn list_of_vec2() -> Ty {
    Ty::List(Box::new(vec2_ty()))
}
fn list_of_f32() -> Ty {
    Ty::List(Box::new(Ty::F32))
}

fn read_f32(bytes: &[u8], at: usize) -> f32 {
    f32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}
/// Deterministic little-endian bytes of `n` f32s from a tiny xorshift PRNG.
fn rng_f32s(mut s: u32, n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n * 4);
    for _ in 0..n {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        let f = (s as f32 / u32::MAX as f32) * 2.0 - 1.0;
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

// =======================================================================================
// 1. The degenerate all-`Val` tree is the simple case.
// =======================================================================================

/// A guest taking `list<vec2>` and returning a one-element `list<f32>`: Σ of all components.
fn sum_components_func() -> std::rc::Rc<Func> {
    Func::new(
        vec![list_of_vec2()],
        vec![list_of_f32()],
        |caller: &mut Caller<'_, '_>, core: &[CoreVal]| {
            let ptr = core[0].as_i32() as usize;
            let count = core[1].as_i32() as usize;
            let s = caller.store();
            let mut sum = 0.0f32;
            for i in 0..count {
                sum += s.read_f32(ptr + i * 8) + s.read_f32(ptr + i * 8 + 4);
            }
            let out = s.realloc(4, 4);
            s.write_f32(out, sum);
            vec![CoreVal::I32(out as i32), CoreVal::I32(1)]
        },
    )
}

fn demo_simple() {
    let raw = rng_f32s(0x1234_5678, N * 2); // N vec2s
    // Reference in the guest's exact f32 order (per vec2: x + y, accumulated).
    let mut native = 0.0f32;
    for c in raw.chunks_exact(8) {
        native += read_f32(c, 0) + read_f32(c, 4);
    }

    let func = sum_components_func();
    let prepared = PreparedCall::new(&func, &[ValSpec::ListElems(Box::new(ValSpec::Val))]).unwrap();
    let mut store = Store::new(N * 8 * 2 + 4096);

    // Build the list purely out of dynamic Vals — the "simple API", now a ValSource tree.
    let elems: Vec<ValSource> = raw
        .chunks_exact(8)
        .map(|c| {
            ValSource::Val(Val::Record(vec![
                Val::F32(read_f32(c, 0)),
                Val::F32(read_f32(c, 4)),
            ]))
        })
        .collect();
    let sum = prepared
        .bind()
        .arg(ValSource::List(ListSource::Elems(elems)))
        .invoke_scoped(&mut store, |r| read_f32(r.view(0), 0))
        .unwrap();

    assert_eq!(native, sum, "all-Val tree must match native");
    println!("  [1 simple]     all-`Val` tree OK          (Σ {sum:.3})");
}

// =======================================================================================
// 2. Guest-obliviousness: flat bytes vs Elems([Val…]) → byte-identical.
// =======================================================================================

fn demo_two_backings() {
    let data: Vec<u32> = (0..N as u32).map(|i| i.wrapping_mul(2654435761)).collect();
    let raw: Vec<u8> = data.iter().flat_map(|n| n.to_le_bytes()).collect();
    let native: u32 = data.iter().copied().fold(0, u32::wrapping_add);
    let u32_ty = Ty::U32;

    // Prove byte-identity at the driver: the two backings produce the same bytes.
    let vb = ValidatedCabiBytes::checked(&raw, &u32_ty).unwrap();
    let mut flat = ValSource::List(ListSource::Flat(vb));
    let elems: Vec<ValSource> = data.iter().map(|&n| ValSource::Val(Val::U32(n))).collect();
    let mut per_elem = ValSource::List(ListSource::Elems(elems));
    assert_eq!(
        drive_eager(&mut flat, &Ty::U32),
        drive_eager(&mut per_elem, &Ty::U32),
        "flat vs per-element must produce byte-identical images"
    );

    // And prove it through a real guest call, both ways.
    let func = Func::new(
        vec![Ty::List(Box::new(Ty::U32))],
        vec![Ty::List(Box::new(Ty::U32))],
        |caller: &mut Caller<'_, '_>, core: &[CoreVal]| {
            let ptr = core[0].as_i32() as usize;
            let count = core[1].as_i32() as usize;
            let s = caller.store();
            let mut sum = 0u32;
            for i in 0..count {
                sum = sum.wrapping_add(s.read_u32(ptr + i * 4));
            }
            let out = s.realloc(4, 4);
            s.write_u32(out, sum);
            vec![CoreVal::I32(out as i32), CoreVal::I32(1)]
        },
    );
    let mut store = Store::new(N * 4 * 2 + 4096);

    let p_flat = PreparedCall::new(&func, &[ValSpec::ListFlat]).unwrap();
    let vb = ValidatedCabiBytes::checked(&raw, &u32_ty).unwrap();
    let sum_flat = p_flat
        .bind()
        .arg_list_flat(vb)
        .invoke_scoped(&mut store, |r| {
            u32::from_le_bytes(r.view(0)[0..4].try_into().unwrap())
        })
        .unwrap();

    let p_elems = PreparedCall::new(&func, &[ValSpec::ListElems(Box::new(ValSpec::Val))]).unwrap();
    let elems: Vec<ValSource> = data.iter().map(|&n| ValSource::Val(Val::U32(n))).collect();
    let sum_elems = p_elems
        .bind()
        .arg(ValSource::List(ListSource::Elems(elems)))
        .invoke_scoped(&mut store, |r| {
            u32::from_le_bytes(r.view(0)[0..4].try_into().unwrap())
        })
        .unwrap();

    assert_eq!(native, sum_flat, "flat backing must match native");
    assert_eq!(native, sum_elems, "elems backing must match native");
    println!("  [2 backings]   flat == Elems([Val…]) OK    (guest can't tell)");
}

// =======================================================================================
// 3. Per-node mixing within one composite: Record{ Flat, Flat, Val<Enum> }.
// =======================================================================================

fn particle_ty() -> Ty {
    // record { pos: vec2, vel: vec2, kind: enum(3) }  — all inline, 20 bytes.
    Ty::Record(vec![vec2_ty(), vec2_ty(), Ty::Enum(3)])
}

fn demo_composite() {
    let pos = rng_f32s(0xaaaa_1111, N * 2);
    let vel = rng_f32s(0xbbbb_2222, N * 2);
    let kinds: Vec<u32> = (0..N).map(|i| (i % 3) as u32).collect();
    let vt = vec2_ty();

    // Native: assemble each particle's canonical image and checksum the whole run.
    let mut native_bytes = Vec::with_capacity(N * 20);
    for i in 0..N {
        native_bytes.extend_from_slice(&pos[i * 8..i * 8 + 8]);
        native_bytes.extend_from_slice(&vel[i * 8..i * 8 + 8]);
        native_bytes.extend_from_slice(&kinds[i].to_le_bytes());
    }

    // Build the list per-element, each particle a Record mixing Flat (pos, vel) + Val (kind).
    let elems: Vec<ValSource> = (0..N)
        .map(|i| {
            let pos_vb = ValidatedCabiBytes::checked(&pos[i * 8..i * 8 + 8], &vt).unwrap();
            let vel_vb = ValidatedCabiBytes::checked(&vel[i * 8..i * 8 + 8], &vt).unwrap();
            ValSource::Record(vec![
                ValSource::Flat(pos_vb),
                ValSource::Flat(vel_vb),
                ValSource::Val(Val::Enum(kinds[i])),
            ])
        })
        .collect();
    let mut mixed = ValSource::List(ListSource::Elems(elems));
    let produced = drive_eager(&mut mixed, &particle_ty());

    assert_eq!(
        native_bytes, produced,
        "per-field mixed composite must assemble the exact canonical image"
    );

    // A guest reads the mixed list and returns Σ(pos+vel components) + Σ(kind) as one f32.
    let func = Func::new(
        vec![Ty::List(Box::new(particle_ty()))],
        vec![list_of_f32()],
        |caller: &mut Caller<'_, '_>, core: &[CoreVal]| {
            let ptr = core[0].as_i32() as usize;
            let count = core[1].as_i32() as usize;
            let s = caller.store();
            let mut acc = 0.0f32;
            for i in 0..count {
                let base = ptr + i * 20;
                for k in 0..4 {
                    acc += s.read_f32(base + k * 4); // pos.x pos.y vel.x vel.y
                }
                acc += s.read_u32(base + 16) as f32; // kind discriminant
            }
            let out = s.realloc(4, 4);
            s.write_f32(out, acc);
            vec![CoreVal::I32(out as i32), CoreVal::I32(1)]
        },
    );
    // Reference in the guest's exact f32 order (per particle: pos.x, pos.y, vel.x, vel.y, kind).
    let mut native_acc = 0.0f32;
    for (i, &kind) in kinds.iter().enumerate() {
        native_acc += read_f32(&pos, i * 8);
        native_acc += read_f32(&pos, i * 8 + 4);
        native_acc += read_f32(&vel, i * 8);
        native_acc += read_f32(&vel, i * 8 + 4);
        native_acc += kind as f32;
    }
    let prepared = PreparedCall::new(
        &func,
        &[ValSpec::ListElems(Box::new(ValSpec::Record(vec![
            ValSpec::Flat, // pos
            ValSpec::Flat, // vel
            ValSpec::Val,  // kind
        ])))],
    )
    .unwrap();
    let mut store = Store::new(N * 20 * 2 + 4096);
    let elems: Vec<ValSource> = (0..N)
        .map(|i| {
            let pos_vb = ValidatedCabiBytes::checked(&pos[i * 8..i * 8 + 8], &vt).unwrap();
            let vel_vb = ValidatedCabiBytes::checked(&vel[i * 8..i * 8 + 8], &vt).unwrap();
            ValSource::Record(vec![
                ValSource::Flat(pos_vb),
                ValSource::Flat(vel_vb),
                ValSource::Val(Val::Enum(kinds[i])),
            ])
        })
        .collect();
    let guest_acc = prepared
        .bind()
        .arg(ValSource::List(ListSource::Elems(elems)))
        .invoke_scoped(&mut store, |r| read_f32(r.view(0), 0))
        .unwrap();

    assert_eq!(
        native_acc, guest_acc,
        "composite guest read must match native"
    );
    println!("  [3 composite]  Record{{Flat,Flat,Val<Enum>}} OK  (mixed within one value)");
}

// =======================================================================================
// 4. Same API, both worlds — archetype projection via a Lazy source.
// =======================================================================================

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Position = 0,
    Velocity = 1,
    Acceleration = 2,
}

/// Host-side AoS storage: `count` entities, each `pos|vel|accel` (three vec2s, 24 bytes).
/// Tracks which fields were ever read, to *prove* projection.
struct Archetype {
    bytes: Vec<u8>,
    count: usize,
    touched: [Cell<bool>; 3],
}

impl Archetype {
    fn build(count: usize) -> Self {
        let mut bytes = Vec::with_capacity(count * 24);
        let (p, v, a) = (
            rng_f32s(0x1111_0000, count * 2),
            rng_f32s(0x2222_0000, count * 2),
            rng_f32s(0x3333_0000, count * 2),
        );
        for i in 0..count {
            bytes.extend_from_slice(&p[i * 8..i * 8 + 8]);
            bytes.extend_from_slice(&v[i * 8..i * 8 + 8]);
            bytes.extend_from_slice(&a[i * 8..i * 8 + 8]);
        }
        Archetype {
            bytes,
            count,
            touched: std::array::from_fn(|_| Cell::new(false)),
        }
    }
    fn read_vec2(&self, i: usize, field: Field) -> (f32, f32) {
        self.touched[field as usize].set(true);
        let off = i * 24 + (field as usize) * 8;
        (read_f32(&self.bytes, off), read_f32(&self.bytes, off + 4))
    }
    fn touched(&self, field: Field) -> bool {
        self.touched[field as usize].get()
    }
    fn reset_touched(&self) {
        for c in &self.touched {
            c.set(false);
        }
    }
}

/// A `LowerOnDemand` that gathers one column out of the AoS archetype on demand. It chooses
/// **not** to cache — it re-reads the archetype each pull (a valid policy; the API leaves
/// caching to the impl). `produced` counts elements it was actually asked for.
struct ArchetypeColumn<'a> {
    arch: &'a Archetype,
    field: Field,
    produced: usize,
}

impl<'a> ArchetypeColumn<'a> {
    fn new(arch: &'a Archetype, field: Field) -> Self {
        ArchetypeColumn {
            arch,
            field,
            produced: 0,
        }
    }
}

impl LowerOnDemand for ArchetypeColumn<'_> {
    fn len(&self) -> usize {
        self.arch.count
    }
    fn produce_range(&mut self, start: usize, count: usize, out: &mut Vec<u8>) {
        for i in start..start + count {
            let (x, y) = self.arch.read_vec2(i, self.field);
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            self.produced += 1;
        }
    }
}

fn demo_both_worlds() {
    let arch = Archetype::build(N);
    let vec2 = vec2_ty();

    // --- Same ValSource tree, driven eager vs lazy → byte-identical ---
    arch.reset_touched();
    let mut col_e = ArchetypeColumn::new(&arch, Field::Position);
    let eager = drive_eager(&mut ValSource::Lazy(&mut col_e), &vec2);

    arch.reset_touched();
    let mut col_l = ArchetypeColumn::new(&arch, Field::Position);
    // The "guest" pulls the same column in three uneven chunks — its own schedule.
    let lazy = drive_lazy(
        &mut ValSource::Lazy(&mut col_l),
        &vec2,
        &[(0, 10), (10, N - 30), (N - 20, 20)],
    );
    assert_eq!(eager, lazy, "same tree, eager vs lazy → byte-identical");

    // --- Projection: velocity is never a source, so it is never touched, in either world ---
    assert!(!arch.touched(Field::Velocity), "velocity projected away — never read");

    // --- Ground it in a real guest call: the archetype projection you described. The guest
    // wants (positions, accelerations); velocity is stored but never sourced. Two Lazy
    // columns gather from the AoS storage on demand. ---
    let func = Func::new(
        vec![list_of_vec2(), list_of_vec2()],
        vec![list_of_f32()],
        |caller: &mut Caller<'_, '_>, core: &[CoreVal]| {
            let (pp, pc) = (core[0].as_i32() as usize, core[1].as_i32() as usize);
            let (ap, ac) = (core[2].as_i32() as usize, core[3].as_i32() as usize);
            let s = caller.store();
            let mut sum = 0.0f32;
            for i in 0..pc {
                sum += s.read_f32(pp + i * 8) + s.read_f32(pp + i * 8 + 4);
            }
            for i in 0..ac {
                sum += s.read_f32(ap + i * 8) + s.read_f32(ap + i * 8 + 4);
            }
            let out = s.realloc(4, 4);
            s.write_f32(out, sum);
            vec![CoreVal::I32(out as i32), CoreVal::I32(1)]
        },
    );
    let prepared = PreparedCall::new(&func, &[ValSpec::Lazy, ValSpec::Lazy]).unwrap();
    let mut store = Store::new(N * 8 * 4 + 4096);
    arch.reset_touched();
    let mut pos_g = ArchetypeColumn::new(&arch, Field::Position);
    let mut acc_g = ArchetypeColumn::new(&arch, Field::Acceleration);
    let guest_sum = prepared
        .bind()
        .arg_lazy(&mut pos_g)
        .arg_lazy(&mut acc_g)
        .invoke_scoped(&mut store, |r| read_f32(r.view(0), 0))
        .unwrap();

    // Native reference in the guest's exact f32 order (pos column, then accel column).
    let mut native_pa = 0.0f32;
    for i in 0..N {
        native_pa += read_f32(&arch.bytes, i * 24) + read_f32(&arch.bytes, i * 24 + 4);
    }
    for i in 0..N {
        native_pa += read_f32(&arch.bytes, i * 24 + 16) + read_f32(&arch.bytes, i * 24 + 20);
    }
    assert_eq!(guest_sum, native_pa, "pos+accel projection must match native");
    assert!(
        !arch.touched(Field::Velocity),
        "velocity projected away: never read by either lazy column"
    );

    // --- Lazy-only: a windowed pull gathers ONLY the window (unneeded values are free) ---
    arch.reset_touched();
    let mut col_w = ArchetypeColumn::new(&arch, Field::Position);
    {
        let mut src = ValSource::Lazy(&mut col_w);
        let window = drive_lazy(&mut src, &vec2, &[(100, 64)]);
        assert_eq!(window.len(), 64 * 8, "window is exactly 64 vec2s");
    }
    let produced_count = col_w.produced;
    assert_eq!(produced_count, 64, "only the 64-element window was ever produced");

    println!(
        "  [4 both-worlds] eager==lazy bytes; velocity projected (untouched); \
         window pulled {produced_count}/{N} only"
    );
}

// =======================================================================================
// 5. ValidatedCabiBytes + arena validated once.
// =======================================================================================

fn demo_arena_validity() {
    // Minting is free for a total type (U32): no sweep, always Ok for well-sized bytes.
    let u32s: Vec<u8> = (0..8u32).flat_map(|n| n.to_le_bytes()).collect();
    assert!(ValidatedCabiBytes::checked(&u32s, &Ty::U32).is_ok());

    // Enum(4) is NOT are_all_bit_patterns_valid, so checked() runs the sweep and REJECTS a
    // discriminant >= 4.
    assert!(!Ty::Enum(4).are_all_bit_patterns_valid());
    let good: Vec<u8> = [0u32, 3, 1, 2].iter().flat_map(|n| n.to_le_bytes()).collect();
    let bad: Vec<u8> = [0u32, 4, 1, 2].iter().flat_map(|n| n.to_le_bytes()).collect();
    assert!(ValidatedCabiBytes::checked(&good, &Ty::Enum(4)).is_ok());
    assert!(
        ValidatedCabiBytes::checked(&bad, &Ty::Enum(4)).is_err(),
        "out-of-range discriminant must be rejected at the wrapper"
    );

    // Arena: validate a list<enum> ONCE at absorb time; replay slices with no re-check.
    let arena = Arena::absorb(good.clone(), Ty::Enum(4)).expect("valid run");
    assert!(
        Arena::absorb(bad, Ty::Enum(4)).is_err(),
        "arena rejects an invalid run at fill time"
    );

    // A guest sums the discriminants of a replayed arena slice (the conduit's second hop).
    let func = Func::new(
        vec![Ty::List(Box::new(Ty::Enum(4)))],
        vec![Ty::List(Box::new(Ty::U32))],
        |caller: &mut Caller<'_, '_>, core: &[CoreVal]| {
            let ptr = core[0].as_i32() as usize;
            let count = core[1].as_i32() as usize;
            let s = caller.store();
            let mut sum = 0u32;
            for i in 0..count {
                sum = sum.wrapping_add(s.read_u32(ptr + i * 4));
            }
            let out = s.realloc(4, 4);
            s.write_u32(out, sum);
            vec![CoreVal::I32(out as i32), CoreVal::I32(1)]
        },
    );
    let prepared = PreparedCall::new(&func, &[ValSpec::Arena]).unwrap();
    let mut store = Store::new(4096);
    let sum = prepared
        .bind()
        .arg_arena(arena.slice(0, arena.elem_count()))
        .invoke_scoped(&mut store, |r| {
            u32::from_le_bytes(r.view(0)[0..4].try_into().unwrap())
        })
        .unwrap();
    assert_eq!(sum, 3 + 1 + 2, "replayed arena slice must reach the guest intact");
    println!("  [5 arena]      ValidatedCabiBytes + validate-once OK  (Σ discriminants {sum})");
}

// =======================================================================================
// 6. Stream — a distinct CM-type node (kept separate per issue #16), mixed with Val + Flat.
// =======================================================================================

/// Hands a byte buffer out in fixed-size chunks — an argument delivered incrementally.
struct ChunkProducer<'d> {
    data: &'d [u8],
    chunk_bytes: usize,
    pos: usize,
}

impl Producer for ChunkProducer<'_> {
    fn next_chunk(&mut self) -> Option<&[u8]> {
        if self.pos >= self.data.len() {
            return None;
        }
        let end = (self.pos + self.chunk_bytes).min(self.data.len());
        let chunk = &self.data[self.pos..end];
        self.pos = end;
        Some(chunk)
    }
}

fn demo_stream() {
    // accumulate(scale: f32 [Val], seed: list<vec2> [Flat], events: stream<vec2> [Stream])
    //   -> list<f32> ;  total = Σ seed components + scale · Σ event components.
    let func = Func::new(
        vec![Ty::F32, list_of_vec2(), list_of_vec2()],
        vec![list_of_f32()],
        |caller: &mut Caller<'_, '_>, core: &[CoreVal]| {
            let scale = match core[0] {
                CoreVal::F32(x) => x,
                _ => unreachable!("param 0 is scale: f32"),
            };
            let (seed_ptr, seed_count) = (core[1].as_i32() as usize, core[2].as_i32() as usize);
            let stream = core[3].as_i32() as usize;

            let mut total = 0.0f32;
            {
                let s = caller.store();
                for i in 0..seed_count {
                    total += s.read_f32(seed_ptr + i * 8) + s.read_f32(seed_ptr + i * 8 + 4);
                }
            }
            // Pull the streamed events incrementally — one bounded memcpy per chunk.
            while let Some((ptr, byte_len)) = caller.pull(stream) {
                let s = caller.store();
                let mut off = ptr;
                while off + 8 <= ptr + byte_len {
                    total += (s.read_f32(off) + s.read_f32(off + 4)) * scale;
                    off += 8;
                }
            }
            let s = caller.store();
            let out = s.realloc(4, 4);
            s.write_f32(out, total);
            vec![CoreVal::I32(out as i32), CoreVal::I32(1)]
        },
    );

    let seed = rng_f32s(0x5eed_1234, N * 2);
    let events = rng_f32s(0xe0e0_e0e0, N * 2);
    let scale = 0.5f32;

    // Native reference, same operation order as the guest.
    let mut native = 0.0f32;
    for c in seed.chunks_exact(8) {
        native += read_f32(c, 0) + read_f32(c, 4);
    }
    for c in events.chunks_exact(8) {
        native += (read_f32(c, 0) + read_f32(c, 4)) * scale;
    }

    let prepared =
        PreparedCall::new(&func, &[ValSpec::Val, ValSpec::ListFlat, ValSpec::Stream]).unwrap();
    let mut store = Store::new(N * 8 * 3 + 8192);
    let vec2 = vec2_ty();
    let seed_vb = ValidatedCabiBytes::checked(&seed, &vec2).unwrap();
    let mut producer = ChunkProducer {
        data: &events,
        chunk_bytes: 64 * 8,
        pos: 0,
    };
    let total = prepared
        .bind()
        .arg_val(Val::F32(scale)) // Val
        .arg_list_flat(seed_vb) // Flat
        .arg(ValSource::Stream(&mut producer)) // Stream — pulled chunk by chunk during the call
        .invoke_scoped(&mut store, |r| read_f32(r.view(0), 0))
        .unwrap();

    assert_eq!(native, total, "Val + Flat + Stream mix must match native");
    println!("  [6 stream]     Val + Flat + Stream mixed OK  (total {total:.3})");
}

fn main() {
    println!("call-builder-sim: ValSource provide surface ({N} entities)");
    demo_simple();
    demo_two_backings();
    demo_composite();
    demo_both_worlds();
    demo_arena_validity();
    demo_stream();
    println!("  OK: ValSource tree, guest-obliviousness, both worlds, validate-once, and stream all check out.");
}
