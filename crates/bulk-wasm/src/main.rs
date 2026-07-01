//! Issue #3 (bulk comparison variant): one component call per step, passing the
//! whole `list<entity>` in and getting the updated list back.
//!
//! This is the synchronous, no-async counterpart to `stream-wasm`: it has the same
//! 1000-calls-per-step call count, so comparing the two isolates the cost of the
//! streaming/async machinery. Structurally it mirrors `crates/naive-wasm`, but each
//! step is a single bulk call instead of one call per entity.

use anyhow::Result;
use scenario::{checksum, initial_state, measure, DT, STEPS};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

wasmtime::component::bindgen!({
    path: "../../guests/bulk-wasm-guest/wit",
    world: "guest",
});

use exports::bad_ideas::bulk::sim::Entity;

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

/// One full simulation: fresh initial state, all [`STEPS`] steps, each step a single
/// bulk call passing the whole entity list through the guest.
fn run(sim: &mut Sim) -> Vec<[f32; 4]> {
    let mut entities: Vec<Entity> = initial_state()
        .iter()
        .map(|&[px, py, vx, vy]| Entity { px, py, vx, vy })
        .collect();

    for _ in 0..STEPS {
        entities = sim.advance(&entities);
    }

    entities.iter().map(|e| [e.px, e.py, e.vx, e.vy]).collect()
}

fn main() -> Result<()> {
    let mut sim = Sim::new()?;
    let results = measure("bulk-wasm", || checksum(&run(&mut sim)));

    let mean_ms = results.mean_ns() / 1e6;
    let min_ms = results.min_ns() as f64 / 1e6;
    let per_step_us = (results.min_ns() as f64 / results.steps as f64) / 1e3;

    println!("bulk-wasm: one call per step, whole list<entity> in/out");
    println!(
        "  {} entities x {} steps, {} warmup + {} timed repeats",
        results.entity_count, results.steps, results.warmups, results.repeats
    );
    println!("  fastest run: {min_ms:.2} ms  (mean {mean_ms:.2} ms)");
    println!("  per step:    {per_step_us:.2} us");
    println!(
        "  throughput:  {:.2} M entity-steps/sec",
        results.entity_steps_per_sec() / 1e6
    );
    println!("  checksum:    {}", results.checksum);

    let path = results.write_json()?;
    println!("wrote {}", path.display());
    Ok(())
}
