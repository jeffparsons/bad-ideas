//! Benchmark orchestration + README regeneration.
//!
//! Run via `./scripts/bench.sh` (a thin wrapper). This binary:
//!
//! 1. Builds the whole workspace in release mode.
//! 2. Runs each registered implementation, which writes `results/<name>.json`.
//! 3. Rewrites the auto-generated regions of `README.md` (an environment block and
//!    a results table) in place, between HTML-comment markers.
//!
//! Pass `--readme-only` to skip steps 1 and 2 and just regenerate the README from
//! whatever `results/*.json` already exist (handy when iterating on formatting).
//!
//! The implementation registry below is the single source of truth for what gets
//! built, run, and tabulated — adding a future implementation is a one-line change.

use std::path::{Path, PathBuf};
use std::process::Command;

/// One benchmark implementation.
struct Impl {
    /// Results filename stem and the name passed to `scenario::measure`.
    name: &'static str,
    /// Cargo package (and binary) to run.
    package: &'static str,
    /// Human-readable description for the README table.
    blurb: &'static str,
    /// Optional Wasm guest package to build for `wasm32-wasip2` before running.
    guest: Option<&'static str>,
}

/// The implementations to build, run, and tabulate, in display order.
const IMPLEMENTATIONS: &[Impl] = &[
    Impl {
        name: "baseline",
        package: "baseline",
        blurb: "Array-of-structs native reference",
        guest: None,
    },
    Impl {
        name: "naive-wasm",
        package: "naive-wasm",
        blurb: "Naive Wasm Component (host call per entity per step)",
        guest: Some("naive-wasm-guest"),
    },
];

const ENV_START: &str = "<!-- BENCH_ENV:START -->";
const ENV_END: &str = "<!-- BENCH_ENV:END -->";
const TABLE_START: &str = "<!-- BENCH_TABLE:START -->";
const TABLE_END: &str = "<!-- BENCH_TABLE:END -->";

fn main() {
    let readme_only = std::env::args().any(|a| a == "--readme-only");
    let root = workspace_root();

    if !readme_only {
        build_release(&root);
        for imp in IMPLEMENTATIONS {
            run_impl(&root, imp);
        }
    }

    regenerate_readme(&root);
    println!("Updated {}", root.join("README.md").display());
}

/// Workspace root, derived from this crate's manifest dir (`crates/xtask`) so it
/// works regardless of the current working directory.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/xtask should have a workspace root two levels up")
        .to_path_buf()
}

fn build_release(root: &Path) {
    println!("Building host crates (release)...");
    run_checked(
        Command::new("cargo")
            .args(["build", "--release"])
            .current_dir(root),
        "cargo build --release",
    );

    for guest in IMPLEMENTATIONS.iter().filter_map(|imp| imp.guest) {
        println!("Building guest '{guest}' (release, wasm32-wasip2)...");
        run_checked(
            Command::new("cargo")
                .args([
                    "build",
                    "--release",
                    "--target",
                    "wasm32-wasip2",
                    "-p",
                    guest,
                ])
                .current_dir(root),
            &format!("cargo build guest {guest}"),
        );
    }
}

fn run_impl(root: &Path, imp: &Impl) {
    println!("Running '{}'...", imp.name);
    let bin = root.join("target").join("release").join(imp.package);
    // Run from the workspace root so `results/<name>.json` lands in the right place.
    run_checked(
        Command::new(&bin).current_dir(root),
        &format!("{} benchmark", imp.name),
    );
}

fn run_checked(cmd: &mut Command, what: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to launch {what}: {e}"));
    if !status.success() {
        panic!("{what} failed with {status}");
    }
}

fn regenerate_readme(root: &Path) {
    let readme_path = root.join("README.md");
    let readme = std::fs::read_to_string(&readme_path)
        .unwrap_or_else(|e| panic!("failed to read README.md: {e}"));

    let env_block = environment_block();
    let table = results_table(root);

    let readme = replace_region(&readme, ENV_START, ENV_END, &env_block);
    let readme = replace_region(&readme, TABLE_START, TABLE_END, &table);

    std::fs::write(&readme_path, readme).expect("failed to write README.md");
}

