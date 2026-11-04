//! 剪贴板读写（Win32）。由 desktop 线程调用，hwnd 是它的隐藏窗口。
//! OpenClipboard 可能因为别的程序占用而失败：最多重试 10 次，每次间隔 10ms。

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};

pub const CF_DIB: u32 = 8;
pub const CF_UNICODETEXT: u32 = 13;
pub const CF_DIBV5: u32 = 17;

pub enum Content {
    Text(String),
    /// format = CF_DIBV5 或 CF_DIB，data = GlobalLock 出来的原始字节。
    Image { format: u32, data: Vec<u8> },
}

/// 打开期间持有；Drop 时 CloseClipboard。
struct Opened;

impl Drop for Opened {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn open(hwnd: HWND) -> Option<Opened> {
    for i in 0..10 {
        if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
            return Some(Opened);
        }
        if i < 9 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    None
}

/// GlobalLock 的守卫：Drop 时 GlobalUnlock。
struct Locked {
    h: HGLOBAL,
    p: *mut u8,
}

impl Locked {
    fn new(h: HGLOBAL) -> Option<Locked> {
        let p = unsafe { GlobalLock(h) } as *mut u8;
        if p.is_null() { None } else { Some(Locked { h, p }) }
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        unsafe {
            // 锁计数归零时也返回“失败”，忽略
            let _ = GlobalUnlock(self.h);
        }
    }
}

fn available(format: u32) -> bool {
    unsafe { IsClipboardFormatAvailable(format) }.is_ok()
}

/// 取某格式的 HGLOBAL 及其大小（剪贴板须已打开）。
fn get_global(format: u32) -> Option<(HGLOBAL, usize)> {
    let h = unsafe { GetClipboardData(format) }.ok()?;
    let hg = HGLOBAL(h.0);
    let size = unsafe { GlobalSize(hg) };
    Some((hg, size))
}

/// 读取剪贴板：优先文字（超过 max_text 字节返回 None），否则若 want_image 读图片（超过 max_image 字节返回 None）。
pub fn read(hwnd: HWND, max_text: usize, want_image: bool, max_image: usize) -> Option<Content> {
    let _g = open(hwnd)?;
    if available(CF_UNICODETEXT) {
        return read_text(max_text).map(Content::Text);
    }
    if !want_image {
        return None;
    }
    for format in [CF_DIBV5, CF_DIB] {
        if available(format) {
            return read_bytes(format, max_image).map(|data| Content::Image { format, data });
        }
    }
    None
}

fn read_text(max_text: usize) -> Option<String> {
    let (hg, size) = get_global(CF_UNICODETEXT)?;
    // UTF-16 字节数先粗筛（ASCII 时 UTF-8 长度 = UTF-16 字节数 / 2），转换后再按 UTF-8 字节精确判断
    if size < 2 || size > max_text.saturating_mul(2).saturating_add(2) {
        return None;
    }
    let lk = Locked::new(hg)?;
    let units = unsafe { std::slice::from_raw_parts(lk.p as *const u16, size / 2) };
    let s = utf16_until_nul(units);
    drop(lk);
    if s.is_empty() || s.len() > max_text { None } else { Some(s) }
}

fn read_bytes(format: u32, max: usize) -> Option<Vec<u8>> {
    let (hg, size) = get_global(format)?;
    if size == 0 || size > max {
        return None;
    }
    let lk = Locked::new(hg)?;
    let v = unsafe { std::slice::from_raw_parts(lk.p as *const u8, size) }.to_vec();
    Some(v)
}

/// UTF-16 读到第一个 0 为止（没有 0 就整段）。
pub(crate) fn utf16_until_nul(units: &[u16]) -> String {
    let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// 文字 → CF_UNICODETEXT 的字节（UTF-16 LE + 结尾 0）。
pub(crate) fn text_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16().chain(std::iter::once(0)).flat_map(|u| u.to_le_bytes()).collect()
}

fn write_global(hwnd: HWND, format: u32, data: &[u8]) -> Option<u32> {
    let g = open(hwnd)?;
    unsafe {
        EmptyClipboard().ok()?;
        let hg = GlobalAlloc(GMEM_MOVEABLE, data.len().max(1)).ok()?;
        match Locked::new(hg) {
            Some(lk) => {
                std::ptr::copy_nonoverlapping(data.as_ptr(), lk.p, data.len());
            }
            None => {
                let _ = GlobalFree(Some(hg));
                return None;
            }
        }
        if SetClipboardData(format, Some(HANDLE(hg.0))).is_err() {
            let _ = GlobalFree(Some(hg));
            return None;
        }
    }
    drop(g);
    Some(seq())
}

/// 写入文字（CF_UNICODETEXT，GlobalAlloc GMEM_MOVEABLE）。返回写入后的 GetClipboardSequenceNumber()。
pub fn write_text(hwnd: HWND, text: &str) -> Option<u32> {
    write_global(hwnd, CF_UNICODETEXT, &text_bytes(text))
}

/// 写入图片。返回写入后的序号。
pub fn write_image(hwnd: HWND, format: u32, data: &[u8]) -> Option<u32> {
    if data.is_empty() || (format != CF_DIB && format != CF_DIBV5) {
        return None;
    }
    write_global(hwnd, format, data)
}

pub fn seq() -> u32 {
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_nul() {
        let v: Vec<u16> = "你好a".encode_utf16().chain([0, 65, 66]).collect();
        assert_eq!(utf16_until_nul(&v), "你好a");
        let w: Vec<u16> = "abc".encode_utf16().collect();
        assert_eq!(utf16_until_nul(&w), "abc");
        assert_eq!(utf16_until_nul(&[]), "");
    }

    #[test]
    fn text_bytes_roundtrip() {
        let b = text_bytes("剪贴板 ok");
        assert_eq!(b.len() % 2, 0);
        assert_eq!(&b[b.len() - 2..], &[0, 0]);
        let u: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(utf16_until_nul(&u), "剪贴板 ok");
    }
}
