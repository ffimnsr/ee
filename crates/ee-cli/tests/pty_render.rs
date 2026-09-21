//! PTY-driven integration tests: spawn the real `ee` binary inside a pseudo
//! terminal and assert on the rendered screen through `vt100`.
//!
//! This is the end-to-end leg of the render test matrix (in-process
//! `TestBackend` snapshots cover deterministic layout; here we verify the
//! actual binary against a real TTY, including wrap-mode gutter alignment).

#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use vt100::Screen;

/// Pseudo-terminal pair; the slave becomes the editor's stdin/stdout.
fn open_pty(rows: u16, cols: u16) -> (File, File) {
    let mut master: std::ffi::c_int = 0;
    let mut slave: std::ffi::c_int = 0;
    let win = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    let rc = unsafe {
        libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &win)
    };
    assert_eq!(rc, 0, "openpty failed: {rc}");
    let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    // Push the winsize to the kernel line discipline so the app's initial
    // `ioctl(TIOCGWINSZ)` reports the test dimensions.
    let rc = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &win) };
    assert_eq!(rc, 0, "TIOCSWINSZ failed");
    (master, slave)
}

/// Spawned `ee` process; killed and reaped on drop so tests never leave
/// orphaned editors.
struct EditorProcess {
    child: Child,
}

impl Drop for EditorProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_ee(path: Option<&Path>, slave: File, home: &Path) -> EditorProcess {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ee"));
    if let Some(path) = path {
        cmd.arg(path);
    }
    cmd.stdin(Stdio::from(slave.try_clone().expect("clone slave stdin")))
        .stdout(Stdio::from(slave.try_clone().expect("clone slave stdout")))
        .stderr(Stdio::null())
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("TERM", "xterm-256color");
    unsafe {
        cmd.pre_exec(|| {
            // Adopt the pty slave as the controlling terminal.  Without this
            // the child keeps the test harness's controlling tty, and
            // `/dev/tty` (crossterm's size/event source) reports the wrong
            // winsize (0x0 or the harness terminal), breaking the render.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // stdin is already dup2'd to the pty slave when pre_exec runs.
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().expect("spawn ee binary");
    EditorProcess { child }
}

/// Reads the PTY master until EOF, forwarding every chunk to the parser
/// channel.
fn spawn_reader(mut master: File) -> mpsc::Receiver<Option<Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match master.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(None);
                    break;
                }
                Ok(n) => {
                    if tx.send(Some(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = tx.send(None);
                    break;
                }
            }
        }
    });
    rx
}

fn screen_text(screen: &Screen) -> String {
    screen.rows(0, COLS).map(|row| row.trim_end().to_string()).collect::<Vec<_>>().join("\n")
}

fn row_text(screen: &Screen, row: usize) -> String {
    screen.rows(0, COLS).nth(row).unwrap_or_default().trim_end().to_string()
}

/// Editor-area rows only (status bar excluded): deterministic across git
/// branch/dirty state, so safe to snapshot.
fn editor_rows(screen: &Screen) -> String {
    (0..ROWS as usize - 2).map(|row| row_text(screen, row)).collect::<Vec<_>>().join("\n")
}

/// Whitespace tokens of a row: `sign? gutter text...`.  The git sign column
/// prefixes added-line markers (`+`) when enabled, so assertions must be
/// token-based rather than column-based.
fn tokens(row: &str) -> Vec<String> {
    row.split_whitespace().map(str::to_owned).collect()
}

/// Drop the optional git sign token (`+`) from the front of the row tokens.
fn strip_sign(t: &[String]) -> Vec<String> {
    if t.first().map(String::as_str) == Some("+") { t[1..].to_vec() } else { t.to_vec() }
}

/// Feed PTY chunks into `parser` until `pred` holds, then return.  Panics
/// with the screen contents on timeout or EOF.
fn wait_for_screen(
    rx: &mpsc::Receiver<Option<Vec<u8>>>,
    parser: &mut vt100::Parser,
    timeout: Duration,
    what: &str,
    mut pred: impl FnMut(&Screen) -> bool,
) {
    let deadline = Instant::now() + timeout;
    loop {
        if pred(parser.screen()) {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            panic!("timeout waiting for {what};\nscreen:\n{}", screen_text(parser.screen()));
        }
        match rx.recv_timeout(deadline - now) {
            Ok(Some(bytes)) => parser.process(&bytes),
            Ok(None) => panic!(
                "pty closed while waiting for {what};\nscreen:\n{}",
                screen_text(parser.screen())
            ),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                "reader thread lost while waiting for {what};\nscreen:\n{}",
                screen_text(parser.screen())
            ),
        }
    }
}

