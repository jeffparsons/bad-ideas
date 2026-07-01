//! Demo: run an ECS-style integration step through the fake component boundary using the
//! call-builder, reusing one `Prepared` plan across every step while the hot buffers are
//! rewritten in place — and check the result against a plain native computation.
//!
//! Mixed argument representations on one call: `dt` as a `Val` (convenient), and the two
//! big columns as flat read-write byte buffers (memcpy). This is the "best of more than
//! one world" the builder is meant to enable.

use call_builder_sim::call_builder::{ArgSpec, Prepared};
use call_builder_sim::fake_wasmtime::{CoreVal, Func, Store, Ty, Val};

const N: usize = 1_000; // entities
const STEPS: usize = 100;
const DT: f32 = 0.01;

fn vec2_ty() -> Ty {
    Ty::Record(vec![Ty::F32, Ty::F32])
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

/// Plain native reference: run the whole simulation without any of the machinery.
fn native(mut pos: Vec<u8>, mut vel: Vec<u8>) -> f64 {
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

fn main() {
    let mut positions = initial(0x1234_5678);
    let mut velocities = initial(0x9abc_def0);

    // Reference answer from an identical initial state.
    let native_sum = native(positions.clone(), velocities.clone());

    // The "guest": reads dt + the two columns from linear memory and mutates them in place.
    let func = Func::new(
        vec![Ty::F32, Ty::List(Box::new(vec2_ty())), Ty::List(Box::new(vec2_ty()))],
        |store: &mut Store, core: &[CoreVal]| {
            let dt = match core[0] {
                CoreVal::F32(x) => x,
                _ => unreachable!("param 0 is dt: f32"),
            };
            let as_ptr = |c: CoreVal| match c {
                CoreVal::I32(x) => x as usize,
                _ => unreachable!("expected pointer"),
            };
            let pos_ptr = as_ptr(core[1]);
            let count = as_ptr(core[2]);
            let vel_ptr = as_ptr(core[3]);
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
        },
    );

    // Prepare ONCE: dt as a Val, the two columns as read-write flat buffers.
    let prepared = Prepared::new(
        &func,
        &[ArgSpec::Val, ArgSpec::FlatInOut, ArgSpec::FlatInOut],
    )
    .unwrap();

    let mut store = Store::new(N * 8 * 2 + 4096);

    // Reuse the plan every step; the columns are borrowed only for each `invoke`, and are
    // rewritten in place (by the call) between iterations.
    for _ in 0..STEPS {
        prepared
            .call()
            .arg_val(Val::F32(DT))
            .arg_flat_inout(&mut positions)
            .arg_flat_inout(&mut velocities)
            .invoke(&mut store)
            .unwrap();
    }

    let sim_sum = checksum(&positions) + checksum(&velocities);

    println!("call-builder-sim demo");
    println!("  {N} entities x {STEPS} steps");
    print!("  arg tiers:  ");
    for (i, tier) in prepared.tiers().iter().enumerate() {
        print!("arg{i}={tier:?} ");
    }
    println!();
    println!("  native checksum: {native_sum}");
    println!("  sim checksum:    {sim_sum}");
    assert_eq!(
        native_sum, sim_sum,
        "fake-component result must match the native reference exactly"
    );
    println!("  OK: reused one Prepared plan across all steps; results match native.");
}
