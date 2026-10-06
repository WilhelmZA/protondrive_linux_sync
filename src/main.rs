//! neutronsync CLI: a thin front-end over the neutronsync core library.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use neutronsync::backend::{effective_dry_run, select};
use neutronsync::config::{self, Backend, Config};
use neutronsync::driveapi::DriveApi;
use neutronsync::engine::Engine;
use neutronsync::events::{EventSink, SyncEvent};
use neutronsync::logger::Logger;
use neutronsync::protoncli::ProtonCli;
use neutronsync::stats::Stats;
use neutronsync::EXAMPLE_CONFIG;
use serde_json::json;

/// Records each finished op into `stats.db` so a CLI sync shows up in the same
/// activity feed the GUI reads (the GUI restores the feed from this table).
/// Best-effort: a DB error is ignored and never interrupts the sync. `Stats`
/// wraps a `Mutex<Connection>`, so this is `Sync` and safe to share across the
/// concurrent download workers.
struct DbSink {
    stats: Stats,
}

impl EventSink for DbSink {
    fn emit(&self, ev: &SyncEvent) {
        if let SyncEvent::OpFinished {
            pair,
            action,
            path,
            ok,
            error,
        } = ev
        {
            let _ = self.stats.record_op(
                pair,
                action,
                path,
                *ok,
                error.as_deref(),
                neutronsync::datefmt::now_epoch(),
            );
        }
    }
}

#[derive(Parser)]
#[command(
    name = "neutronsync",
    version,
    about = "Bidirectional Proton Drive folder sync with a built-in API sidecar"
)]
struct Cli {
    /// Path to the config file.
    #[arg(short, long, global = true)]
    config: Option<String>,
    /// Debug output.
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Warnings and errors only.
    #[arg(short, long, global = true)]
    quiet: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter config file.
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Sign in through the selected backend.
    Login,
    /// Sign out through the selected backend.
    Logout,
    /// Show config, resolved pairs and account status.
    Status,
    /// Probe the selected backend and its remote listing.
    Doctor { path: Option<String> },
    /// Run the bidirectional sync.
    Sync {
        /// Override the configured remote backend for this run.
        #[arg(long, value_enum)]
        backend: Option<Backend>,
        /// Pair name(s) to sync (default: all).
        pair: Vec<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        resync: bool,
        #[arg(long)]
        json: bool,
    },
    /// Watch local folders and sync on change (plus a periodic rescan).
    Watch {
        /// Pair name(s) to watch (default: all).
        pair: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli) -> anyhow::Result<ExitCode> {
    match &cli.command {
        Cmd::Init { force } => cmd_init(cli, *force),
        Cmd::Login => cmd_login(cli),
        Cmd::Logout => cmd_logout(cli),
        Cmd::Status => cmd_status(cli),
        Cmd::Doctor { path } => cmd_doctor(cli, path.as_deref()),
        Cmd::Sync {
            backend,
            pair,
            dry_run,
            resync,
            json,
        } => cmd_sync(cli, pair, *dry_run, *resync, *json, *backend),
        Cmd::Watch { pair } => cmd_watch(cli, pair),
    }
}

fn load_cfg(cli: &Cli) -> anyhow::Result<Config> {
    config::load(cli.config.as_deref())
}

fn cmd_init(cli: &Cli, force: bool) -> anyhow::Result<ExitCode> {
    let dest: PathBuf = match &cli.config {
        Some(c) => config::expand(c),
        None => config::default_config_path(),
    };
    if dest.exists() && !force {
        println!(
            "Config already exists: {} (use --force to overwrite)",
            dest.display()
        );
        return Ok(ExitCode::from(1));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest, EXAMPLE_CONFIG)?;
    println!("Wrote starter config: {}", dest.display());
    println!("Edit it, then run:  neutronsync login  &&  neutronsync sync --dry-run");
    Ok(ExitCode::SUCCESS)
}

fn cmd_login(cli: &Cli) -> anyhow::Result<ExitCode> {
    let cfg = load_cfg(cli)?;
    if cfg.backend == Backend::Api {
        let api = DriveApi::new(&cfg)?;
        let username = prompt("Username: ")?;
        let password = rpassword::prompt_password("Password: ")?;
        let mut result = api.auth(
            "auth.login",
            json!({"username":username,"password":password}),
        )?;
        loop {
            if result["ok"] == true {
                break;
            }
            result = if result["need_2fa"] == true {
                api.auth(
                    "auth.submit_2fa",
                    json!({"code":rpassword::prompt_password("TOTP code: ")?}),
                )?
            } else if result["need_mailbox_password"] == true {
                api.auth(
                    "auth.submit_mailbox_password",
                    json!({"password":rpassword::prompt_password("Mailbox password: ")?}),
                )?
            } else if result["need_human_verification"] == true {
                let url = result["url"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("api backend: fatal"))?;
                if !url.starts_with("https://verify.proton.me/") {
                    anyhow::bail!("api backend: fatal");
                }
                println!("Complete human verification: {url}");
                if prompt("Open in your browser? [y/N] ")?.eq_ignore_ascii_case("y") {
                    let _ = std::process::Command::new("xdg-open").arg(url).status();
                }
                prompt("Press Enter after verification completes: ")?;
                api.auth("auth.login", json!({"username":username,"password":password,"hv_token":result["token"],"hv_type":result["methods"][0]}))?
            } else {
                anyhow::bail!("api backend: fatal");
            };
        }
        println!("Logged in.");
        return Ok(ExitCode::SUCCESS);
    }
    let proton = ProtonCli::new(&cfg);
    proton.login()?;
    println!("Logged in.");
    Ok(ExitCode::SUCCESS)
}

fn prompt(label: &str) -> anyhow::Result<String> {
    use std::io::Write;
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        anyhow::bail!("sign-in cancelled");
    }
    Ok(line.trim().to_string())
}

