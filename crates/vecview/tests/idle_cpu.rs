//! Runs the real `vv` in a pseudo-terminal and checks it stays quiet when left alone.
//!
//! A unit test can check a condition, but only the running event loop can show that nothing keeps
//! waking it. A spinning loop reads about 100% here and a healthy one about 0%, so the threshold has
//! plenty of room on both sides. Skipped where libpdfium isn't available, as in CI.
#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_IDLE_CPU_PERCENT: f64 = 50.0;
const COPY_MODE_HINT: &str = "-- COPY:";
const HELP_MARKER: &str = "vecview - keyboard shortcuts";
const IMAGE_START: &str = "\x1b_G";
const ZOOM_IN: &[u8] = b"+";
/// SGR mouse report: left button pressed and released on the same cell.
const SINGLE_CLICK: &[u8] = b"\x1b[<0;20;6M\x1b[<0;20;6m";

/// A one-page PDF with a line of text, so copy mode has something to select.
fn minimal_pdf() -> Vec<u8> {
    let content = b"BT /F1 24 Tf 40 200 Td (Hello vecview idle test) Tj ET";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 300] /Contents 4 0 R \
         /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_string(),
        format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            content.len(),
            String::from_utf8_lossy(content)
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        write!(pdf, "{} 0 obj\n{object}\nendobj\n", i + 1).unwrap();
    }
    let xref = pdf.len();
    write!(pdf, "xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).unwrap();
    for offset in offsets {
        writeln!(pdf, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        pdf,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    )
    .unwrap();
    pdf
}

/// A scratch directory holding the test PDF, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("vv-idle-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t.pdf"), minimal_pdf()).unwrap();
        Self(dir)
    }

    fn pdf(&self) -> PathBuf {
        self.0.join("t.pdf")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn pdfium_available(scratch: &Scratch) -> bool {
    Command::new(env!("CARGO_BIN_EXE_vv"))
        .arg("--render")
        .args(["--size", "16x16"])
        .arg("--output")
        .arg(scratch.0.join("probe.png"))
        .arg(scratch.pdf())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `vv` running on a pseudo-terminal, with everything it prints collected.
struct Session {
    child: Child,
    master: File,
    output: Arc<Mutex<Vec<u8>>>,
}

impl Session {
    fn start(pdf: &Path) -> Self {
        // SAFETY: plain libc calls on a descriptor owned here; each result is checked.
        let (master, slave) = unsafe {
            let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(fd >= 0, "posix_openpt");
            let master = File::from_raw_fd(fd);
            assert_eq!(libc::grantpt(fd), 0, "grantpt");
            assert_eq!(libc::unlockpt(fd), 0, "unlockpt");
            let mut name = [0 as libc::c_char; 128];
            assert_eq!(libc::ptsname_r(fd, name.as_mut_ptr(), name.len()), 0, "ptsname_r");
            let path = std::ffi::CStr::from_ptr(name.as_ptr()).to_str().unwrap();
            let slave = File::options().read(true).write(true).open(path).unwrap();
            let size = libc::winsize { ws_row: 24, ws_col: 80, ws_xpixel: 640, ws_ypixel: 480 };
            assert_eq!(libc::ioctl(fd, libc::TIOCSWINSZ, &size), 0, "TIOCSWINSZ");
            (master, slave)
        };

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vv"));
        cmd.args(["-b", "herdr"])
            .arg(pdf)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        // SAFETY: setsid and ioctl are async-signal-safe, as pre_exec requires.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().expect("failed to start vv");
        drop(slave);

        // Keep reading so vv never blocks on a full terminal buffer.
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        let mut reader = master.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Self { child, master, output }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }

    fn output_len(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    /// Wait until `predicate` holds, failing the test rather than measuring the wrong state.
    fn wait_until(&self, what: &str, predicate: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !predicate(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_for_text(&self, text: &str) {
        self.wait_until(text, |s| {
            let output = s.output.lock().unwrap();
            output.windows(text.len()).any(|w| w == text.as_bytes())
        });
    }

    /// Send a key that redraws, returning as soon as the terminal starts receiving the new image.
    fn send_and_wait_for_draw(&mut self, bytes: &[u8]) {
        let before = self.output_len();
        self.send(bytes);
        self.wait_until("the redraw", |s| s.output_len() > before);
    }

    fn cpu_percent(&self, over: Duration) -> f64 {
        let ticks = || {
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.child.id())).unwrap();
            let fields: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..].split(' ').collect();
            fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
        };
        // SAFETY: sysconf only reads a system constant.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        let before = ticks();
        std::thread::sleep(over);
        (ticks() - before) as f64 / hz / over.as_secs_f64() * 100.0
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Bring `vv` up, put it in a state with `arrange`, and require it to stop using CPU there.
fn assert_quiet(name: &str, arrange: impl FnOnce(&mut Session)) {
    let scratch = Scratch::new(name);
    if !pdfium_available(&scratch) {
        eprintln!("skipping {name}: vv can't render a PDF here (no libpdfium)");
        return;
    }
    let mut session = Session::start(&scratch.pdf());
    session.wait_for_text(IMAGE_START);
    // Longer than the idle prefetch delay, so startup's own warming is over.
    std::thread::sleep(Duration::from_millis(500));
    arrange(&mut session);
    std::thread::sleep(Duration::from_millis(500));
    let cpu = session.cpu_percent(Duration::from_secs(2));
    assert!(cpu < MAX_IDLE_CPU_PERCENT, "{name}: vv used {cpu:.1}% CPU while left alone");
}

#[test]
fn quiet_after_startup() {
    assert_quiet("startup", |_| {});
}

#[test]
fn quiet_in_copy_mode_entered_by_key_after_zoom() {
    assert_quiet("copy-key", |s| {
        s.send_and_wait_for_draw(ZOOM_IN);
        std::thread::sleep(Duration::from_millis(300));
        s.send(b"y");
        s.wait_for_text(COPY_MODE_HINT);
    });
}

#[test]
fn quiet_in_copy_mode_entered_by_click_after_zoom() {
    assert_quiet("copy-click", |s| {
        s.send_and_wait_for_draw(ZOOM_IN);
        std::thread::sleep(Duration::from_millis(300));
        s.send(SINGLE_CLICK);
        s.wait_for_text(COPY_MODE_HINT);
    });
}

#[test]
fn quiet_in_help_opened_right_after_a_draw() {
    assert_quiet("help", |s| {
        // Immediately after the redraw starts, so the prefetch it sets up is still pending.
        s.send_and_wait_for_draw(ZOOM_IN);
        s.send(b"?");
        s.wait_for_text(HELP_MARKER);
    });
}
