//! Issue #3 (batched comparison variant): like `bulk-wasm`, but the entities are
//! passed through the guest in fixed-size batches rather than one whole `list` per
//! step. With `BATCH_SIZE` entities per call there are `ceil(100000 / BATCH_SIZE)`
//! calls per step, filling the spectrum between `naive-wasm` (one call per entity)
//! and `bulk-wasm` (one call for the whole list).
//!
//! The guest is *identical* to `bulk-wasm`'s (`advance(list<entity>) -> list<entity>`),
//! so this reuses `bulk-wasm-guest` directly — only the host-side chunking differs.
//! The batch size is read from the `BATCH_SIZE` environment variable so a single
//! binary can drive a whole sweep; the result name is derived from it.

use anyhow::Result;
use scenario::{checksum, initial_state, measure, DT, STEPS};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

wasmtime::component::bindgen!({
    path: "../../guests/bulk-wasm-guest/wit",
    world: "guest",
});

use exports::bad_ideas::bulk::sim::Entity;

// Reuses the `bulk-wasm` guest component — the interface and math are the same.
const COMPONENT_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32-wasip2/release/bulk_wasm_guest.wasm"
);

struct Sim {
    store: Store<()>,
    bindings: Guest,
}

impl Sim {
    fn new() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)?;
        let component = Component::from_file(&engine, COMPONENT_PATH)?;
        let linker = Linker::new(&engine);
        let mut store = Store::new(&engine, ());
        let bindings = Guest::instantiate(&mut store, &component, &linker)?;
        Ok(Sim { store, bindings })
    }

    fn advance(&mut self, entities: &[Entity]) -> Vec<Entity> {
        self.bindings
            .bad_ideas_bulk_sim()
            .call_advance(&mut self.store, entities, DT)
            .expect("guest advance call failed")
    }
}

/// One full simulation: fresh state, all [`STEPS`] steps, each step passing the
/// entities through the guest in batches of `batch` at a time.
fn run(sim: &mut Sim, batch: usize) -> Vec<[f32; 4]> {
    let mut entities: Vec<Entity> = initial_state()
        .iter()
        .map(|&[px, py, vx, vy]| Entity { px, py, vx, vy })
        .collect();

    for _ in 0..STEPS {
        let mut next = Vec::with_capacity(entities.len());
        for chunk in entities.chunks(batch) {
            next.extend(sim.advance(chunk));
        }
        entities = next;
    }

    entities.iter().map(|e| [e.px, e.py, e.vx, e.vy]).collect()
}

fn main() -> Result<()> {
    let batch: usize = std::env::var("BATCH_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .expect("BATCH_SIZE environment variable required (e.g. 1000)");
    let name = format!("batched-wasm-{batch}");

    let mut sim = Sim::new()?;
    let results = measure(&name, || checksum(&run(&mut sim, batch)));

    let mean_ms = results.mean_ns() / 1e6;
    let min_ms = results.min_ns() as f64 / 1e6;
    let calls_per_step = results.entity_count.div_ceil(batch);

    println!("{name}: one call per {batch}-entity batch ({calls_per_step} calls/step)");
    println!(
        "  {} entities x {} steps, {} warmup + {} timed repeats",
        results.entity_count, results.steps, results.warmups, results.repeats
    );
    println!("  fastest run: {min_ms:.2} ms  (mean {mean_ms:.2} ms)");
    println!(
        "  throughput:  {:.2} M entity-steps/sec",
        results.entity_steps_per_sec() / 1e6
    );
    println!("  checksum:    {}", results.checksum);

    let path = results.write_json()?;
    println!("wrote {}", path.display());
    Ok(())
}
