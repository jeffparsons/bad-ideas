//! Demo: exercise all four quadrants of the canonical-ABI transfer matrix through the
//! fake component boundary, checking every result against a plain native computation.
//!
//! * **Quadrant 1** — host → guest *params* (bulk lower): an ECS integrate step, mixing a
//!   `Val` arg with two flat read-write columns, reusing one `PreparedCall` across steps.
//! * **Quadrant 2** — host → guest *results* (bulk lift): a guest that *returns* a list,
//!   read back both as a zero-copy scoped view and as an eager copy-out.
//! * **Quadrants 3 & 4** — guest → host *params* and *results* (bulk lift + bulk lower):
//!   a guest that calls a host import, which reads the guest's list as a borrowed view
//!   and writes its own list result back into guest memory.
//!
//! The final pipeline threads a single value through all four quadrants at once.

use call_builder_sim::call_builder::{ArgSpec, PreparedCall};
use call_builder_sim::fake_wasmtime::{Caller, CoreVal, Func, HostImport, Source, Store, Ty, Val};

const N: usize = 1_000; // entities
const STEPS: usize = 100;
const DT: f32 = 0.01;

fn vec2_ty() -> Ty {
    Ty::Record(vec![Ty::F32, Ty::F32])
}

fn list_of_vec2() -> Ty {
    Ty::List(Box::new(vec2_ty()))
}

/// One semi-implicit Euler step toward the origin (same shape as the benchmark scenario).
fn step(px: f32, py: f32, vx: f32, vy: f32, dt: f32) -> (f32, f32, f32, f32) {
    let len = (px * px + py * py).sqrt();
    let (ax, ay) = if len > 0.0 {
        (-px / len, -py / len)
    } else {
        (0.0, 0.0)
    };
    let nvx = vx + ax * dt;
    let nvy = vy + ay * dt;
    (px + nvx * dt, py + nvy * dt, nvx, nvy)
}

fn normalize(x: f32, y: f32) -> (f32, f32) {
    let len = (x * x + y * y).sqrt();
    if len > 0.0 {
        (x / len, y / len)
    } else {
        (0.0, 0.0)
    }
}

