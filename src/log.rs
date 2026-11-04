//! 极简日志：写 %APPDATA%\kmlink\<name>.log（超过 2 MB 轮转为 <name>.old.log），带 --console 时同时打到控制台。
//! `log!` 不阻塞：只格式化后放进 channel，后台线程做文件/控制台 IO。init 之前的日志先缓存在 channel 里。
//! 仍然不要在钩子回调里打日志（格式化也要时间）。

use crossbeam_channel::{Receiver, Sender, unbounded};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub static CONSOLE: AtomicBool = AtomicBool::new(false);
const MAX: u64 = 2 * 1024 * 1024;

fn chan() -> &'static (Sender<String>, Receiver<String>) {
    static CH: OnceLock<(Sender<String>, Receiver<String>)> = OnceLock::new();
    CH.get_or_init(unbounded)
}

static STARTED: AtomicBool = AtomicBool::new(false);
/// 已入队 / 已写完的行数（flush 用）。
static QUEUED: AtomicU64 = AtomicU64::new(0);
static DONE: AtomicU64 = AtomicU64::new(0);

pub fn init(name: &str) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let dir = crate::config::dir();
    let _ = std::fs::create_dir_all(&dir);
    let cur = dir.join(format!("{name}.log"));
    let old = dir.join(format!("{name}.old.log"));
    let rx = chan().1.clone();
    let r = std::thread::Builder::new().name("log".into()).spawn(move || writer(rx, cur, old));
    if r.is_err() {
        STARTED.store(false, Ordering::SeqCst);
    }
}

struct Out {
    cur: PathBuf,
    old: PathBuf,
    file: Option<std::fs::File>,
    size: u64,
}

impl Out {
    fn open(&mut self) {
        self.file = std::fs::OpenOptions::new().create(true).append(true).open(&self.cur).ok();
        self.size = std::fs::metadata(&self.cur).map(|m| m.len()).unwrap_or(0);
    }

    fn rotate_if_big(&mut self) {
        if self.size > MAX {
            self.file = None;
            let _ = std::fs::remove_file(&self.old);
            let _ = std::fs::rename(&self.cur, &self.old);
            self.open();
        }
    }

    fn write(&mut self, s: &str) {
        if let Some(f) = self.file.as_mut()
            && f.write_all(s.as_bytes()).is_ok()
        {
            self.size += s.len() as u64;
        }
    }
}

fn writer(rx: Receiver<String>, cur: PathBuf, old: PathBuf) {
    let mut o = Out { cur, old, file: None, size: 0 };
    o.open();
    o.rotate_if_big();
    while let Ok(first) = rx.recv() {
        // 一次取完积压的行，合成一次写
        let mut buf = first;
        let mut n = 1u64;
        while buf.len() < 64 * 1024 {
            match rx.try_recv() {
                Ok(s) => {
                    buf.push_str(&s);
                    n += 1;
                }
                Err(_) => break,
            }
        }
        o.write(&buf);
        if CONSOLE.load(Ordering::Relaxed) {
            eprint!("{buf}");
        }
        o.rotate_if_big();
        DONE.fetch_add(n, Ordering::SeqCst);
    }
}

pub fn write(s: &str) {
    let line = format_line(s);
    QUEUED.fetch_add(1, Ordering::SeqCst);
    let _ = chan().0.send(line);
}

fn format_line(s: &str) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{}.{:03} {}\n", now.as_secs(), now.subsec_millis(), s)
}

/// 等后台线程把已入队的日志写完（最多 timeout_ms）。退出前调用，免得最后几行丢掉。
pub fn flush(timeout_ms: u64) {
    if !STARTED.load(Ordering::SeqCst) {
        return;
    }
    let target = QUEUED.load(Ordering::SeqCst);
    let end = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while DONE.load(Ordering::SeqCst) < target && std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[macro_export]
macro_rules! log {
    ($($a:tt)*) => { $crate::log::write(&format!($($a)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_format_and_no_block_before_init() {
        let l = format_line("你好");
        assert!(l.ends_with(" 你好\n"));
        // init 之前写日志：只入队，不阻塞、不 panic
        write("before init");
    }

    #[test]
    fn rotates_when_big() {
        let dir = std::env::temp_dir().join(format!("kmlink-logtest-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cur = dir.join("t.log");
        let old = dir.join("t.old.log");
        let _ = std::fs::remove_file(&cur);
        let _ = std::fs::remove_file(&old);
        let mut o = Out { cur: cur.clone(), old: old.clone(), file: None, size: 0 };
        o.open();
        o.write("abc\n");
        o.size = MAX + 1; // 模拟写满
        o.rotate_if_big();
        assert!(old.exists());
        assert_eq!(o.size, 0);
        o.write("x\n");
        drop(o);
        assert_eq!(std::fs::read_to_string(&cur).unwrap(), "x\n");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "abc\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
