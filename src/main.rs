//! fsp-mint command line: `config`, `service-key`, `serve`, `export-esp`,
//! `audit`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use fsp_mint::air_listener;
use fsp_mint::config::{self, Config, ConfigCell};
use fsp_mint::esp_air::AirIssuer;
use fsp_mint::esp_client::{self, Issuer};
use fsp_mint::issue::Issues;
use fsp_mint::lock::{DataLock, LockError};
use fsp_mint::server::{self, FspState};
use fsp_mint::spend::Spender;
use fsp_mint::store;
use secp256k1::{Keypair, SECP256K1, SecretKey};
use sp_wire::ServiceDid;
use sp_wire::air_request::addr;

const KEY_FILE: &str = "service_key.hex";

const USAGE: &str = "\
fsp-mint <command>

  config               Print the settings in use, and what each does.
  service-key          Make the service's DID key (service_key.hex) if there is
                       none, and print its public half for service.did.
  serve                Run the AIR listener (the only way wallets reach this mint:
                       mint info, keys, fees, rotation, issue and spend) and the
                       operator's routes on server.bind (loopback only): GET
                       /audit, GET /health for the health monitor and POST
                       /export-esp for export-esp. Needs air.server and air.host
                       (the listener's own AIR server) and, for the ESP mint,
                       which is also reached over AIR only, air.esp_did,
                       air.esp_server and air.esp_host. Asks the ESP mint which
                       network it is on before it starts. Takes the data
                       folder's writer lock: a second serve on the same folder
                       is refused. SIGHUP reads the settings again; fees and
                       limits change with no restart, and a setting that needs
                       a restart is named and keeps its running value.
  audit [--json]       Print what the mint owes and holds: outstanding FSP per
                       key set and in total (in FSP units and PSP), and the ESP
                       in reserve, matured and unmatured. Reads the data files,
                       writes nothing, and works while serve runs.
  export-esp --out DIR [--serve-only]
                       Write the ESP this mint has kept to DIR, one hand-over
                       file per redeemable-after day, for the service's
                       ecash-wallet to `receive` and `redeem`. Works while serve
                       runs: it asks the running serve, which does the export
                       between two issues. With no serve running it does the
                       export itself, unless --serve-only: then it fails and
                       touches nothing in the data folder (for a caller that
                       runs as another account than serve). The files are
                       readable by their owner only and each note is written
                       once.

Settings: fsp-mint.toml in FSP_DATA_DIR (default ~/.fsp-mint) or FSP_CONFIG,
then FSP_... variables. Data lives in FSP_DATA_DIR/<network>.";

fn base_dir() -> Result<PathBuf, String> {
    if let Ok(d) = std::env::var("FSP_DATA_DIR") {
        return Ok(PathBuf::from(d));
    }
    let home =
        std::env::var("HOME").map_err(|_| "HOME is not set: set FSP_DATA_DIR".to_string())?;
    Ok(PathBuf::from(home).join(".fsp-mint"))
}

/// The service key in `path`. An empty file is named as such (an older
/// fsp-mint could leave one if it stopped while making the key); a file that
/// holds anything but a whole key is damaged, and is never replaced here.
fn read_key(path: &Path) -> Result<SecretKey, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "cannot read {}: {e}. Run `fsp-mint service-key` first",
            path.display()
        )
    })?;
    if text.is_empty() {
        return Err(format!(
            "{} is empty: no key was ever written in it. Run `fsp-mint service-key`, which makes it again",
            path.display()
        ));
    }
    hex::decode(text.trim())
        .ok()
        .and_then(|b| SecretKey::from_slice(&b).ok())
        .ok_or_else(|| {
            format!(
                "{} is damaged: it does not hold a whole key (64 hex digits). fsp-mint does not replace it: restore it from your backup, or, if no service ever used it, move it aside and run `fsp-mint service-key` again",
                path.display()
            )
        })
}

