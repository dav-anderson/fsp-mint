//! fsp-mint command line: `config`, `service-key`, `serve`, `export-esp`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use fsp_mint::config::{self, Config};
use fsp_mint::esp_client::{self, HttpIssuer};
use fsp_mint::issue::Issues;
use fsp_mint::server::{self, FspState};
use fsp_mint::spend::Spender;
use fsp_mint::store;
use secp256k1::{Keypair, SECP256K1, SecretKey};

const KEY_FILE: &str = "service_key.hex";

const USAGE: &str = "\
fsp-mint <command>

  config               Print the settings in use, and what each does.
  service-key          Make the service's DID key (service_key.hex) if there is
                       none, and print its public half for service.did.
  serve                Run the HTTP API (POST /issue, POST /spend, GET /keys,
                       GET /fees, GET /rotation, GET /health).
  export-esp --out DIR Write the ESP this mint has kept to DIR, one hand-over
                       file per redeemable-after day, for the service's
                       ecash-wallet to `receive` and `redeem`. Stop serve first.

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

fn load_key(dir: &Path, cfg: &Config) -> Result<Keypair, String> {
    let path = dir.join(KEY_FILE);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "cannot read {}: {e}. Run `fsp-mint service-key` first",
            path.display()
        )
    })?;
    let sk = hex::decode(text.trim())
        .ok()
        .and_then(|b| SecretKey::from_slice(&b).ok())
        .ok_or_else(|| format!("{} is damaged", path.display()))?;
    let kp = Keypair::from_secret_key(SECP256K1, &sk);
    match cfg.service() {
        Some(did) if did == kp.x_only_public_key().0 => Ok(kp),
        Some(did) => Err(format!(
            "service.did is {did}, but {} holds the key for {}. They must match",
            path.display(),
            kp.x_only_public_key().0
        )),
        None => Err(format!(
            "service.did is not set. Set it to {} (the key in {})",
            kp.x_only_public_key().0,
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
    rot.first_issue = fsp_mint::keysets::load_first_issue(&dir).map_err(|e| e.to_string())?;
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
                println!("{} ({}, default `{}`): {}", s.key, s.env, s.default, s.what);
            }
            Ok(())
        }
        "service-key" => {
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let path = dir.join(KEY_FILE);
            if !path.exists() {
                let kp = Keypair::new(SECP256K1, &mut rand::rngs::OsRng);
                store::write_secret(&path, &hex::encode(kp.secret_key().secret_bytes()))
                    .map_err(|e| e.to_string())?;
                println!("made {}", path.display());
            }
            let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let sk = hex::decode(text.trim())
                .ok()
                .and_then(|b| SecretKey::from_slice(&b).ok())
                .ok_or_else(|| format!("{} is damaged", path.display()))?;
            println!(
                "service.did = {}",
                Keypair::from_secret_key(SECP256K1, &sk)
                    .x_only_public_key()
                    .0
            );
            Ok(())
        }
        "export-esp" => {
            let out = match args.iter().position(|a| a == "--out") {
                Some(i) if i + 1 < args.len() => PathBuf::from(&args[i + 1]),
                _ => return Err("export-esp needs --out DIR".into()),
            };
            let service = cfg.service().ok_or("service.did is not set")?;
            let mut issues = Issues::open(&dir).map_err(|e| e.to_string())?;
            let mut by_day: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
            for (i, k) in issues.data.kept.iter().enumerate() {
                if !k.exported {
                    by_day.entry(k.redeemable_after_day).or_default().push(i);
                }
            }
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            for (day, idx) in &by_day {
                let notes: Vec<serde_json::Value> = idx
                    .iter()
                    .map(|&i| {
                        let k = &issues.data.kept[i];
                        serde_json::json!({"amount": k.amount, "note": k.note, "spend_secret": k.spend_secret, "keyset": k.keyset})
                    })
                    .collect();
                let file = out.join(format!("esp-{day}-{}.json", fsp_mint::now_secs()));
                let body = serde_json::json!({"service": service, "redeemable_after_day": day, "notes": notes});
                store::save(&file, &body).map_err(|e| e.to_string())?;
                println!("wrote {} ({} notes)", file.display(), idx.len());
            }
            for idx in by_day.values() {
                for &i in idx {
                    issues.data.kept[i].exported = true;
                }
            }
            issues.save().map_err(|e| e.to_string())?;
            if by_day.is_empty() {
                println!("no ESP to export");
            }
            Ok(())
        }
        "serve" => {
            let keypair = load_key(&dir, &cfg)?;
            let issuer = HttpIssuer::new(&cfg.esp_url, cfg.esp_timeout_secs)?;
            let reported = issuer
                .network()
                .map_err(|e| format!("cannot ask the ESP mint at {} which network it is on: {e}. Start esp-mint serve first", cfg.esp_url))?;
            esp_client::check_network(&cfg.network, reported.as_deref())?;
            println!(
                "{}",
                esp_client::same_network_line(&cfg.network, &cfg.esp_url)
            );
            let spender = Spender::open(
                &dir,
                cfg.rotation(),
                cfg.spend_max_batch as usize,
                fsp_mint::now_secs(),
            )
            .map_err(|e| e.to_string())?
            .with_max_items(cfg.spend_max_items as usize);
            let issues = Issues::open(&dir).map_err(|e| e.to_string())?;
            let bind = cfg.bind.clone();
            let state = Arc::new(FspState {
                config: cfg,
                spender,
                issues: Mutex::new(issues),
                issuer: Box::new(issuer),
                service_keypair: Some(keypair),
                clock: Box::new(fsp_mint::now_secs),
            });
            let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind(&bind)
                    .await
                    .map_err(|e| format!("cannot bind {bind}: {e}"))?;
                println!("fsp-mint api listening on http://{bind}");
                axum::serve(listener, server::router(state))
                    .await
                    .map_err(|e| e.to_string())
            })
        }
        other => Err(format!("unknown command `{other}`. Run with --help")),
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