/// Replace the text between `start` and `end` markers (exclusive) with `inner`,
/// keeping the markers themselves. Panics if either marker is missing.
fn replace_region(text: &str, start: &str, end: &str, inner: &str) -> String {
    let s = text
        .find(start)
        .unwrap_or_else(|| panic!("README.md is missing marker: {start}"));
    let after_start = s + start.len();
    let e = text[after_start..]
        .find(end)
        .map(|i| after_start + i)
        .unwrap_or_else(|| panic!("README.md is missing marker: {end}"));

    format!("{}\n{}\n{}", &text[..after_start], inner, &text[e..])
}

/// Best-effort machine/environment description, as a markdown bullet list.
fn environment_block() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let cpu = cpu_brand().unwrap_or_else(|| "unknown".to_string());
    let rustc = command_first_line("rustc", &["--version"]).unwrap_or_else(|| "unknown".to_string());

    let warmups = std::env::var("BENCH_WARMUPS").unwrap_or_else(|_| "2".to_string());
    let repeats = std::env::var("BENCH_REPEATS").unwrap_or_else(|_| "5".to_string());

    format!(
        "_Measured on:_\n\n- **CPU:** {cpu}\n- **OS / arch:** {os} / {arch}\n- **Toolchain:** {rustc}\n- **Build profile:** release (`opt-level=3`, `lto=true`, `codegen-units=1`)\n- **Warmup / timed repeats:** {warmups} / {repeats}",
    )
}

/// Best-effort CPU brand string per platform.
fn cpu_brand() -> Option<String> {
    match std::env::consts::OS {
        "macos" => command_first_line("sysctl", &["-n", "machdep.cpu.brand_string"]),
        "linux" => {
            let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").ok()?;
            cpuinfo.lines().find_map(|l| {
                l.strip_prefix("model name")
                    .and_then(|r| r.split(':').nth(1))
                    .map(|v| v.trim().to_string())
            })
        }
        _ => None,
    }
}

fn command_first_line(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().next()?.trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
}

/// Build the results table by reading each implementation's `results/<name>.json`.
/// Missing result files are noted with placeholders rather than failing.
fn results_table(root: &Path) -> String {
    let mut out = String::new();
    out.push_str("| Implementation | Entities | Steps | Fastest | Mean | Throughput | Checksum |\n");
    out.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: |\n");

    for imp in IMPLEMENTATIONS {
        let path = root.join("results").join(format!("{}.json", imp.name));
        let label = format!("{} (`{}`)", imp.blurb, imp.name);

        match std::fs::read_to_string(&path).ok().and_then(|j| Record::parse(&j)) {
            Some(r) => {
                let min_ms = r.min_ns as f64 / 1e6;
                let mean_ms = r.mean_ns / 1e6;
                let throughput = r.entity_steps_per_sec / 1e6;
                out.push_str(&format!(
                    "| {label} | {} | {} | {min_ms:.2} ms | {mean_ms:.2} ms | {throughput:.1} M/s | {} |\n",
                    thousands(r.entity_count),
                    thousands(r.steps),
                    r.checksum,
                ));
            }
            None => {
                out.push_str(&format!("| {label} | — | — | — | — | — | _no results_ |\n"));
            }
        }
    }

    out
}

/// The subset of a `results/<name>.json` document we tabulate.
struct Record {
    entity_count: u64,
    steps: u64,
    min_ns: u64,
    mean_ns: f64,
    entity_steps_per_sec: f64,
    checksum: String,
}

impl Record {
    /// Parse our own flat, one-field-per-line JSON. Not a general JSON parser — it
    /// relies on the fixed shape emitted by `scenario::Results::to_json`.
    fn parse(json: &str) -> Option<Record> {
        Some(Record {
            entity_count: field(json, "entity_count")?.parse().ok()?,
            steps: field(json, "steps")?.parse().ok()?,
            min_ns: field(json, "min_ns")?.parse().ok()?,
            mean_ns: field(json, "mean_ns")?.parse().ok()?,
            entity_steps_per_sec: field(json, "entity_steps_per_sec")?.parse().ok()?,
            checksum: field(json, "checksum")?,
        })
    }
}

/// Extract the value for `key` from our flat JSON, stripping quotes and a trailing
/// comma. Returns `None` if the key is absent.
fn field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    for line in json.lines() {
        if let Some(rest) = line.trim().strip_prefix(&needle) {
            let v = rest.trim().trim_end_matches(',').trim().trim_matches('"');
            return Some(v.to_string());
        }
    }
    None
}

/// Format an integer with thousands separators (e.g. `100000` -> `100,000`).
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    let len = digits.len();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}
