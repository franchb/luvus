//! A/B measurement of the `VtEngine` implementations through Luvus's own
//! code paths.
//!
//! `CONTRIBUTING.md` asks for performance changes to be measured before and
//! after. This is where the terminal engines get measured: not the VT parsers
//! in isolation, but `advance`, `for_each_cell` and `backend_capture` as Luvus
//! calls them, so the numbers describe what a pane actually does.
//!
//! ```sh
//! scripts/bench-engines.sh
//! ```
//!
//! Or one engine at a time:
//!
//! ```sh
//! LUVUS_VT_ENGINE=shitty cargo test --release --features shitty-engine \
//!     --bin luvus bench_engines -- --ignored --nocapture
//! ```
//!
//! # Method, and what it is worth
//!
//! One engine per process, chosen by `LUVUS_VT_ENGINE`, so the allocator is
//! never shared between the two and the memory figure means something. Feeds
//! arrive in 8 KiB chunks because that is roughly what a read from a pty
//! delivers, and feeding one large slice instead measures a case that never
//! happens. Each timing is the median of several runs after a warm-up.
//!
//! Memory is an RSS delta around a fresh engine rather than an absolute, so
//! the process baseline drops out, and it is reported per retained row
//! because engines given the same byte budget do not retain the same number
//! of rows. The engines' own `history_metrics` are estimates - neither
//! reports `exact_bytes` - so RSS is the honest measure here.
//!
//! Numbers from different machines are not comparable. Numbers from the same
//! machine before and after a change are, which is the point.

use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

use super::{create_engine, VtEngine, VtEngineKind};
use crate::terminal::backend::CaptureMode;
use crate::terminal::pty::InputAction;

const COLS: u16 = 120;
const ROWS: u16 = 40;
const LINES: usize = 20_000;
/// About what one read from a pty returns.
const CHUNK: usize = 8 * 1024;
const ITERATIONS: usize = 7;
const HISTORY_BUDGET: usize = 8 * 1024 * 1024;

/// Plain output: a build log, a file listing, anything that is mostly text.
fn ascii() -> Vec<u8> {
    (0..LINES)
        .flat_map(|i| {
            format!("{i:6} the quick brown fox jumps over the lazy dog 0123456789\r\n").into_bytes()
        })
        .collect()
}

/// Heavily styled output: an agent's transcript, a colourised diff.
fn sgr() -> Vec<u8> {
    (0..LINES)
        .flat_map(|i| {
            format!(
                "\x1b[3{}m{i:6}\x1b[0m \x1b[1;38;5;{}mcompiling\x1b[0m \
                 \x1b[38;2;{};{};{}mcrate-{i}\x1b[0m ok\r\n",
                i % 8,
                i % 256,
                i % 255,
                (i * 7) % 255,
                (i * 13) % 255,
            )
            .into_bytes()
        })
        .collect()
}

/// Wide characters and clusters, where the engines disagree most about width.
fn unicode() -> Vec<u8> {
    (0..LINES)
        .flat_map(|i| format!("{i:6} 日本語のテキスト café naïve \u{1F600} done\r\n").into_bytes())
        .collect()
}

fn new_engine(kind: VtEngineKind) -> Arc<Mutex<dyn VtEngine>> {
    let (tx, rx) = mpsc::channel::<InputAction>();
    // The engine sends replies; a dropped receiver would turn every send into
    // an error and measure a path no running pane takes.
    std::mem::forget(rx);
    create_engine(kind, COLS, ROWS, tx, HISTORY_BUDGET)
}

fn feed(engine: &mut dyn VtEngine, corpus: &[u8]) {
    for chunk in corpus.chunks(CHUNK) {
        engine.advance(chunk);
    }
}

/// Median milliseconds to feed `corpus` into a fresh engine.
fn feed_ms(kind: VtEngineKind, corpus: &[u8]) -> f64 {
    let mut samples = Vec::with_capacity(ITERATIONS);
    for iteration in 0..=ITERATIONS {
        let handle = new_engine(kind);
        let mut engine = handle.lock().expect("engine");
        let start = Instant::now();
        feed(&mut *engine, corpus);
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        if iteration > 0 {
            samples.push(elapsed);
        }
    }
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

/// Resident set size in KiB, or 0 where /proc is not mounted.
fn rss_kb() -> usize {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|status| status.split_whitespace().nth(1)?.parse::<usize>().ok())
        .map_or(0, |pages| pages * 4)
}

#[test]
#[ignore = "measurement, not a check: scripts/bench-engines.sh"]
// The debug build stops at the panic below, which makes the rest of the body
// unreachable in exactly that build and nowhere else.
#[cfg_attr(debug_assertions, allow(unreachable_code))]
fn bench_engines() {
    // A debug build measures nothing useful. The shitty core is a C++ library
    // that is optimised however it was packaged, while the alacritty engine is
    // compiled with this crate - so an unoptimised build flatters one side by
    // a margin larger than anything being measured.
    #[cfg(debug_assertions)]
    panic!(
        "run with --release. The shitty core is a C++ library optimised when it \
         was packaged, while the alacritty engine is compiled with this crate, so \
         an unoptimised build flatters one side by more than anything measured here."
    );

    let kind = VtEngineKind::configured();
    println!(
        "\nengine {kind:?}  grid {COLS}x{ROWS}  history {} MiB",
        HISTORY_BUDGET >> 20
    );

    // Memory first, while the allocator has not yet grown and shrunk around
    // the engines the timings create: measured after those, a fresh engine
    // reuses freed pages and the delta collapses to nothing.
    let corpus = ascii();
    let before = rss_kb();
    {
        let handle = new_engine(kind);
        let mut engine = handle.lock().expect("engine");
        feed(&mut *engine, &corpus);
        let rows = engine.history_metrics().retained_rows.max(1);
        let delta = rss_kb().saturating_sub(before);
        if delta == 0 {
            println!("memory      unavailable (no /proc, or no growth to measure)");
        } else {
            println!(
                "memory   {rows:6} rows {delta:7} KiB {:6.2} KiB/row",
                delta as f64 / rows as f64
            );
        }
    }

    for (name, corpus) in [("ascii", ascii()), ("sgr", sgr()), ("unicode", unicode())] {
        let ms = feed_ms(kind, &corpus);
        let mib = corpus.len() as f64 / (1024.0 * 1024.0);
        println!(
            "feed {name:8} {mib:6.2} MiB {ms:8.2} ms {:7.1} MiB/s",
            mib / (ms / 1000.0)
        );
    }

    // The read paths, on an engine already holding a full scrollback.
    let handle = new_engine(kind);
    let mut engine = handle.lock().expect("engine");
    feed(&mut *engine, &corpus);

    let mut cells = 0usize;
    let start = Instant::now();
    for _ in 0..200 {
        engine.for_each_cell(&mut |_, _, _, _| cells += 1);
    }
    let grid_ms = start.elapsed().as_secs_f64() * 1000.0;
    // Guard against timing an empty grid, which would look like a win.
    assert!(cells > 0, "the grid should have been visited");
    println!(
        "read grid  200x  {grid_ms:8.2} ms  ({} cells/pass)",
        cells / 200
    );

    let mut lines = 0usize;
    let start = Instant::now();
    for _ in 0..50 {
        lines += engine
            .backend_capture(CaptureMode::RecentUnwrapped, 500, false, 1 << 20)
            .lines;
    }
    let capture_ms = start.elapsed().as_secs_f64() * 1000.0;
    assert!(lines > 0, "the capture should have returned lines");
    println!(
        "capture     50x  {capture_ms:8.2} ms  ({} lines/pass)",
        lines / 50
    );
}
