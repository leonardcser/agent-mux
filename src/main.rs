mod agent;
mod tui;

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
        return run_bench();
    }

    let tmux = std::env::var("TMUX").unwrap_or_default();
    let session_id = tmux.rsplit('/').next().unwrap_or(&tmux).to_string();
    let _ = agent::start_watch();
    tui::run(session_id)
}

fn run_bench() -> Result<()> {
    let start = std::time::Instant::now();
    let panes = agent::list_panes()?;
    eprintln!(
        "ListPanes:      {:?} (panes={})",
        start.elapsed(),
        panes.len()
    );
    if let Some(pane) = panes.first() {
        let t = std::time::Instant::now();
        let _ = agent::capture_pane(&pane.target, 50);
        eprintln!("CapturePane:    {:?}", t.elapsed());
    }
    eprintln!("Total:          {:?}", start.elapsed());
    Ok(())
}