/// Deterministic initial column of `N` `vec2`s as raw little-endian bytes.
fn initial(mut s: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(N * 8);
    for _ in 0..N * 2 {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        let f = (s as f32 / u32::MAX as f32) * 2.0 - 1.0;
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

fn read_f32(bytes: &[u8], at: usize) -> f32 {
    f32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn write_f32(bytes: &mut [u8], at: usize, v: f32) {
    bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Sum of every f32 component, folded in f64 (matches the benchmark's checksum idea).
fn checksum(bytes: &[u8]) -> f64 {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64)
        .sum()
}

// --- Quadrant 1: host -> guest params (bulk lower) -----------------------------------

fn native_integrate(mut pos: Vec<u8>, mut vel: Vec<u8>) -> f64 {
    for _ in 0..STEPS {
        for i in 0..N {
            let (px, py) = (read_f32(&pos, i * 8), read_f32(&pos, i * 8 + 4));
            let (vx, vy) = (read_f32(&vel, i * 8), read_f32(&vel, i * 8 + 4));
            let (npx, npy, nvx, nvy) = step(px, py, vx, vy, DT);
            write_f32(&mut pos, i * 8, npx);
            write_f32(&mut pos, i * 8 + 4, npy);
            write_f32(&mut vel, i * 8, nvx);
            write_f32(&mut vel, i * 8 + 4, nvy);
        }
    }
    checksum(&pos) + checksum(&vel)
}

fn demo_params() {
    let mut positions = initial(0x1234_5678);
    let mut velocities = initial(0x9abc_def0);
    let native_sum = native_integrate(positions.clone(), velocities.clone());

    // Guest: reads dt + the two columns from linear memory and mutates them in place.
    let func = Func::new(
        vec![Ty::F32, list_of_vec2(), list_of_vec2()],
        vec![],
        |caller: &mut Caller, core: &[CoreVal]| {
            let dt = match core[0] {
                CoreVal::F32(x) => x,
                _ => unreachable!("param 0 is dt: f32"),
            };
            let pos_ptr = core[1].as_i32() as usize;
            let count = core[2].as_i32() as usize;
            let vel_ptr = core[3].as_i32() as usize;
            let store = caller.store();
            for i in 0..count {
                let (pb, vb) = (pos_ptr + i * 8, vel_ptr + i * 8);
                let (npx, npy, nvx, nvy) = step(
                    store.read_f32(pb),
                    store.read_f32(pb + 4),
                    store.read_f32(vb),
                    store.read_f32(vb + 4),
                    dt,
                );
                store.write_f32(pb, npx);
                store.write_f32(pb + 4, npy);
                store.write_f32(vb, nvx);
                store.write_f32(vb + 4, nvy);
            }
            vec![]
        },
    );

    // Prepare ONCE: dt as a Val, the two columns as read-write flat buffers.
    let prepared =
        PreparedCall::new(&func, &[ArgSpec::Val, ArgSpec::FlatInOut, ArgSpec::FlatInOut]).unwrap();
    let mut store = Store::new(N * 8 * 2 + 4096);

    for _ in 0..STEPS {
        prepared
            .bind()
            .arg_val(Val::F32(DT))
            .arg_flat_inout(&mut positions)
            .arg_flat_inout(&mut velocities)
            .invoke(&mut store)
            .unwrap();
    }

    let sim_sum = checksum(&positions) + checksum(&velocities);
    print!("  arg tiers: ");
    for (i, tier) in prepared.tiers().iter().enumerate() {
        print!("arg{i}={tier:?} ");
    }
    println!();
    assert_eq!(native_sum, sim_sum, "Q1 result must match native");
    println!("  [Q1] host->guest params  OK  (checksum {sim_sum})");
}

// --- Quadrant 2: host -> guest results (bulk lift) -----------------------------------

fn demo_results() {
    let positions = initial(0x0bad_1dea);

    // Guest: returns a NEW list<vec2> of normalized positions (does not touch its input).
    let func = Func::new(
        vec![list_of_vec2()],
        vec![list_of_vec2()],
        |caller: &mut Caller, core: &[CoreVal]| {
            let in_ptr = core[0].as_i32() as usize;
            let count = core[1].as_i32() as usize;
            let store = caller.store();
            let out_ptr = store.realloc(count * 8, 4);
            for i in 0..count {
                let (x, y) = (store.read_f32(in_ptr + i * 8), store.read_f32(in_ptr + i * 8 + 4));
                let (nx, ny) = normalize(x, y);
                store.write_f32(out_ptr + i * 8, nx);
                store.write_f32(out_ptr + i * 8 + 4, ny);
            }
            vec![CoreVal::I32(out_ptr as i32), CoreVal::I32(count as i32)]
        },
    );

    let mut native_out = vec![0u8; positions.len()];
    for i in 0..N {
        let (nx, ny) = normalize(read_f32(&positions, i * 8), read_f32(&positions, i * 8 + 4));
        write_f32(&mut native_out, i * 8, nx);
        write_f32(&mut native_out, i * 8 + 4, ny);
    }
    let native_sum = checksum(&native_out);

    let prepared = PreparedCall::new(&func, &[ArgSpec::FlatIn]).unwrap();
    let mut store = Store::new(N * 8 * 2 + 4096);

    // Zero-copy scoped view: read the result without copying it out of guest memory.
    let scoped_sum = prepared
        .bind()
        .arg_flat_in(&positions)
        .invoke_scoped(&mut store, |results| checksum(results.view(0)))
        .unwrap();

    // Same result, lifted as a dynamic `Val` instead (per-element) — proving the receive
    // side lets you pick the representation at the pull site.
    let val_sum = prepared
        .bind()
        .arg_flat_in(&positions)
        .invoke_scoped(&mut store, |results| match results.val(0) {
            Val::List(items) => items
                .iter()
                .map(|v| match v {
                    Val::Record(fields) => fields
                        .iter()
                        .map(|f| match f {
                            Val::F32(x) => *x as f64,
                            _ => unreachable!(),
                        })
                        .sum::<f64>(),
                    _ => unreachable!(),
                })
                .sum::<f64>(),
            _ => unreachable!(),
        })
        .unwrap();

    // Eager copy-out: keep the result bytes as host-owned storage.
    let collected = prepared
        .bind()
        .arg_flat_in(&positions)
        .invoke_collect(&mut store)
        .unwrap();
    let collected_sum = checksum(&collected[0]);

    assert_eq!(native_sum, scoped_sum, "Q2 scoped view must match native");
    assert_eq!(native_sum, val_sum, "Q2 val-lift must match native");
    assert_eq!(native_sum, collected_sum, "Q2 copy-out must match native");
    println!("  [Q2] host->guest results OK  (checksum {scoped_sum}; view == val == copy-out)");
}

// --- Single-element, pre-lowered vs dynamic (arity is not just lists) -----------------

fn demo_single_element() {
    // A guest taking one `vec2` *by value* (a record, two flattened f32 core params) and
    // returning its length as a one-element list<f32>.
    let func = Func::new(
        vec![vec2_ty()],
        vec![Ty::List(Box::new(Ty::F32))],
        |caller: &mut Caller, core: &[CoreVal]| {
            let (x, y) = match (core[0], core[1]) {
                (CoreVal::F32(x), CoreVal::F32(y)) => (x, y),
                _ => unreachable!("vec2 flattens to two f32 core params"),
            };
            let store = caller.store();
            let ptr = store.realloc(4, 4);
            store.write_f32(ptr, (x * x + y * y).sqrt());
            vec![CoreVal::I32(ptr as i32), CoreVal::I32(1)]
        },
    );

    let (px, py) = (3.0f32, 4.0f32);
    let native_len = (px * px + py * py).sqrt();

    // Provide the SAME vec2 two ways: as pre-lowered CABI bytes, and as a dynamic `Val`.
    let mut store = Store::new(4096);

    let flat_prepared = PreparedCall::new(&func, &[ArgSpec::FlatIn]).unwrap();
    let mut vec2_bytes = Vec::new();
    vec2_bytes.extend_from_slice(&px.to_le_bytes());
    vec2_bytes.extend_from_slice(&py.to_le_bytes());
    let flat_len = flat_prepared
        .bind()
        .arg_flat_in(&vec2_bytes)
        .invoke_scoped(&mut store, |r| read_f32(r.view(0), 0))
        .unwrap();

    let val_prepared = PreparedCall::new(&func, &[ArgSpec::Val]).unwrap();
    let val_len = val_prepared
        .bind()
        .arg_val(Val::Record(vec![Val::F32(px), Val::F32(py)]))
        .invoke_scoped(&mut store, |r| read_f32(r.view(0), 0))
        .unwrap();

    assert_eq!(native_len, flat_len, "single pre-lowered value must match native");
    assert_eq!(native_len, val_len, "single Val must match native");
    println!("  [1×] single value: pre-lowered bytes == Val  (|(3,4)| = {flat_len})");
}

// --- All four cells + every representation, in one pipeline --------------------------
//
// A single call that mixes bulk and Val on *both* the lowering and lifting sides, in both
// directions. `scale(pts: list<vec2>, factors: list<f32>) -> (scaled: list<vec2>,
// mags: list<f32>)`, where the guest forwards everything to a host import:
//
//   * host->guest params (lower):   pts as FLAT,  factors as VAL
//   * guest->host params  (lift):   pts as VIEW,  factors as VAL   (inside the import)
//   * guest->host results (lower):  scaled as FLAT, mags as VAL    (inside the import)
//   * host->guest results (lift):   scaled as VIEW, mags as VAL

fn demo_mixed_pipeline() {
    let pts = initial(0xfeed_face);
    let factors: Vec<f32> = (0..N).map(|i| 0.5 + (i % 4) as f32 * 0.25).collect();

    let scale_import = HostImport::new(
        vec![list_of_vec2(), Ty::List(Box::new(Ty::F32))],
        vec![list_of_vec2(), Ty::List(Box::new(Ty::F32))],
        |ic| {
            let mut scaled = Vec::with_capacity(N * 8);
            let mut mags = Vec::with_capacity(N);
            {
                let args = ic.args();
                let pts_bytes = args.view(0); // guest->host param, lifted as a VIEW
                let factor_vals = match args.val(1) {
                    // guest->host param, lifted as a VAL
                    Val::List(items) => items,
                    _ => unreachable!(),
                };
                for (i, chunk) in pts_bytes.chunks_exact(8).enumerate() {
                    let x = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    let y = f32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
                    let f = match factor_vals[i] {
                        Val::F32(f) => f,
                        _ => unreachable!(),
                    };
                    let (sx, sy) = (x * f, y * f);
                    scaled.extend_from_slice(&sx.to_le_bytes());
                    scaled.extend_from_slice(&sy.to_le_bytes());
                    mags.push(Val::F32((sx * sx + sy * sy).sqrt()));
                }
            } // arg views dropped here, before we take `&mut ic` to set results

            ic.set(0, Source::Flat(&scaled)); // guest->host result, lowered as FLAT
            ic.set(1, Source::Val(Val::List(mags))); // guest->host result, lowered as VAL
        },
    );

    let func = Func::with_imports(
        vec![list_of_vec2(), Ty::List(Box::new(Ty::F32))],
        vec![list_of_vec2(), Ty::List(Box::new(Ty::F32))],
        vec![scale_import],
        |caller: &mut Caller, core: &[CoreVal]| caller.call_import(0, core),
    );

    // Native reference.
    let (mut nat_scaled, mut nat_mags) = (vec![0u8; pts.len()], 0.0f64);
    for (i, &f) in factors.iter().enumerate() {
        let (x, y) = (read_f32(&pts, i * 8), read_f32(&pts, i * 8 + 4));
        let (sx, sy) = (x * f, y * f);
        write_f32(&mut nat_scaled, i * 8, sx);
        write_f32(&mut nat_scaled, i * 8 + 4, sy);
        nat_mags += (sx * sx + sy * sy).sqrt() as f64;
    }
    let native_sum = checksum(&nat_scaled) + nat_mags;

    let prepared = PreparedCall::new(&func, &[ArgSpec::FlatIn, ArgSpec::Val]).unwrap();
    let mut store = Store::new(N * 8 * 6 + 8192);

    // host->guest params: pts FLAT, factors VAL.
    let factors_val = Val::List(factors.iter().map(|&f| Val::F32(f)).collect());
    let pipeline_sum = prepared
        .bind()
        .arg_flat_in(&pts)
        .arg_val(factors_val)
        // host->guest results: scaled VIEW, mags VAL.
        .invoke_scoped(&mut store, |r| {
            let scaled_sum = checksum(r.view(0));
            let mags_sum = match r.val(1) {
                Val::List(items) => items
                    .iter()
                    .map(|v| match v {
                        Val::F32(x) => *x as f64,
                        _ => unreachable!(),
                    })
                    .sum::<f64>(),
                _ => unreachable!(),
            };
            scaled_sum + mags_sum
        })
        .unwrap();

    assert_eq!(native_sum, pipeline_sum, "mixed pipeline must match native");
    println!("  [mix] all 4 cells, flat+Val both ways OK  (checksum {pipeline_sum})");
}

fn main() {
    println!("call-builder-sim: canonical-ABI transfer matrix ({N} entities)");
    demo_params();
    demo_results();
    demo_single_element();
    demo_mixed_pipeline();
    println!("  OK: every cell matches its native reference, for both bulk and Val.");
}
