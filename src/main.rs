//! neutronsync CLI: a thin front-end over the neutronsync core library.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use neutronsync::backend::{effective_dry_run, select};
use neutronsync::config::{self, Config};
use neutronsync::driveapi::DriveApi;
use neutronsync::engine::Engine;
use neutronsync::events::{EventSink, SyncEvent};
use neutronsync::logger::Logger;
use neutronsync::remote::Remote;
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
        match ev {
            SyncEvent::OpFinished {
                pair,
                action,
                path,
                ok,
                error,
            } => {
                let _ = self.stats.record_op(
                    pair,
                    action,
                    path,
                    *ok,
                    error.as_deref(),
                    neutronsync::datefmt::now_epoch(),
                );
            }
            // A CLI run's conflicts land in the same inbox the window shows.
            SyncEvent::ConflictSettled {
                pair,
                path,
                outcome,
                copy,
            } => {
                let _ = self.stats.record_conflict(
                    pair,
                    path,
                    *outcome,
                    copy.as_deref(),
                    neutronsync::datefmt::now_epoch(),
                );
            }
            _ => {}
        }
    }
}

#[derive(Parser)]
#[command(
    name = "neutronsync",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("NEUTRONSYNC_BUILD_SHA"), ")"),
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
    /// Sign in to Proton through the sidecar.
    Login,
    /// Sign out and remove the saved session.
    Logout,
    /// Show config, resolved pairs and account status.
    Status,
    /// Probe the sidecar and its remote listing.
    Doctor { path: Option<String> },
    /// Run the bidirectional sync.
    Sync {
        /// Pair name(s) to sync (default: all).
        pair: Vec<String>,
        #[arg(long)]
        dry_run: bool,
        /// Merge both sides afresh, deleting nothing (brings held-back files back).
        #[arg(long)]
        resync: bool,
        /// Let this run delete more files than the allowance permits, for the
        /// named pair(s) only. Never applies to every pair at once.
        #[arg(long)]
        allow_mass_delete: bool,
        #[arg(long)]
        json: bool,
    },
    /// Watch local folders and sync on change (plus a periodic rescan).
    Watch {
        /// Pair name(s) to watch (default: all).
        pair: Vec<String>,
    },
    /// Run the sync engine without a desktop and expose it over HTTP
    /// (/api/v1, with live updates as server-sent events).
    #[cfg(feature = "serve")]
    Serve {
        /// Address and port to listen on. Anything but loopback needs a token.
        #[arg(long, default_value = "127.0.0.1:8090")]
        listen: String,
        /// File holding the API token (else NEUTRONSYNC_API_TOKEN). At least
        /// 32 characters; required on every request except /api/v1/health.
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// Folder the API may browse and add synced folders under. Repeat for
        /// several. Without it, the home directory.
        #[arg(long = "local-root")]
        local_root: Vec<PathBuf>,
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
            pair,
            dry_run,
            resync,
            allow_mass_delete,
            json,
        } => cmd_sync(cli, pair, *dry_run, *resync, *allow_mass_delete, *json),
        Cmd::Watch { pair } => cmd_watch(cli, pair),
        #[cfg(feature = "serve")]
        Cmd::Serve {
            listen,
            token_file,
            local_root,
        } => neutronsync::server::run(neutronsync::server::ServeOptions {
            config: cli.config.clone(),
            listen: listen.clone(),
            token_file: token_file.clone(),
            local_roots: local_root.clone(),
        }),
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
    DriveApi::new(&cfg)?.auth("auth.logout", json!({}))?;
    println!("Logged out.");
    Ok(ExitCode::SUCCESS)
}

