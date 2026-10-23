//! The command line's messages to the operator, through the real `fsp-mint`
//! program: what it says, and that it exits with a failure, when a command is
//! unknown, the settings file is wrong, `export-esp` is missing something, or the
//! data cannot be read. None of these needs an AIR server.

mod common;

use common::{fsp_run, scratch};

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
        "  export-esp --out DIR",
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
