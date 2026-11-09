//! A 端：把复制的第一层路径（CF_HDROP）展开成清单。在工作线程里做（可能慢：网络共享、大文件夹）。
//!
//! - 文件夹先序：文件夹项在它里面的东西之前，空文件夹也有自己的项。
//! - 联接点、目录符号链接、文件符号链接：不进入、不列入（日志记一笔）。OneDrive 这类云文件占位：列入（真被读时才下载）。
//! - 数到 10001 项立即停 / 展开超过 10 秒：整次不同步（“文件太多”）。
//! - 第一层重名（从搜索结果里复制了不同文件夹下的同名项）：改名 a (2).txt。
//! - 名字不合格（第 8.2 节规则）：整次不同步，说原因，不悄悄丢掉其中几项。

use super::{ATTR_DIR, Item, KEEP_ATTRS, MAX_ITEMS, MAX_PATH_UNITS, Manifest, component_ok};
use std::collections::HashSet;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SCAN_LIMIT: Duration = Duration::from_secs(10);
const ATTR_REPARSE: u32 = 0x400;
/// 云文件占位（OneDrive 等）的属性：带 reparse point 也照样列入。
const ATTR_CLOUD: u32 = 0x1000 | 0x40000 | 0x80000 | 0x100000 | 0x400000;

pub struct Scanned {
    pub man: Manifest,
    /// 每项在本机的绝对路径（和 man.items 一一对应，不发给对方）。
    pub paths: Vec<PathBuf>,
}

struct Walk {
    items: Vec<Item>,
    paths: Vec<PathBuf>,
    until: Instant,
    skipped: usize,
}

fn link(attrs: u32) -> bool {
    attrs & ATTR_REPARSE != 0 && attrs & ATTR_CLOUD == 0
}

/// 第一层重名时的新名字：a.txt → a (2).txt；文件夹 / 没扩展名：a → a (2)。
fn renamed(name: &str, n: usize, dir: bool) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !dir && !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

impl Walk {
    fn push(&mut self, rel: String, abs: PathBuf, md: &std::fs::Metadata) -> Result<(), &'static str> {
        if self.items.len() >= MAX_ITEMS {
            return Err("文件太多");
        }
        if Instant::now() > self.until {
            return Err("文件太多");
        }
        if rel.encode_utf16().count() > MAX_PATH_UNITS {
            return Err("路径太长");
        }
        let d = md.is_dir();
        self.items.push(Item {
            p: rel,
            s: if d { 0 } else { md.len() },
            m: if d { 0 } else { md.last_write_time() },
            a: md.file_attributes() & KEEP_ATTRS & !ATTR_DIR,
            d,
        });
        self.paths.push(abs);
        Ok(())
    }

    fn dir(&mut self, rel: &str, abs: &Path) -> Result<(), &'static str> {
        let rd = std::fs::read_dir(abs).map_err(|e| {
            crate::log!("文件：读文件夹 {} 失败：{e}", abs.display());
            "读不了"
        })?;
        let mut subdirs = Vec::new();
        for e in rd {
            let Ok(e) = e else { return Err("读不了") };
            // DirEntry::metadata 用的是枚举时拿到的信息，不打开文件、不跟随链接
            let Ok(md) = e.metadata() else { return Err("读不了") };
            if link(md.file_attributes()) {
                self.skipped += 1;
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            component_ok(&name)?;
            let r = format!("{rel}\\{name}");
            if md.is_dir() {
                subdirs.push((r, e.path(), md));
            } else {
                self.push(r, e.path(), &md)?;
            }
        }
        // 文件先列、子文件夹后列：每个子文件夹紧跟着它里面的东西（先序）
        for (r, p, md) in subdirs {
            self.push(r.clone(), p.clone(), &md)?;
            self.dir(&r, &p)?;
        }
        Ok(())
    }
}

/// 展开第一层路径。Err = 整次不同步的原因（≤ 4 字，提示里用）。
pub fn expand(tops: &[PathBuf]) -> Result<Scanned, &'static str> {
    let mut w = Walk { items: Vec::new(), paths: Vec::new(), until: Instant::now() + SCAN_LIMIT, skipped: 0 };
    let mut names: HashSet<String> = HashSet::new();
    for top in tops {
        let Ok(md) = std::fs::symlink_metadata(top) else { return Err("读不了") };
        if link(md.file_attributes()) {
            w.skipped += 1;
            continue;
        }
        // 盘符根目录（D:\）没有名字：不支持
        let Some(name) = top.file_name().map(|n| n.to_string_lossy().into_owned()) else { return Err("名字不支持") };
        component_ok(&name)?;
        let mut n = 1;
        let mut use_name = name.clone();
        while !names.insert(use_name.to_lowercase()) {
            n += 1;
            use_name = renamed(&name, n, md.is_dir());
        }
        w.push(use_name.clone(), top.clone(), &md)?;
        if md.is_dir() {
            w.dir(&use_name, top)?;
        }
    }
    if w.skipped > 0 {
        crate::log!("文件：跳过了 {} 个链接（联接点 / 符号链接）", w.skipped);
    }
    if w.items.is_empty() {
        return Err("没有文件");
    }
    let man = Manifest::new(w.items);
    super::validate(&man)?;
    Ok(Scanned { man, paths: w.paths })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_rules() {
        assert_eq!(renamed("a.txt", 2, false), "a (2).txt");
        assert_eq!(renamed("a", 3, false), "a (3)");
        assert_eq!(renamed("a.b", 2, true), "a.b (2)");
        assert_eq!(renamed(".gitignore", 2, false), ".gitignore (2)");
    }

    #[test]
    fn expands_tree() {
        let base = std::env::temp_dir().join(format!("kmlink-scan-{}", super::super::hex(&super::super::new_mid()[..4])));
        let d = base.join("dir");
        std::fs::create_dir_all(d.join("empty")).unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a.txt"), b"abc").unwrap();
        std::fs::write(d.join("sub").join("b.bin"), vec![1u8; 10]).unwrap();
        let other = base.join("x");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("a.txt"), b"z").unwrap();
        let s = expand(&[d.clone(), d.join("a.txt"), other.join("a.txt")]).unwrap();
        let ps: Vec<&str> = s.man.items.iter().map(|i| i.p.as_str()).collect();
        assert_eq!(ps[0], "dir");
        assert!(ps.contains(&"dir\\empty"));
        assert!(ps.contains(&"dir\\sub\\b.bin"));
        let sub = ps.iter().position(|p| *p == "dir\\sub").unwrap();
        let b = ps.iter().position(|p| *p == "dir\\sub\\b.bin").unwrap();
        assert!(sub < b);
        // 第一层两个 a.txt：第二个改名
        assert!(ps.contains(&"a.txt") && ps.contains(&"a (2).txt"));
        assert_eq!(s.man.bytes, 3 + 10 + 3 + 1);
        assert_eq!(s.paths.len(), s.man.items.len());
        let _ = std::fs::remove_dir_all(&base);
    }
}

impl std::fmt::Debug for Scanned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Scanned {{ items: {}, bytes: {} }}", self.man.items.len(), self.man.bytes)
    }
}
