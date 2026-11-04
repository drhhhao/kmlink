//! 极简日志：写 %APPDATA%\kmlink\kmlink.log（超过 2 MB 轮转一次），带 --console 时同时打到控制台。
//! 不要在钩子回调里打日志（文件 IO 会拖慢回调）；需要时先发消息给别的线程。

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
pub static CONSOLE: AtomicBool = AtomicBool::new(false);
const MAX: u64 = 2 * 1024 * 1024;

pub fn init(name: &str) {
    let dir = crate::config::dir();
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join(format!("{name}.log"));
    if std::fs::metadata(&p).map(|m| m.len() > MAX).unwrap_or(false) {
        let _ = std::fs::rename(&p, dir.join(format!("{name}.old.log")));
    }
    let f = std::fs::OpenOptions::new().create(true).append(true).open(p).ok();
    *FILE.lock().unwrap() = f;
}

pub fn write(s: &str) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs();
    let line = format!("{}.{:03} {}\n", secs, now.subsec_millis(), s);
    if let Some(f) = FILE.lock().unwrap().as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
    if CONSOLE.load(Ordering::Relaxed) {
        eprint!("{line}");
    }
}

#[macro_export]
macro_rules! log {
    ($($a:tt)*) => { $crate::log::write(&format!($($a)*)) };
}
