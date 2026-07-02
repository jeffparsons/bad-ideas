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
use call_builder_sim::fake_wasmtime::{Caller, CoreVal, Func, HostImport, Store, Ty, Val};

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
        .invoke_scoped(&mut store, |results| checksum(results.flat_list_view(0)))
        .unwrap();

    // Eager copy-out: keep the result bytes as host-owned storage.
    let collected = prepared
        .bind()
        .arg_flat_in(&positions)
        .invoke_collect(&mut store)
        .unwrap();
    let collected_sum = checksum(&collected[0]);

    assert_eq!(native_sum, scoped_sum, "Q2 scoped view must match native");
    assert_eq!(native_sum, collected_sum, "Q2 copy-out must match native");
    println!("  [Q2] host->guest results OK  (checksum {scoped_sum}; scoped view == copy-out)");
}

// --- Quadrants 3 & 4: guest -> host params + results (bulk lift + bulk lower) ---------

fn demo_import_pipeline() {
    let positions = initial(0xfeed_face);

    // Host import: reads the guest's list as a borrowed view (Q3, lift), computes a new
    // list, and writes it back into guest memory (Q4, lower).
    let normalize_import = HostImport::new(
        vec![list_of_vec2()],
        vec![list_of_vec2()],
        |ic| {
            // Q3: borrow the guest-owned argument bytes in place — no copy.
            let input = ic.arg_list_view(0);
            let mut out = Vec::with_capacity(input.len());
            for chunk in input.chunks_exact(8) {
                let x = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                let y = f32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
                let (nx, ny) = normalize(x, y);
                out.extend_from_slice(&nx.to_le_bytes());
                out.extend_from_slice(&ny.to_le_bytes());
            }
            // `input`'s borrow ends here; now we may take `&mut ic` to write the result.
            // Q4: hand host-owned bytes back as the flat list result.
            ic.return_flat_list(0, &out);
        },
    );

    // Guest: forwards its input list to the host import and returns the host's result.
    let func = Func::with_imports(
        vec![list_of_vec2()],
        vec![list_of_vec2()],
        vec![normalize_import],
        |caller: &mut Caller, core: &[CoreVal]| caller.call_import(0, core),
    );

    let mut native_out = vec![0u8; positions.len()];
    for i in 0..N {
        let (nx, ny) = normalize(read_f32(&positions, i * 8), read_f32(&positions, i * 8 + 4));
        write_f32(&mut native_out, i * 8, nx);
        write_f32(&mut native_out, i * 8 + 4, ny);
    }
    let native_sum = checksum(&native_out);

    let prepared = PreparedCall::new(&func, &[ArgSpec::FlatIn]).unwrap();
    let mut store = Store::new(N * 8 * 4 + 4096);

    // One call touching every quadrant: host->guest param (Q1) -> guest->host param (Q3)
    // -> guest->host result (Q4) -> host->guest result (Q2).
    let pipeline_sum = prepared
        .bind()
        .arg_flat_in(&positions)
        .invoke_scoped(&mut store, |results| checksum(results.flat_list_view(0)))
        .unwrap();

    assert_eq!(native_sum, pipeline_sum, "Q3+Q4 pipeline must match native");
    println!("  [Q3+Q4] guest<->host import OK  (checksum {pipeline_sum}; all four quadrants)");
}

fn main() {
    println!("call-builder-sim: canonical-ABI transfer matrix ({N} entities)");
    demo_params();
    demo_results();
    demo_import_pipeline();
    println!("  OK: every quadrant matches its native reference.");
}