/// Wait until the screen shows `pred` on three consecutive polls ~200ms
/// apart.  The editor repaints every input-poll tick, so a byte-idle screen
/// never occurs; consecutive stable reads avoid a mid-frame snapshot.
fn wait_stable(
    rx: &mpsc::Receiver<Option<Vec<u8>>>,
    parser: &mut vt100::Parser,
    timeout: Duration,
    what: &str,
    mut pred: impl FnMut(&Screen) -> bool,
) {
    let deadline = Instant::now() + timeout;
    let mut consecutive = 0u32;
    loop {
        if pred(parser.screen()) {
            consecutive += 1;
            if consecutive >= 3 {
                return;
            }
        } else {
            consecutive = 0;
        }
        let now = Instant::now();
        if now >= deadline {
            panic!("timeout waiting for stable {what};\nscreen:\n{}", screen_text(parser.screen()));
        }
        match rx.recv_timeout((deadline - now).min(Duration::from_millis(200))) {
            Ok(Some(bytes)) => parser.process(&bytes),
            Ok(None) => panic!(
                "pty closed while waiting for stable {what};\nscreen:\n{}",
                screen_text(parser.screen())
            ),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                "reader thread lost while waiting for stable {what};\nscreen:\n{}",
                screen_text(parser.screen())
            ),
        }
    }
}

const COLS: u16 = 80;
const ROWS: u16 = 24;

fn hello_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join("test_assets/hello.txt")
}

fn setup()
-> (EditorProcess, File, mpsc::Receiver<Option<Vec<u8>>>, vt100::Parser, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("temp dir");
    let (master, slave) = open_pty(ROWS, COLS);
    let proc = spawn_ee(Some(&hello_path()), slave, temp.path());
    let rx = spawn_reader(master.try_clone().expect("clone master reader"));
    let parser = vt100::Parser::new(ROWS, COLS, 0);
    (proc, master, rx, parser, temp)
}

/// Write keystrokes to the editor's stdin via the PTY master.
fn send_keys(master: &mut File, keys: &[u8]) {
    master.write_all(keys).expect("write keys to pty");
    master.flush().expect("flush pty");
}

#[test]
fn pty_hello_opens_with_cat_n_gutter() {
    let (_proc, _master, rx, mut parser, _temp) = setup();

    // Initial render: 9 unwrapped logical lines, cat -n style gutters.
    wait_for_screen(&rx, &mut parser, Duration::from_secs(20), "initial render", |screen| {
        row_text(screen, 0).contains("Lorem ipsum")
            && row_text(screen, 2).contains("Ut ultrices")
            && row_text(screen, 4).contains("Sed rhoncus")
            && row_text(screen, 8).contains("Nunc ut felis")
            && row_text(screen, ROWS as usize - 2).contains("hello.txt")
    });

    let screen = parser.screen();
    for (row, number, starts) in
        [(0usize, "1", "Lorem ipsum"), (2, "3", "Ut ultrices"), (4, "5", "Sed rhoncus")]
    {
        let t = strip_sign(&tokens(&row_text(screen, row)));
        assert_eq!(
            t.first().map(String::as_str),
            Some(number),
            "row {row} must carry gutter {number}, got {t:?}"
        );
        let text = t[1..].join(" ");
        assert!(text.starts_with(starts), "row {row} must start with {starts:?}, got {text:?}");
    }
    // Blank lines carry their number and nothing else.
    assert_eq!(strip_sign(&tokens(&row_text(screen, 1))), ["2"], "blank line gutter row");
    assert_eq!(strip_sign(&tokens(&row_text(screen, 3))), ["4"], "blank line gutter row");
    // No wrap flag before `set wrap`.
    let status = row_text(screen, ROWS as usize - 2);
    assert!(status.contains("hello.txt"), "status row: {status:?}");
    assert!(!status.contains("wrap"), "wrap flag must be off: {status:?}");
}