fn cmd_logout(cli: &Cli) -> anyhow::Result<ExitCode> {
    let cfg = load_cfg(cli)?;
    if cfg.backend == Backend::Api {
        DriveApi::new(&cfg)?.auth("auth.logout", json!({}))?;
    } else {
        ProtonCli::new(&cfg).logout()?;
    }
    println!("Logged out.");
    Ok(ExitCode::SUCCESS)
}

fn cmd_status(cli: &Cli) -> anyhow::Result<ExitCode> {
    let cfg = load_cfg(cli)?;
    let proton = ProtonCli::new(&cfg);
    let resolved = proton
        .resolve_binary()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "NOT FOUND on PATH".to_string());
    println!("neutronsync {}", env!("CARGO_PKG_VERSION"));
    if let Some(p) = &cfg.source_path {
        println!("config          {}", p.display());
    }
    println!("backend         {:?}", cfg.backend);
    if cfg.backend == Backend::Api {
        match DriveApi::new(&cfg).and_then(|api| api.status()) {
            Ok(status) => println!(
                "signed in       {} (sidecar auth.status; account: {})",
                status["signed_in"],
                status["account"].as_str().unwrap_or("none")
            ),
            Err(error) => println!("signed in       unavailable: {error}"),
        }
    } else {
        println!("proton-drive    {resolved} (configured: {})", cfg.binary);
    }
    println!("remote root     {}", cfg.remote_root);
    println!(
        "propagate del.  {} (local -> {:?})",
        cfg.propagate_deletes, cfg.local_delete
    );
    println!("conflict        {:?}", cfg.conflict);
    println!("compare         {:?}", cfg.compare);
    println!("state dir       {}", cfg.state_dir.display());
    println!("pairs:");
    for p in &cfg.pairs {
        let exists = if p.local.exists() { "ok" } else { "missing" };
        println!(
            "  - {:<16} {}  [{exists}]  <->  {}",
            p.name,
            p.local.display(),
            p.remote
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_doctor(cli: &Cli, path: Option<&str>) -> anyhow::Result<ExitCode> {
    let cfg = load_cfg(cli)?;
    if cfg.backend == Backend::Api {
        let api = DriveApi::new(&cfg)?;
        let status = api.status()?;
        println!(
            "backend: api\nsidecar auth.status: signed_in={}, account={}",
            status["signed_in"],
            status["account"].as_str().unwrap_or("none")
        );
        use neutronsync::protoncli::Remote;
        let entries = api.list_dir(path.unwrap_or(&cfg.remote_root))?;
        println!("sidecar node.list: {} entries", entries.len());
        return Ok(ExitCode::SUCCESS);
    }
    let proton = ProtonCli::new(&cfg);
    println!(
        "binary: {}",
        proton
            .resolve_binary()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "NOT FOUND".into())
    );
    println!("version: {}", proton.version());
    let target = path.unwrap_or(&cfg.remote_root);
    println!("\nProbing `filesystem list -j {target}` ...");
    let raw = proton.raw_list(target)?;
    println!("--- raw JSON (first 1500 chars) ---");
    let shown: String = raw.chars().take(1500).collect();
    println!("{}", if shown.is_empty() { "(empty)" } else { &shown });
    println!("--- parsed by neutronsync ---");
    match ProtonCli::parse_list(&raw, target) {
        Ok(entries) => {
            if entries.is_empty() {
                println!(
                    "  (no entries parsed - if the raw JSON is non-empty, adjust protoncli.rs)"
                );
            }
            for e in entries.iter().take(50) {
                let kind = if e.is_dir { "dir " } else { "file" };
                println!(
                    "  {kind} size={:<12} mtime={:?} sha1={:?}  {}",
                    e.size, e.mtime, e.sha1, e.path
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            println!("  PARSE FAILED: {e}");
            Ok(ExitCode::from(1))
        }
    }
}

fn cmd_sync(
    cli: &Cli,
    pairs: &[String],
    dry_run: bool,
    resync: bool,
    json: bool,
    backend: Option<Backend>,
) -> anyhow::Result<ExitCode> {
    let mut cfg = load_cfg(cli)?;
    if let Some(backend) = backend {
        cfg.backend = backend;
    }
    let proton = select(&cfg)?;
    let log = if proton.read_only() {
        Logger::console(cli.verbose, cli.quiet)
    } else {
        Logger::new(&cfg.log_dir(), cli.verbose, cli.quiet)
    };
    let dry_run = effective_dry_run(&proton, dry_run, &log);

    // Select pairs.
    let selected: Vec<&neutronsync::config::Pair> = if pairs.is_empty() {
        cfg.pairs.iter().collect()
    } else {
        let mut out = Vec::new();
        for name in pairs {
            match cfg.pairs.iter().find(|p| &p.name == name) {
                Some(p) => out.push(p),
                None => {
                    log.error(&format!("unknown pair: {name}"));
                    return Ok(ExitCode::from(2));
                }
            }
        }
        out
    };

    if dry_run {
        log.info("DRY RUN - no changes will be made.\n");
    }

    // Record ops into stats.db so this CLI sync appears in the GUI activity
    // feed too (not just sync.log). Skipped on a dry run (nothing happened) and
    // declared before the engine so its borrow outlives the observer.
    let db_sink = if dry_run {
        None
    } else {
        Stats::open(&cfg.state_dir)
            .ok()
            .map(|stats| DbSink { stats })
    };
    let mut engine = Engine::new(&cfg, proton, &log, dry_run);
    if let Some(sink) = &db_sink {
        engine.set_observer(Some(sink as &dyn EventSink), None);
    }
    let mut total_errors = 0usize;
    let mut summaries: Vec<(String, usize, usize, String)> = Vec::new();
    for pair in &selected {
        match engine.sync_pair(pair, resync) {
            Ok(res) => {
                total_errors += res.errors.len();
                log.info(&format!(
                    "pair {:?}: {} change(s) applied, {} error(s){}\n",
                    res.pair,
                    res.applied,
                    res.errors.len(),
                    if dry_run { " (dry run)" } else { "" }
                ));
                summaries.push((res.pair, res.applied, res.errors.len(), res.plan_summary));
            }
            Err(e) => {
                total_errors += 1;
                log.error(&format!("pair {:?} failed: {e}", pair.name));
            }
        }
    }

    // Keep the activity-feed table bounded, same cap the GUI uses.
    if let Some(sink) = &db_sink {
        let _ = sink.stats.prune_ops(1000);
    }

    if json {
        let arr: Vec<serde_json::Value> = summaries
            .iter()
            .map(|(name, applied, errors, plan)| {
                serde_json::json!({"name": name, "applied": applied, "errors": errors, "plan": plan})
            })
            .collect();
        let out = serde_json::json!({"dry_run": dry_run, "pairs": arr});
        println!("{}", serde_json::to_string_pretty(&out)?);
    }

    Ok(if total_errors > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn cmd_watch(cli: &Cli, pairs: &[String]) -> anyhow::Result<ExitCode> {
    let cfg = load_cfg(cli)?;
    let log = Logger::new(&cfg.log_dir(), cli.verbose, cli.quiet);

    let selected: Vec<neutronsync::config::Pair> = if pairs.is_empty() {
        cfg.pairs.clone()
    } else {
        let mut out = Vec::new();
        for name in pairs {
            match cfg.pairs.iter().find(|p| &p.name == name) {
                Some(p) => out.push(p.clone()),
                None => {
                    log.error(&format!("unknown pair: {name}"));
                    return Ok(ExitCode::from(2));
                }
            }
        }
        out
    };

    if cfg.backend == Backend::Cli && ProtonCli::new(&cfg).resolve_binary().is_none() {
        log.error(&format!(
            "proton-drive not found (configured: {:?}). Install it or set cli.binary.",
            cfg.binary
        ));
        return Ok(ExitCode::from(1));
    }
    // CLI runs until Ctrl-C; the stop flag is only used by the GUI toggle.
    let stop = std::sync::atomic::AtomicBool::new(false);
    neutronsync::watcher::watch(&cfg, selected, &log, &stop)?;
    Ok(ExitCode::SUCCESS)
}
