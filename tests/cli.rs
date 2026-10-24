//! The command line's messages to the operator, through the real `fsp-mint`
//! program: what it says, and that it exits with a failure, when a command is
//! unknown, the settings file is wrong, `export-esp` is missing something, or the
//! data cannot be read. None of these needs an AIR server.

mod common;

use common::{fsp_run, scratch};
use fsp_mint::lock::DataLock;

/// What the program printed to stderr, as text.
fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn an_unknown_command_is_refused_and_no_command_prints_the_usage() {
    let base = scratch("cli-unknown");
    let out = fsp_run(&base, &["x"], &[]);
    assert!(!out.status.success());
    assert_eq!(
        stderr(&out),
        "error: unknown command `x`. Run with --help\n"
    );

    // No command, or --help: the usage on stdout, and success.
    let none = fsp_run(&base, &[], &[]);
    assert!(none.status.success(), "{}", stderr(&none));
    let usage = String::from_utf8_lossy(&none.stdout).into_owned();
    assert!(usage.starts_with("fsp-mint <command>\n"), "{usage}");
    for command in [
        "  config ",
        "  service-key ",
        "  serve ",
        "  audit [--json]",
        "  export-esp --out DIR [--serve-only]",
    ] {
        assert!(usage.contains(command), "{command} is not in:\n{usage}");
    }
    assert_eq!(stderr(&none), "");
    let help = fsp_run(&base, &["--help"], &[]);
    assert!(help.status.success(), "{}", stderr(&help));
    assert_eq!(help.stdout, none.stdout);
}

#[test]
fn config_refuses_a_damaged_settings_file_and_an_unknown_setting() {
    // A file that is not TOML.
    let base = scratch("cli-damaged");
    let file = base.join("fsp-mint.toml");
    std::fs::write(&file, "[server\nbind = ").unwrap();
    let out = fsp_run(&base, &["config"], &[]);
    let err = stderr(&out);
    assert!(!out.status.success(), "config accepted it");
    assert!(err.contains("fsp-mint settings are not valid"), "{err}");
    assert!(
        err.contains(&format!("{}: not valid TOML: ", file.display())),
        "{err}"
    );
    assert!(out.stdout.is_empty(), "nothing is printed before the check");

    // A setting that does not exist, named in the message.
    let base = scratch("cli-unknown-setting");
    let file = base.join("fsp-mint.toml");
    std::fs::write(&file, "[server]\nnope = 1\n").unwrap();
    let out = fsp_run(&base, &["config"], &[]);
    let err = stderr(&out);
    assert!(!out.status.success(), "config accepted it");
    assert!(err.contains("fsp-mint settings are not valid"), "{err}");
    assert!(
        err.contains(&format!(
            "{}: unknown setting `server.nope`",
            file.display()
        )),
        "{err}"
    );
    assert!(out.stdout.is_empty(), "nothing is printed before the check");
}

#[test]
fn export_esp_needs_an_out_folder_and_the_service_did() {
    let base = scratch("cli-export");
    // No --out, and --out with nothing after it.
    for args in [&["export-esp"][..], &["export-esp", "--out"][..]] {
        let out = fsp_run(&base, args, &[]);
        assert!(!out.status.success(), "{args:?}");
        assert_eq!(
            stderr(&out),
            "error: export-esp needs --out DIR\n",
            "{args:?}"
        );
    }
    // --out, but no service.did: refused before the folder is made.
    let folder = base.join("handover");
    let folder_arg = folder.to_str().unwrap();
    let out = fsp_run(&base, &["export-esp", "--out", folder_arg], &[]);
    assert!(!out.status.success());
    assert_eq!(stderr(&out), "error: service.did is not set\n");
    assert!(!folder.exists(), "the folder was made");
}

/// `service-key`: an empty `service_key.hex` (an older fsp-mint could leave one
/// if it stopped while making it) never held a key, so it is made again, whole
/// and owner-only; a file holding part of a key is refused and left as it is.
#[test]
fn service_key_makes_an_empty_key_file_again_and_never_replaces_a_damaged_one() {
    let base = scratch("cli-key");
    let dir = base.join("main");
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("service_key.hex");
    std::fs::write(&key, "").unwrap();
    let out = fsp_run(&base, &["service-key"], &[]);
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{}", stderr(&out));
    // After the settings it prints, the last three lines.
    let tail: Vec<&str> = said.lines().rev().take(3).collect();
    assert_eq!(
        tail[2],
        format!("{} was empty: making the key again", key.display())
    );
    assert_eq!(tail[1], format!("made {}", key.display()));
    assert!(tail[0].starts_with("service.did = 0"), "{said}");
    assert_eq!(tail[0].len(), "service.did = ".len() + 66);
    // Fails if the key is not whole, a temporary file is left, or others can read it.
    assert_eq!(std::fs::read_to_string(&key).unwrap().len(), 64);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // The same key on a second run: nothing is made.
    let again = fsp_run(&base, &["service-key"], &[]);
    assert!(again.status.success());
    let again = String::from_utf8_lossy(&again.stdout).into_owned();
    assert_eq!(again.lines().last(), Some(tail[0]));
    assert!(!again.contains("made "), "{again}");

    std::fs::write(&key, "0123abcd").unwrap();
    let out = fsp_run(&base, &["service-key"], &[]);
    assert!(!out.status.success(), "a damaged key was taken");
    assert!(
        stderr(&out).starts_with(&format!(
            "error: {} is damaged: it does not hold a whole key (64 hex digits). fsp-mint does not replace it",
            key.display()
        )),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read_to_string(&key).unwrap(), "0123abcd");
}

