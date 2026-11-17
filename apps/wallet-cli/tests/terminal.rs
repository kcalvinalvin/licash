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

/// The wallet at `file` run with `args`, its stdout on a terminal when
/// `terminal`: what it printed there, on success.
#[cfg(unix)]
fn run(file: &std::path::Path, args: &[&str], terminal: bool) -> String {
    use std::{
        fs::{File, OpenOptions},
        io::Read,
        os::{fd::FromRawFd, unix::fs::OpenOptionsExt},
        process::{Command, Stdio},
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_licash-wallet"));
    command
        .arg("--wallet")
        .arg(file)
        .args(args)
        .env_remove("LICASH_WALLET_PROXY")
        .env_remove("LICASH_WALLET_LOG")
        .env_remove("RUST_LOG")
        .env("LICASH_WALLET_NETWORK", "regtest")
        .stdin(Stdio::null())
        .stderr(Stdio::piped());
    if !terminal {
        let output = command.stdout(Stdio::piped()).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        return String::from_utf8(output.stdout).unwrap();
    }
    // A pseudo-terminal: the wallet writes to its side, the test reads the
    // other, as a terminal emulator does.
    let (mut master, name) = unsafe {
        let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        assert_eq!(libc::grantpt(fd), 0);
        assert_eq!(libc::unlockpt(fd), 0);
        let name = std::ffi::CStr::from_ptr(libc::ptsname(fd));
        (File::from_raw_fd(fd), name.to_str().unwrap().to_owned())
    };
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(&name)
        .unwrap();
    let child = command.stdout(slave).spawn().unwrap();
    // The command's copy of the slave is closed now: reading ends with the
    // child's (in an error on Linux, once all is read).
    drop(command);
    let reader = std::thread::spawn(move || {
        let (mut out, mut chunk) = (Vec::new(), [0; 4096]);
        while let Ok(n @ 1..) = master.read(&mut chunk) {
            out.extend_from_slice(&chunk[..n]);
        }
        out
    });
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    // A terminal ends lines in a carriage return too.
    String::from_utf8(reader.join().unwrap())
        .unwrap()
        .replace("\r\n", "\n")
}

#[cfg(unix)]
#[test]
fn qr_codes_are_drawn_on_a_terminal_and_scripts_get_the_text_alone() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("wallet.sqlite");
    run(&file, &["init"], false);
    let address = run(&file, &["address"], false);
    assert_eq!(run(&file, &["address", "--qr"], false), address);
    assert_eq!(run(&file, &["address"], true), address);
    let drawn = run(&file, &["address", "--qr"], true);
    let drawing = drawn.strip_suffix(&address).expect(&drawn);
    // Black on white, every line, whatever the terminal's colors.
    for line in drawing.lines() {
        assert!(line.starts_with("\x1b[30;107m") && line.ends_with("\x1b[0m"));
    }
    let modules = drawing.lines().filter(|l| l.contains(['█', '▀', '▄']));
    assert!(modules.count() > 10, "{drawing}");
    assert!(run(&file, &["--json", "address", "--qr"], true).starts_with("{\"address\""));
}