#[test]
fn pty_wrap_blanks_continuation_rows_and_stays_aligned() {
    let (_proc, mut master, rx, mut parser, _temp) = setup();
    wait_for_screen(&rx, &mut parser, Duration::from_secs(20), "initial render", |screen| {
        row_text(screen, 0).contains("Lorem ipsum")
            && row_text(screen, 8).contains("Nunc ut felis")
            && row_text(screen, ROWS as usize - 2).contains("hello.txt")
    });

    send_keys(&mut master, b":set wrap\r");
    wait_for_screen(&rx, &mut parser, Duration::from_secs(20), "wrap re-render", |screen| {
        let status = row_text(screen, ROWS as usize - 2);
        status.contains("wrap")
            && row_text(screen, 0).contains("Lorem ipsum")
            && (1..ROWS as usize - 2)
                .any(|row| strip_sign(&tokens(&row_text(screen, row))) == ["2"])
            && (1..ROWS as usize - 2).any(|row| {
                let t = strip_sign(&tokens(&row_text(screen, row)));
                t.join(" ").starts_with("3 Ut ultrices")
            })
    });

    // The blank logical line must sit at a numbered row whose text is empty.
    let blank_row = (1..ROWS as usize - 2)
        .find(|row| strip_sign(&tokens(&row_text(parser.screen(), *row))) == ["2"])
        .unwrap();
    assert!(blank_row > 4, "long first line must wrap into several rows, got {blank_row}");
    // Continuation rows above it carry no gutter number (sign + text only).
    for row in 1..blank_row {
        let t = strip_sign(&tokens(&row_text(parser.screen(), row)));
        assert!(
            !t.first().is_some_and(|tok| tok.chars().all(|c| c.is_ascii_digit())),
            "continuation row {row} must have a blank gutter, got {t:?}"
        );
    }
    // The row after the blank carries its number AND its text on the same row.
    let t = strip_sign(&tokens(&row_text(parser.screen(), blank_row + 1)));
    assert!(
        t.join(" ").starts_with("3 Ut ultrices"),
        "row after blank must be `3 Ut ultrices...`, got {t:?}"
    );

    // Cursor onto the blank line: gutter and text must stay aligned.  Also
    // wait out the transient `set: wrap=true` notification toast so the
    // snapshot below is deterministic.
    send_keys(&mut master, &b"j".repeat(blank_row));
    wait_stable(
        &rx,
        &mut parser,
        Duration::from_secs(20),
        "cursor on blank line, toast expired",
        |screen| {
            strip_sign(&tokens(&row_text(screen, blank_row))) == ["2"]
                && strip_sign(&tokens(&row_text(screen, blank_row + 1)))
                    .join(" ")
                    .starts_with("3 Ut ultrices")
                && row_text(screen, ROWS as usize - 2).contains(&format!("Ln {}", blank_row + 1))
                && !(0..ROWS as usize - 2).any(|row| row_text(screen, row).contains("notification"))
        },
    );
    let t = strip_sign(&tokens(&row_text(parser.screen(), blank_row + 1)));
    assert!(
        t.join(" ").starts_with("3 Ut ultrices"),
        "cursor on blank line must keep alignment, got {t:?}"
    );

    // Editor-area snapshot for review: wrap gutter + `wrap` flag status.
    insta::assert_snapshot!(editor_rows(parser.screen()));
}

#[test]
fn pty_git_sign_absent_for_tracked_clean_file() {
    // `test_assets/hello.txt` is gitignored (`*.txt`), so the app correctly
    // shows it as an all-added file.  A tracked, clean file must show no git
    // signs in the gutter; capture it through the same PTY path for review.
    let temp = tempfile::tempdir().expect("temp dir");
    let (master, slave) = open_pty(ROWS, COLS);
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("test_assets/sample-program.rs");
    let _proc = spawn_ee(Some(&path), slave, temp.path());
    let rx = spawn_reader(master.try_clone().expect("clone master reader"));
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);

    wait_for_screen(&rx, &mut parser, Duration::from_secs(20), "sample-program render", |screen| {
        row_text(screen, 0).contains("use std::collections")
            && row_text(screen, 9).contains("use std::time")
    });
    // Source-control refresh runs ~250ms after startup idle; the large
    // fixture is never fully cached, so no git status is ever cached for it —
    // wait out the refresh window and assert both the missing badge and the
    // absence of phantom gutter signs.
    std::thread::sleep(Duration::from_millis(1500));
    wait_stable(&rx, &mut parser, Duration::from_secs(10), "git refresh settled", |_| true);

    let status = row_text(parser.screen(), ROWS as usize - 2);
    assert!(
        !status.contains("git:"),
        "partially-cached file must not cache a git status, got {status:?}"
    );
    for row in 0..ROWS as usize - 2 {
        let t = tokens(&row_text(parser.screen(), row));
        assert!(
            !matches!(t.first().map(String::as_str), Some("+" | "-" | "~")),
            "row {row} must have no git sign when the file is clean, got {t:?}"
        );
    }
    insta::assert_snapshot!(editor_rows(parser.screen()));
}