#[test]
fn audit_says_so_when_the_data_cannot_be_read() {
    // issues.json is damaged, so the data of the network cannot be read.
    let base = scratch("cli-audit");
    let dir = base.join("regtest");
    std::fs::create_dir_all(&dir).unwrap();
    let issues = dir.join("issues.json");
    std::fs::write(&issues, "not json").unwrap();
    let out = fsp_run(&base, &["audit"], &[("FSP_NETWORK", "regtest")]);
    let err = stderr(&out);
    assert!(!out.status.success(), "audit read it");
    assert!(
        err.starts_with(&format!(
            "error: cannot read the data in {}: {} is damaged: ",
            dir.display(),
            issues.display()
        )),
        "{err}"
    );
}

#[test]
fn audit_names_the_data_folder_when_a_file_is_where_it_should_be() {
    // A file where the network's data folder should be: the data cannot be read,
    // and the message says where, not just "Not a directory".
    let base = scratch("cli-audit-file");
    let dir = base.join("regtest");
    std::fs::write(&dir, "not a folder").unwrap();
    let out = fsp_run(&base, &["audit"], &[("FSP_NETWORK", "regtest")]);
    let err = stderr(&out);
    assert!(!out.status.success(), "audit read it");
    assert!(
        err.starts_with(&format!(
            "error: cannot read the data in {}: ",
            dir.display()
        )),
        "{err}"
    );
}

/// One process writes a data folder. The lock is taken before anything else is
/// opened, so a second `serve` is refused whatever its settings are, and it
/// touches nothing. Once the lock is free, `serve` goes on to its own checks.
#[test]
fn a_second_serve_on_the_same_data_folder_is_refused_before_anything_else() {
    let base = scratch("cli-lock");
    let data = base.join("main");
    let held = DataLock::acquire(&data).unwrap();
    let out = fsp_run(&base, &["serve"], &[]);
    let err = stderr(&out);
    assert!(!out.status.success(), "a second serve started");
    assert!(
        err.starts_with("error: another fsp-mint is already running on this data folder"),
        "{err}"
    );
    assert!(
        err.contains(&format!(
            "({} is locked)",
            data.join("fsp-mint.lock").display()
        )),
        "{err}"
    );
    // Nothing but the lock file is in the folder.
    assert_eq!(std::fs::read_dir(&data).unwrap().count(), 1);

    // The same lock held by `export-esp`'s own export, or by any process, refuses
    // the same way; a lock that is let go (as a process that ends lets it go) does not.
    drop(held);
    let out = fsp_run(&base, &["serve"], &[]);
    let err = stderr(&out);
    assert!(!out.status.success());
    assert!(err.contains("serve needs the AIR settings"), "{err}");
    assert!(!err.contains("already running"), "{err}");
}

/// `config` says, for every setting, whether a change reloads live (SIGHUP) or
/// needs a restart.
#[test]
fn config_says_for_every_setting_whether_it_reloads_live_or_needs_a_restart() {
    let base = scratch("cli-config-reload");
    let out = fsp_run(&base, &["config"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let kind = |key: &str| -> String {
        let line = text
            .lines()
            .find(|l| l.starts_with(&format!("{key} (")))
            .unwrap_or_else(|| panic!("no line for {key} in:\n{text}"));
        if line.contains(", reloads live): ") {
            "live".into()
        } else if line.contains(", needs a restart): ") {
            "restart".into()
        } else {
            panic!("{line}")
        }
    };
    let live = [
        "server.max_request_bytes",
        "fees.issue_fee_per_byte",
        "issue.max_items",
        "spend.max_items",
        "spend.max_memo_bytes",
        "spend.max_batch",
        "esp.keyset_ttl_secs",
    ];
    let restart = [
        "server.network",
        "server.bind",
        "service.did",
        "rotation.activation_hours",
        "rotation.period_hours",
        "air.esp_did",
        "air.esp_server",
        "air.esp_host",
        "air.server",
        "air.host",
        "air.held_reads",
        "air.reply_wait_secs",
        "air.attempts",
        "air.max_skew_secs",
        "air.timeout_secs",
        "storage.mint_state",
    ];
    for key in live {
        assert_eq!(kind(key), "live", "{key}");
    }
    for key in restart {
        assert_eq!(kind(key), "restart", "{key}");
    }
    // No setting is left out of the two lists.
    assert_eq!(live.len() + restart.len(), fsp_mint::config::SPECS.len());
}