fn cmd_status(cli: &Cli) -> anyhow::Result<ExitCode> {
    let cfg = load_cfg(cli)?;
    println!("neutronsync {}", env!("CARGO_PKG_VERSION"));
    if let Some(p) = &cfg.source_path {
        println!("config          {}", p.display());
    }
    match DriveApi::new(&cfg).and_then(|api| api.status()) {
        Ok(status) => println!(
            "signed in       {} (sidecar auth.status; account: {})",
            status["signed_in"],
            status["account"].as_str().unwrap_or("none")
        ),
        Err(error) => println!("signed in       unavailable: {error}"),
    }
    println!("remote root     {}", cfg.remote_root);
    println!(
        "propagate del.  {} (local -> {:?})",
        cfg.propagate_deletes, cfg.local_delete
    );
    println!(
        "delete guard    max_delete_files = {}, max_delete_percent = {}{}",
        cfg.max_delete_files,
        cfg.max_delete_percent,
        if cfg.max_delete_files == 0 && cfg.max_delete_percent == 0 {
            " (off)"
        } else {
            ""
        }
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
    let api = DriveApi::new(&cfg)?;
    let status = api.status()?;
    println!(
        "sidecar auth.status: signed_in={}, account={}",
        status["signed_in"],
        status["account"].as_str().unwrap_or("none")
    );
    let entries = api.list_dir(path.unwrap_or(&cfg.remote_root))?;
    println!("sidecar node.list: {} entries", entries.len());
    Ok(ExitCode::SUCCESS)
}

fn cmd_sync(
    cli: &Cli,
    pairs: &[String],
    dry_run: bool,
    resync: bool,
    allow_mass_delete: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    // The permission to delete past the allowance is given for named pairs
    // only, so it can never be applied to every pair by accident.
    if allow_mass_delete && pairs.is_empty() {
        eprintln!(
            "usage: neutronsync sync --allow-mass-delete NAME...\n\
             --allow-mass-delete needs the name of the folder pair whose held-back deletions \
             you want carried out. Nothing was synced."
        );
        return Ok(ExitCode::from(2));
    }
    let cfg = load_cfg(cli)?;
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
        match Stats::open(&cfg.state_dir) {
            Ok(stats) => Some(DbSink { stats }),
            Err(e) => {
                // Every pair refuses the same file below; say why once here.
                log.warn(&format!("sync state could not be opened: {e:#}"));
                None
            }
        }
    };
    let mut engine = Engine::new(&cfg, proton, &log, dry_run);
    if let Some(sink) = &db_sink {
        engine.set_observer(Some(sink as &dyn EventSink), None);
    }
    engine.set_allow_mass_delete(allow_mass_delete);
    let mut total_errors = 0usize;
    let mut summaries: Vec<(String, usize, usize, String)> = Vec::new();
    for pair in &selected {
        match engine.sync_pair(pair, resync) {
            Ok(res) => {
                total_errors += res.errors.len();
                if let Some(held) = res.held {
                    // A held run is a failed run: say what was refused, on
                    // which side, and the two ways to answer it.
                    total_errors += 1;
                    let sides = neutronsync::engine::held_sides(&held);
                    if dry_run {
                        log.error(&format!(
                            "pair {:?}: would hold back {} deletions ({sides}) of {} tracked \
                             files. Nothing would be deleted. To bring the files back: \
                             neutronsync sync --resync {}  To delete them: \
                             neutronsync sync --allow-mass-delete {}",
                            res.pair,
                            held.total(),
                            held.tracked,
                            res.pair,
                            res.pair
                        ));
                    } else {
                        log.error(&format!(
                            "pair {:?}: {} deletions held back ({sides}) of {} tracked files. \
                             Nothing was deleted. To bring the files back: \
                             neutronsync sync --resync {}  To delete them: \
                             neutronsync sync --allow-mass-delete {}",
                            res.pair,
                            held.total(),
                            held.tracked,
                            res.pair,
                            res.pair
                        ));
                    }
                }
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
                log.error(&format!("pair {:?} failed: {e:#}", pair.name));
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

    // CLI runs until Ctrl-C; the stop flag is only used by the GUI toggle.
    let stop = std::sync::atomic::AtomicBool::new(false);
    neutronsync::watcher::watch(&cfg, selected, &log, &stop)?;
    Ok(ExitCode::SUCCESS)
}
