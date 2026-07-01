//! Issue #2: the naive Wasm Component driver.
//!
//! The integration "system" lives in a Wasm Component ([`naive-wasm-guest`]); this
//! host embeds Wasmtime and calls the guest's `advance` **once per entity per
//! step** — 100M calls per full run. The per-call boundary cost is the whole point:
//! this measures how slow the most naive Wasm-Component approach is, as the starting
//! line we then try to beat.
//!
//! The guest component has no imports (pure compute), so the linker is empty and no
//! WASI wiring is needed. The guest is stateless, so a single instance is reused
//! across every warmup/timed run.

use anyhow::Result;
use scenario::{checksum, initial_state, measure, DT, STEPS};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

wasmtime::component::bindgen!({
    path: "../../guests/naive-wasm-guest/wit",
    world: "guest",
});

use exports::bad_ideas::naive::sim::Entity;

/// The component built by `naive-wasm-guest` for the `wasm32-wasip2` target,
/// resolved relative to this crate so it's independent of the working directory.
const COMPONENT_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32-wasip2/release/naive_wasm_guest.wasm"
);

/// A ready-to-call guest instance plus its store.
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
        // No imports to satisfy: an empty linker is enough.
        let linker = Linker::new(&engine);
        let mut store = Store::new(&engine, ());
        let bindings = Guest::instantiate(&mut store, &component, &linker)?;
        Ok(Sim { store, bindings })
    }

    #[inline]
    fn advance(&mut self, e: Entity, dt: f32) -> Entity {
        self.bindings
            .bad_ideas_naive_sim()
            .call_advance(&mut self.store, e, dt)
            .expect("guest advance call failed")
    }
}

/// One full simulation: fresh initial state, all [`STEPS`] steps, each step calling
/// the guest once per entity. Returns the final state as `[px, py, vx, vy]` rows.
fn run(sim: &mut Sim) -> Vec<[f32; 4]> {
    let mut entities: Vec<Entity> = initial_state()
        .iter()
        .map(|&[px, py, vx, vy]| Entity { px, py, vx, vy })
        .collect();

    for _ in 0..STEPS {
        for e in entities.iter_mut() {
            *e = sim.advance(*e, DT);
        }
    }

    entities.iter().map(|e| [e.px, e.py, e.vx, e.vy]).collect()
}

fn main() -> Result<()> {
    let mut sim = Sim::new()?;
    let results = measure("naive-wasm", || checksum(&run(&mut sim)));

    let mean_ms = results.mean_ns() / 1e6;
    let min_ms = results.min_ns() as f64 / 1e6;
    let per_step_us = (results.min_ns() as f64 / results.steps as f64) / 1e3;

    println!("naive-wasm: host calls guest once per entity per step");
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
