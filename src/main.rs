mod agent;
mod tui;

#[global_allocator]
static ALLOC: smelt_perf::alloc::Counting = smelt_perf::alloc::Counting;

use anyhow::{Result, bail};

fn main() -> Result<()> {
    if std::env::var_os("TMUX").is_none() {
        bail!("agent-mux must be run inside tmux");
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "watch") {
        return agent::watch::run();
    }
    if args.iter().any(|arg| arg == "refresh") {
        return agent::watch::refresh_once();
    }

    if args
        .iter()
        .any(|arg| arg == "--bench" || arg == "--bench-cold" || arg == "--bench-loop")
    {
        return run_bench(&args);
    }

    let tmux = std::env::var("TMUX").unwrap_or_default();
    let session_id = tmux.rsplit('/').next().unwrap_or(&tmux).to_string();
    let _ = agent::start_watch();
    tui::run(session_id)
}

fn run_bench(args: &[String]) -> Result<()> {
    smelt_perf::alloc::enable();
    smelt_perf::perf::enable();
    smelt_perf::perf::clear();

    let iterations = if args.iter().any(|arg| arg == "--bench-loop") {
        10
    } else {
        1
    };

    for _ in 0..iterations {
        let _g = smelt_perf::perf::begin("bench.iteration");
        let panes = {
            let _g = smelt_perf::perf::begin("bench.list_panes");
            agent::list_panes()?
        };
        smelt_perf::perf::record_value("bench.panes", panes.len() as u64);
        if let Some(pane) = panes.first() {
            let _g = smelt_perf::perf::begin("bench.preview_capture");
            let content = agent::capture_pane(&pane.target, 50)?;
            smelt_perf::perf::record_value("bench.preview_bytes", content.len() as u64);
        }
    }

    smelt_perf::perf::print_summary();
    let alloc = smelt_perf::alloc::snapshot();
    eprintln!(
        "allocs={} reallocs={} bytes={} peak={}",
        alloc.allocs, alloc.reallocs, alloc.bytes_allocated, alloc.peak_bytes
    );
    Ok(())
}