fn load_key(dir: &Path, cfg: &Config) -> Result<Keypair, String> {
    let path = dir.join(KEY_FILE);
    let sk = read_key(&path)?;
    let kp = Keypair::from_secret_key(SECP256K1, &sk);
    // The whole DID is compared, the parity byte too: a DID that differs from
    // the key's only in 02 and 03 is another service.
    let own = ServiceDid::from(kp.public_key());
    match cfg.service() {
        Some(did) if did == own => Ok(kp),
        Some(did) => Err(format!(
            "service.did is {did}, but {} holds the key for {own}. They must match",
            path.display()
        )),
        None => Err(format!(
            "service.did is not set. Set it to {own} (the key in {})",
            path.display()
        )),
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let base = base_dir()?;
    let env = |n: &str| std::env::var(n).ok();
    let loaded = config::load(&base, &env)
        .map_err(|p| format!("fsp-mint settings are not valid:\n  {}", p.join("\n  ")))?;
    match &loaded.file {
        Some(f) => println!(
            "fsp-mint settings: file {}, then the environment:",
            f.display()
        ),
        None => println!("fsp-mint settings: no config file, so defaults and the environment:"),
    }
    for line in loaded.describe() {
        println!("{line}");
    }
    let cfg = loaded.config;
    let dir = base.join(&cfg.network);
    println!("fsp-mint on {}: data in {}", cfg.network, dir.display());
    let mut rot = cfg.rotation();
    rot.first_issue = fsp_mint::keysets::load_first_issue(&dir)
        .map_err(|e| format!("cannot read the data in {}: {e}", dir.display()))?;
    let show = |t: Option<u64>| {
        t.map(|t| format!("{t} (Unix seconds, UTC)"))
            .unwrap_or_else(|| "not yet".into())
    };
    if rot.activation_secs == 0 {
        println!("rotation: off (rotation.activation_hours = 0)");
    } else {
        println!("first issue: {}", show(rot.first_issue));
        println!(
            "rotation activates: {}",
            match rot.activation() {
                Some(t) => show(Some(t)),
                None => "activation_hours after the first issue".into(),
            }
        );
        println!(
            "next rotation: {}",
            show(rot.start_of(rot.epoch_at(fsp_mint::now_secs()) + 1))
        );
    }
    match args[0].as_str() {
        "config" => {
            println!();
            for s in config::SPECS {
                println!(
                    "{} ({}, default `{}`, {}): {}",
                    s.key,
                    s.env,
                    s.default,
                    if s.reload {
                        "reloads live"
                    } else {
                        "needs a restart"
                    },
                    s.what
                );
            }
            Ok(())
        }
        "service-key" => {
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let path = dir.join(KEY_FILE);
            // An empty file never held a key (an older fsp-mint could leave one
            // if it stopped while making it), so it is made again. Any other
            // file is kept as it is, damaged or not.
            if std::fs::metadata(&path).is_ok_and(|m| m.len() == 0) {
                std::fs::remove_file(&path)
                    .map_err(|e| format!("cannot remove the empty {}: {e}", path.display()))?;
                println!("{} was empty: making the key again", path.display());
            }
            if !path.exists() {
                let kp = Keypair::new(SECP256K1, &mut rand::rngs::OsRng);
                match store::write_secret(&path, &hex::encode(kp.secret_key().secret_bytes())) {
                    Ok(()) => println!("made {}", path.display()),
                    // Another `service-key` made it meanwhile: that key is the one.
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(format!("cannot write {}: {e}", path.display())),
                }
            }
            let sk = read_key(&path)?;
            println!(
                "service.did = {}",
                ServiceDid::from(Keypair::from_secret_key(SECP256K1, &sk).public_key())
            );
            Ok(())
        }
        "audit" => {
            let report = fsp_mint::audit::from_dir(&dir, fsp_mint::now_secs())
                .map_err(|e| format!("cannot read the data in {}: {e}", dir.display()))?;
            if args.iter().any(|a| a == "--json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
                );
            } else {
                print_audit(&report);
            }
            Ok(())
        }
        "export-esp" => {
            let out = match args.iter().position(|a| a == "--out") {
                Some(i) if i + 1 < args.len() => PathBuf::from(&args[i + 1]),
                _ => return Err("export-esp needs --out DIR".into()),
            };
            // Anything else is refused, so that a mistyped --serve-only is never
            // taken as "export by yourself".
            let mut rest = args[1..].iter();
            while let Some(a) = rest.next() {
                match a.as_str() {
                    "--out" => {
                        rest.next();
                    }
                    "--serve-only" => {}
                    other => return Err(format!("export-esp does not take `{other}`")),
                }
            }
            let service = cfg.service().ok_or("service.did is not set")?;
            let from = fsp_mint::export::folder(&dir);
            let bind = cfg.bind.clone();
            if args.iter().any(|a| a == "--serve-only") {
                // Only the running serve exports: no lock is taken and no data
                // file is opened or made here, so a caller running as another
                // account (aso's redeem job, as root) cannot leave a file in the
                // data folder that serve's account cannot write.
                println!("asking the running fsp-mint serve to export (--serve-only)");
                fsp_mint::export::ask_serve(&bind, &|e| {
                    format!(
                        "export-esp --serve-only: fsp-mint serve is not answering at {bind}: {e}. Nothing was exported and the data folder was not touched. Start serve and run this again"
                    )
                })?;
            } else {
                // A running serve holds the writer lock and has issues.json in
                // memory, so it does the export. With none running the lock is
                // taken here, so that none can start meanwhile.
                match DataLock::acquire(&dir) {
                    Ok(_lock) => {
                        let mut issues = Issues::open(&dir).map_err(|e| e.to_string())?;
                        fsp_mint::export::export_kept(
                            &mut issues,
                            &from,
                            service,
                            fsp_mint::now_secs(),
                        )?;
                    }
                    Err(LockError::Held(_)) => {
                        println!("fsp-mint serve is running: asking it to export");
                        fsp_mint::export::ask_serve(&bind, &|e| {
                            format!(
                                "the data folder is in use by another fsp-mint, but nothing answers at {bind}: {e}. If that is another `export-esp`, run this again in a moment; otherwise check that `fsp-mint serve` has the same settings"
                            )
                        })?;
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
            let written = fsp_mint::export::deliver(&from, &out)?;
            for (file, notes) in &written {
                println!("wrote {} ({notes} notes)", file.display());
            }
            if written.is_empty() {
                println!("no ESP to export");
            }
            Ok(())
        }
        "serve" => {
            // One process writes the data folder: take its lock before anything
            // is opened, and keep it until this process ends. A second serve is
            // refused here, however it is set up.
            let _lock = DataLock::acquire(&dir).map_err(|e| e.to_string())?;
            // The signals come first. From here on SIGHUP is a request to read
            // the settings again, not the end of the process; one that arrives
            // while serve is starting waits in the channel until serve is up.
            let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
            let mut hangups = listen_for_sighup(&rt)?;
            // The ESP mint is reached over AIR only: without its DID and the
            // AIR server, serve does not start.
            cfg.require_air()?;
            let keypair = load_key(&dir, &cfg)?;
            let issuer = AirIssuer::new(&cfg)?;
            let at = issuer.describe();
            let reported = issuer.network().map_err(|e| {
                format!(
                    "cannot ask the ESP mint ({at}) which network it is on: {e}. Start esp-mint serve, with its AIR listener, first"
                )
            })?;
            esp_client::check_network(&cfg.network, reported.as_deref())?;
            println!("{}", esp_client::same_network_line(&cfg.network, &at));
            let spender = Spender::open(
                &dir,
                cfg.rotation(),
                cfg.spend_max_batch as usize,
                fsp_mint::now_secs(),
            )
            .map_err(|e| e.to_string())?
            .with_max_items(cfg.spend_max_items as usize)
            .with_max_memo_bytes(cfg.spend_max_memo_bytes as usize);
            let issues = Issues::open(&dir).map_err(|e| e.to_string())?;
            let bind = cfg.bind.clone();
            // The listener answers with the service's own DID key: the one
            // `load_key` has just checked against `service.did`.
            let air = cfg.air_listener(&dir);
            let did_key = air_key(&dir)?;
            let own = ServiceDid::from(keypair.public_key());
            if addr::key_to_did(&did_key.public_key()) != own {
                return Err(format!(
                    "the AIR key made from {} is not the service's DID {own}",
                    dir.join(KEY_FILE).display()
                ));
            }
            let state = Arc::new(FspState {
                config: ConfigCell::new(cfg),
                data_dir: dir.clone(),
                spender,
                issues: Mutex::new(issues),
                issuer: Box::new(issuer),
                service_keypair: Some(keypair),
                clock: Box::new(fsp_mint::now_secs),
            });
            start_air_listener(air, did_key, Arc::clone(&state))?;
            let reloading = Arc::clone(&state);
            let settings_base = base.clone();
            rt.spawn(async move {
                while hangups.recv().await.is_some() {
                    let (state, base) = (Arc::clone(&reloading), settings_base.clone());
                    let _ =
                        tokio::task::spawn_blocking(move || reload_and_say(&state, &base)).await;
                }
            });
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind(&bind)
                    .await
                    .map_err(|e| format!("cannot bind {bind}: {e}"))?;
                println!("fsp-mint api (/audit, /health) listening on http://{bind}");
                axum::serve(listener, server::router(state))
                    .with_graceful_shutdown(async {
                        shutdown_signal().await;
                        println!("shutting down");
                    })
                    .await
                    .map_err(|e| e.to_string())
            })
        }
        other => Err(format!("unknown command `{other}`. Run with --help")),
    }
}

/// The service's key as an AIR key, for the listener: the 32 bytes of
/// `service_key.hex`.
fn air_key(dir: &Path) -> Result<sp_wire::seal::SecretKey, String> {
    let path = dir.join(KEY_FILE);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    addr::secret_from_hex(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Start the AIR listener on its own thread. It answers every wallet-facing
/// request (`server::air_served`). If it fails (the AIR client gave up) it says
/// so and the mint exits with status 1, so that the service manager starts it
/// again and the listener resumes from the stored slot counter: a mint that no
/// wallet can reach is not left running.
fn start_air_listener(
    air: air_listener::Settings,
    key: sp_wire::seal::SecretKey,
    state: Arc<FspState>,
) -> Result<(), String> {
    println!(
        "fsp-mint AIR listener: wallets reach this service with the air.fsp entry {}@{}@{} (ECASH_AIR_FSP, air.fsp; to be replaced by DID resolution)",
        addr::key_to_did(&key.public_key()),
        air.server.name.trim(),
        air.server.host.trim()
    );
    std::thread::Builder::new()
        .name("air-listener".into())
        .spawn(move || {
            let serve: Arc<air_listener::Serve> =
                Arc::new(move |kind: u8, body: &[u8]| server::air_served(&state, kind, body));
            match air_listener::run_blocking(&air, &key, &serve) {
                Ok(()) => eprintln!("AIR listener: stopped"),
                Err(e) => {
                    eprintln!("AIR listener failed: {e}\nExiting so that a supervisor can restart the process");
                    std::process::exit(1);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    println!("AIR listener starting");
    Ok(())
}

/// Turn SIGHUP into messages on a channel. The signal is registered before this
/// returns, so that none that follows ends the process.
fn listen_for_sighup(
    rt: &tokio::runtime::Runtime,
) -> Result<tokio::sync::mpsc::UnboundedReceiver<()>, String> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut hangup = rt
        .block_on(async { signal(SignalKind::hangup()) })
        .map_err(|e| format!("cannot listen for SIGHUP: {e}"))?;
    let (send, receive) = tokio::sync::mpsc::unbounded_channel();
    rt.spawn(async move {
        while hangup.recv().await.is_some() {
            if send.send(()).is_err() {
                return;
            }
        }
    });
    Ok(receive)
}

/// SIGHUP: read the settings again and say what happened. The environment is the
/// one the process started with, so it is the settings file that can change.
fn reload_and_say(state: &FspState, base: &Path) {
    let env = |n: &str| std::env::var(n).ok();
    match server::reload(state, base, &env) {
        Ok(reloaded) => {
            println!("fsp-mint: SIGHUP, settings read again:");
            for line in &reloaded.lines {
                println!("{line}");
            }
            if reloaded.changed.is_empty() {
                println!("fsp-mint: no setting that reloads live has changed");
            } else {
                println!(
                    "fsp-mint: now in use, from the next request: {}",
                    reloaded.changed.join(", ")
                );
            }
            if !reloaded.needs_restart.is_empty() {
                println!(
                    "fsp-mint: {} need a restart, which is a brief downtime, so they keep their running values until fsp-mint is restarted",
                    reloaded.needs_restart.join(", ")
                );
            }
        }
        Err(problems) => {
            eprintln!("fsp-mint: SIGHUP, the settings are not valid, so none changed:");
            for problem in problems {
                eprintln!("  {problem}");
            }
        }
    }
}

fn print_audit(r: &fsp_mint::audit::Report) {
    use fsp_mint::audit::Amount;
    let show = |a: &Amount| format!("{} units = {} PSP", a.units, a.psp);
    println!();
    println!("audit as of {} (day {})", r.now, r.today);
    println!();
    println!("FSP, per key set (outstanding = issued + change - spent):");
    for k in &r.fsp.keysets {
        println!("  {} ({})", k.keyset, k.status);
        println!("    issued      {}", show(&k.issued));
        println!("    change      {}", show(&k.change));
        println!("    spent       {}", show(&k.spent));
        println!("    outstanding {}", show(&k.outstanding));
    }
    println!(
        "FSP outstanding, live key sets: {}",
        show(&r.fsp.outstanding)
    );
    println!(
        "FSP expired (key sets deleted at rotation): {}",
        show(&r.fsp.expired)
    );
    println!("FSP issued in all: {}", show(&r.fsp.issued));
    println!("FSP paid to the service in all: {}", show(&r.fsp.paid));
    println!();
    let e = &r.esp_reserve;
    println!("ESP held in reserve (kept from issues):");
    println!(
        "  matured    {} notes, {}",
        e.matured.notes,
        show(&e.matured.esp)
    );
    println!(
        "  unmatured  {} notes, {}",
        e.unmatured.notes,
        show(&e.unmatured.esp)
    );
    println!(
        "  total      {} notes, {}",
        e.total.notes,
        show(&e.total.esp)
    );
    println!(
        "  of the total, written out by export-esp: {}",
        show(&e.total.exported_esp)
    );
    for d in &e.by_day {
        println!(
            "    redeemable after day {} ({}): {} notes, {}",
            d.redeemable_after_day,
            if d.matured { "matured" } else { "unmatured" },
            d.notes,
            show(&d.esp)
        );
    }
    for w in &r.warnings {
        println!();
        println!("WARNING: {w}");
    }
}

/// Resolves on SIGINT (Ctrl-C) or SIGTERM, which is what `launchd` and
/// `systemd` send to stop a service. The server then stops taking new
/// connections, lets the requests it is answering finish, and returns, and the
/// process exits with status 0.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).ok();
    let terminate = async {
        match term.as_mut() {
            Some(t) => {
                t.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate => {}
    }
}

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
