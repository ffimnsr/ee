//! PTY-driven real-fixture check for the VLF "stuck on Loading" bug:
//! `G` while the index is still scanning (inexact line count), then bursts of
//! fast page scrolls, must never leave the visible rows on `Loading…`.
//!
//! Runs against a real fixture (default `test_assets/vbig-100.txt`, override
//! with `EE_VLF_REPRO_ASSET`). Marked `ignore` like the other large-fixture
//! checks: it needs the fixture and a few seconds.

#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use vt100::{Parser, Screen};

const COLS: u16 = 100;
const ROWS: u16 = 40;

fn open_pty(rows: u16, cols: u16) -> (File, File) {
    let mut master: std::ffi::c_int = 0;
    let mut slave: std::ffi::c_int = 0;
    let mut win = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(win),
        )
    };
    assert_eq!(rc, 0, "openpty failed: {rc}");
    let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    let rc = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &win) };
    assert_eq!(rc, 0, "TIOCSWINSZ failed");
    (master, slave)
}

struct EditorProcess {
    child: Child,
}

impl Drop for EditorProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_ee(path: &Path, slave: File, home: &Path) -> EditorProcess {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ee"));
    cmd.arg(path);
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
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().expect("spawn ee binary");
    EditorProcess { child }
}

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

fn loading_rows(screen: &Screen) -> Vec<usize> {
    (0..ROWS as usize - 2)
        .filter(|row| row_text(screen, *row).contains("Loading"))
        .collect::<Vec<_>>()
}

fn pump(rx: &mpsc::Receiver<Option<Vec<u8>>>, parser: &mut Parser, how_long: Duration) {
    let deadline = Instant::now() + how_long;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        match rx.recv_timeout((deadline - now).min(Duration::from_millis(50))) {
            Ok(Some(bytes)) => parser.process(&bytes),
            Ok(None) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn wait_until(
    rx: &mpsc::Receiver<Option<Vec<u8>>>,
    parser: &mut Parser,
    timeout: Duration,
    what: &str,
    mut pred: impl FnMut(&Screen) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if pred(parser.screen()) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            eprintln!("TIMEOUT waiting for {what};\nscreen:\n{}\n", screen_text(parser.screen()));
            return false;
        }
        match rx.recv_timeout((deadline - now).min(Duration::from_millis(100))) {
            Ok(Some(bytes)) => parser.process(&bytes),
            Ok(None) => return false,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn send_keys(master: &mut File, keys: &[u8]) {
    master.write_all(keys).expect("write keys to pty");
    master.flush().expect("flush pty");
}

const PAGE_UP: &[u8] = b"\x1b[5~";
const PAGE_DOWN: &[u8] = b"\x1b[6~";

#[test]
#[ignore = "manual real-fixture check; requires a vbig asset"]
fn vlf_fast_scroll_after_goto_end_keeps_rows_loaded() {
    let asset = std::env::var("EE_VLF_REPRO_ASSET")
        .unwrap_or_else(|_| String::from("test_assets/vbig-100.txt"));
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(asset);
    assert!(path.exists(), "missing asset {}", path.display());

    let temp = tempfile::tempdir().expect("temp dir");
    let (master, slave) = open_pty(ROWS, COLS);
    let _proc = spawn_ee(&path, slave, temp.path());
    let rx = spawn_reader(master.try_clone().expect("clone master reader"));
    let mut parser = Parser::new(ROWS, COLS, 0);
    let mut master = master;

    // 1. Initial viewport renders rows.
    assert!(
        wait_until(&rx, &mut parser, Duration::from_secs(60), "initial rows", |screen| {
            !row_text(screen, 0).is_empty() && loading_rows(screen).is_empty()
        }),
        "initial render never produced rows"
    );

    // 2. Goto end while the index is (typically) still scanning, then burst
    // page scrolls before the tail chase can settle.
    send_keys(&mut master, b"G");
    pump(&rx, &mut parser, Duration::from_millis(120));
    for _ in 0..3 {
        send_keys(&mut master, &PAGE_UP.repeat(15));
        pump(&rx, &mut parser, Duration::from_millis(40));
        send_keys(&mut master, &PAGE_DOWN.repeat(15));
        pump(&rx, &mut parser, Duration::from_millis(40));
    }

    // 3. The editor must drain every queued render: rows may be briefly
    // `Loading`, but they must never stay that way.
    pump(&rx, &mut parser, Duration::from_secs(8));
    let after_scroll = screen_text(parser.screen());
    let scroll_loading = loading_rows(parser.screen());
    assert!(
        scroll_loading.is_empty(),
        "rows stayed on Loading after the scroll burst:\n{after_scroll}"
    );

    // 4. Jump to the top: it must re-render too.
    send_keys(&mut master, b"gg");
    pump(&rx, &mut parser, Duration::from_secs(4));
    let after_top = screen_text(parser.screen());
    let top_loading = loading_rows(parser.screen());
    assert!(top_loading.is_empty(), "rows stayed on Loading after `gg`:\n{after_top}");
}
