#[test]
fn terminal_errors_do_not_execute_control_sequences_from_paths() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing\u{1b}[2J.sqlite");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_licash-wallet"))
        .arg("--wallet")
        .arg(path)
        .arg("balance")
        .env_remove("RUST_LOG")
        .env_remove("LICASH_WALLET_LOG")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let text = String::from_utf8(output.stderr).unwrap();
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(text.contains("\\u{1b}[2J"), "{text:?}");
}
