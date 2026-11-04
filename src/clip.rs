//! 剪贴板读写（Win32）。由 desktop 线程调用，hwnd 是它的隐藏窗口。
//! OpenClipboard 可能因为别的程序占用而失败：最多重试 10 次，每次间隔 10ms。

use windows::Win32::Foundation::HWND;

pub const CF_DIB: u32 = 8;
pub const CF_UNICODETEXT: u32 = 13;
pub const CF_DIBV5: u32 = 17;

pub enum Content {
    Text(String),
    /// format = CF_DIBV5 或 CF_DIB，data = GlobalLock 出来的原始字节。
    Image { format: u32, data: Vec<u8> },
}

/// 读取剪贴板：优先文字（超过 max_text 字节返回 None），否则若 want_image 读图片（超过 max_image 字节返回 None）。
pub fn read(hwnd: HWND, max_text: usize, want_image: bool, max_image: usize) -> Option<Content> {
    let _ = (hwnd, max_text, want_image, max_image);
    todo!()
}

/// 写入文字（CF_UNICODETEXT，GlobalAlloc GMEM_MOVEABLE）。返回写入后的 GetClipboardSequenceNumber()。
pub fn write_text(hwnd: HWND, text: &str) -> Option<u32> {
    let _ = (hwnd, text);
    todo!()
}

/// 写入图片。返回写入后的序号。
pub fn write_image(hwnd: HWND, format: u32, data: &[u8]) -> Option<u32> {
    let _ = (hwnd, format, data);
    todo!()
}

pub fn seq() -> u32 {
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}
