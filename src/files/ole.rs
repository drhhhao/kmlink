//! B 端：剪贴板上的“虚拟文件”。
//!
//! 线程：
//! - clip-ole（STA，OleInitialize）：所有 OleSetClipboard / OleIsCurrentClipboard / 退出时的 Win32 写入都在这里。
//!   desktop 线程不碰 OLE（别的程序 GetClipboardData 引发的 WM_RENDERFORMAT 落在这条线程上，和托盘、灵动岛隔开）。
//! - files-mta（MTA，常驻：保证进程里一直有 MTA）：数据对象在这里封送（CoMarshalInterThreadInterfaceInStream），
//!   clip-ole 拿到的是代理，交给 OleSetClipboard。资源管理器跨进程调来的 GetData / Read 因此落在 RPC 线程池上，
//!   Read 等网络、等续传都不卡任何有消息循环的线程。
//!
//! 数据对象固定提供：FileGroupDescriptorW、FileContents（只给 IStream，lindex 必须是一个文件）、
//! Preferred DropEffect = 复制、CanUploadToCloudClipboard = 0、CanIncludeInClipboardHistory = 0；
//! 预取完成后的第二阶段对象另有 CF_HDROP（指向已写完的临时文件）。GetData 绝不等网络；
//! FileContents 绝不渲染成 HGLOBAL（剪贴板管理器扫一遍就会把几个 GB 拉进内存）。
//! 退出：剪贴板还是自己的就换成纯 CF_HDROP（已预取完）或清空；绝不调 OleFlushClipboard。

use super::fetch::{ReadErr, Remote};
use crate::hub::{ToDesktop, WM_APP_WAKE};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use windows::Win32::Foundation::{
    CLIPBRD_E_CANT_EMPTY, CLIPBRD_E_CANT_OPEN, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_ABORT, E_NOTIMPL, ERROR_CANCELLED, ERROR_NETNAME_DELETED,
    FILETIME, HGLOBAL, LPARAM, OLE_E_ADVISENOTSUPPORTED, S_OK, STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION, STG_E_READFAULT,
    WPARAM,
};
use windows::Win32::System::Com::Marshal::CoMarshalInterThreadInterfaceInStream;
use windows::Win32::System::Com::StructuredStorage::CoGetInterfaceAndReleaseStream;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemAlloc, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC,
    IEnumSTATDATA, ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0,
    STREAM_SEEK,
};
use windows::Win32::System::DataExchange::{GetClipboardOwner, GetClipboardSequenceNumber, RegisterClipboardFormatW};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{OleInitialize, OleIsCurrentClipboard, OleSetClipboard, ReleaseStgMedium};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::SHCreateStdEnumFmtEtc;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DispatchMessageW, GetMessageW, GetWindowThreadProcessId, HWND_MESSAGE, MSG, PostThreadMessageW,
    TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE,
};
use windows::core::{BOOL, HRESULT, Interface, Ref, implement, w};

const TYMED_HGLOBAL: u32 = 1;
const TYMED_ISTREAM: u32 = 4;
const DVASPECT_CONTENT: u32 = 1;
const DATADIR_GET: u32 = 1;
const STGTY_STREAM: u32 = 2;
const STATFLAG_NONAME: i32 = 1;
const DROPEFFECT_COPY: u32 = 1;

// ---------------------------------------------------------------- 格式

struct Fmts {
    fgd: u16,
    contents: u16,
    effect: u16,
    cloud: u16,
    history: u16,
    performed: u16,
    succeeded: u16,
    logical: u16,
}

fn fmts() -> &'static Fmts {
    static F: OnceLock<Fmts> = OnceLock::new();
    F.get_or_init(|| unsafe {
        let r = |n| RegisterClipboardFormatW(n) as u16;
        Fmts {
            fgd: r(w!("FileGroupDescriptorW")),
            contents: r(w!("FileContents")),
            effect: r(w!("Preferred DropEffect")),
            cloud: r(w!("CanUploadToCloudClipboard")),
            history: r(w!("CanIncludeInClipboardHistory")),
            performed: r(w!("Performed DropEffect")),
            succeeded: r(w!("Paste Succeeded")),
            logical: r(w!("Logical Performed DropEffect")),
        }
    })
}

fn fe(cf: u16, tymed: u32) -> FORMATETC {
    FORMATETC { cfFormat: cf, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT, lindex: -1, tymed }
}

fn hglobal(bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
    // SAFETY：新分配的内存，锁定期间写满 bytes.len() 字节；交出后由调用方 ReleaseStgMedium
    unsafe {
        let h: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            let _ = windows::Win32::Foundation::GlobalFree(Some(h));
            return Err(E_ABORT.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Ok(STGMEDIUM { tymed: TYMED_HGLOBAL, u: STGMEDIUM_0 { hGlobal: h }, pUnkForRelease: ManuallyDrop::new(None) })
    }
}

fn read_hr(e: ReadErr) -> HRESULT {
    match e {
        ReadErr::Net => ERROR_NETNAME_DELETED.to_hresult(),
        ReadErr::Changed => STG_E_READFAULT,
        ReadErr::Abort => E_ABORT,
        // 用户在灵动岛上点了取消：资源管理器按“用户取消”处理（不弹错、删半截文件）
        ReadErr::Cancelled => ERROR_CANCELLED.to_hresult(),
    }
}

// ---------------------------------------------------------------- 数据对象

#[implement(IDataObject)]
struct DataObj {
    remote: Arc<Remote>,
    /// 第二阶段：DROPFILES 字节（指向已写完的临时文件）。
    hdrop: Option<Arc<Vec<u8>>>,
}

enum Want {
    Desc,
    Stream(u32),
    Dword(u32),
    Hdrop,
}

impl DataObj {
    /// 这个 FORMATETC 我们给不给、给什么（GetData 和 QueryGetData 同一套规则）。
    fn want(&self, f: &FORMATETC) -> Result<Want, HRESULT> {
        let m = fmts();
        if f.dwAspect != DVASPECT_CONTENT {
            return Err(DV_E_FORMATETC);
        }
        let need = |t: u32| if f.tymed & t != 0 { Ok(()) } else { Err(DV_E_TYMED) };
        let cf = f.cfFormat;
        if cf == 0 {
            return Err(DV_E_FORMATETC);
        }
        if cf == m.fgd {
            need(TYMED_HGLOBAL)?;
            return Ok(Want::Desc);
        }
        if cf == m.contents {
            need(TYMED_ISTREAM)?;
            let i = f.lindex;
            let ok = i >= 0 && self.remote.man.items.get(i as usize).is_some_and(|it| !it.d);
            return if ok { Ok(Want::Stream(i as u32)) } else { Err(DV_E_LINDEX) };
        }
        if cf == m.effect {
            need(TYMED_HGLOBAL)?;
            return Ok(Want::Dword(DROPEFFECT_COPY));
        }
        if cf == m.cloud || cf == m.history {
            need(TYMED_HGLOBAL)?;
            return Ok(Want::Dword(0));
        }
        if cf as u32 == crate::clip::CF_HDROP && self.hdrop.is_some() {
            need(TYMED_HGLOBAL)?;
            return Ok(Want::Hdrop);
        }
        Err(DV_E_FORMATETC)
    }

    fn formats(&self) -> Vec<FORMATETC> {
        let m = fmts();
        let mut v = vec![
            fe(m.fgd, TYMED_HGLOBAL),
            fe(m.contents, TYMED_ISTREAM),
            fe(m.effect, TYMED_HGLOBAL),
            fe(m.cloud, TYMED_HGLOBAL),
            fe(m.history, TYMED_HGLOBAL),
        ];
        if self.hdrop.is_some() {
            v.push(fe(crate::clip::CF_HDROP as u16, TYMED_HGLOBAL));
        }
        v
    }
}

impl IDataObject_Impl for DataObj_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        if pformatetcin.is_null() {
            return Err(DV_E_FORMATETC.into());
        }
        // SAFETY：调用方给的有效指针
        let f = unsafe { &*pformatetcin };
        match self.want(f).map_err(windows::core::Error::from)? {
            Want::Desc => hglobal(&self.remote.desc),
            Want::Dword(v) => hglobal(&v.to_le_bytes()),
            Want::Hdrop => hglobal(self.hdrop.as_deref().map(|v| &v[..]).unwrap_or(&[])),
            Want::Stream(i) => {
                let s: IStream = FileStream::new(self.remote.clone(), i).into();
                Ok(STGMEDIUM { tymed: TYMED_ISTREAM, u: STGMEDIUM_0 { pstm: ManuallyDrop::new(Some(s)) }, pUnkForRelease: ManuallyDrop::new(None) })
            }
        }
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        if pformatetc.is_null() {
            return DV_E_FORMATETC;
        }
        // SAFETY：调用方给的有效指针
        match self.want(unsafe { &*pformatetc }) {
            Ok(_) => S_OK,
            Err(e) => e,
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, pformatetcout: *mut FORMATETC) -> HRESULT {
        if !pformatetcout.is_null() {
            // SAFETY：调用方给的有效指针；按约定出错时把 ptd 置空
            unsafe { (*pformatetcout).ptd = std::ptr::null_mut() };
        }
        E_NOTIMPL
    }

    fn SetData(&self, pformatetc: *const FORMATETC, pmedium: *const STGMEDIUM, frelease: BOOL) -> windows::core::Result<()> {
        if pformatetc.is_null() || pmedium.is_null() {
            return Err(E_NOTIMPL.into());
        }
        let m = fmts();
        // SAFETY：调用方给的有效指针
        let cf = unsafe { (*pformatetc).cfFormat };
        if cf != m.performed && cf != m.succeeded && cf != m.logical {
            return Err(E_NOTIMPL.into());
        }
        // 只收下、不做任何删除（我们永远只给“复制”）。fRelease 时介质归我们，负责释放
        if frelease.as_bool() {
            // SAFETY：调用方把介质交给了我们
            unsafe { ReleaseStgMedium(pmedium as *mut STGMEDIUM) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET {
            return Err(E_NOTIMPL.into());
        }
        // SAFETY：数组在调用期间有效，函数内部拷贝
        unsafe { SHCreateStdEnumFmtEtc(&self.formats()) }
    }

    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<'_, IAdviseSink>) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

// ---------------------------------------------------------------- 文件流

#[implement(IStream)]
struct FileStream {
    remote: Arc<Remote>,
    idx: u32,
    size: u64,
    pos: Mutex<u64>,
    /// 读到过文件尾（Release 时据此判断是读完了还是被取消）。
    done: AtomicBool,
}

impl FileStream {
    fn new(remote: Arc<Remote>, idx: u32) -> FileStream {
        let size = remote.man.items[idx as usize].s;
        if let Some(f) = super::get() {
            f.fetch.stream_open(&remote.mid);
        }
        FileStream { remote, idx, size, pos: Mutex::new(0), done: AtomicBool::new(size == 0) }
    }
}

impl Drop for FileStream {
    fn drop(&mut self) {
        if let Some(f) = super::get() {
            f.fetch.stream_close(&self.remote.mid, self.idx, self.done.load(Ordering::Relaxed));
        }
    }
}

impl ISequentialStream_Impl for FileStream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if !pcbread.is_null() {
            // SAFETY：调用方给的有效指针
            unsafe { *pcbread = 0 };
        }
        if pv.is_null() {
            return STG_E_INVALIDFUNCTION;
        }
        let Some(files) = super::get() else { return E_ABORT };
        let mut pos = self.pos.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY：调用方给的缓冲至少 cb 字节，Read 返回前归我们写
        let out = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, cb as usize) };
        match files.fetch.read(&self.remote, self.idx, *pos, out, false) {
            Ok(n) => {
                *pos += n as u64;
                if *pos >= self.size {
                    self.done.store(true, Ordering::Relaxed);
                }
                if !pcbread.is_null() {
                    // SAFETY：同上
                    unsafe { *pcbread = n as u32 };
                }
                S_OK
            }
            Err(e) => {
                crate::log!("文件：读第 {} 项失败（{e:?}）", self.idx);
                read_hr(e)
            }
        }
    }

    fn Write(&self, _: *const core::ffi::c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for FileStream_Impl {
    fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> windows::core::Result<()> {
        let mut pos = self.pos.lock().unwrap_or_else(|e| e.into_inner());
        let base = match dworigin.0 {
            0 => 0i128,
            1 => *pos as i128,
            2 => self.size as i128,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let n = base + dlibmove as i128;
        if n < 0 || n > self.size as i128 {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        *pos = n as u64;
        if !plibnewposition.is_null() {
            // SAFETY：调用方给的有效指针
            unsafe { *plibnewposition = *pos };
        }
        Ok(())
    }

    fn SetSize(&self, _: u64) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(&self, _: Ref<'_, IStream>, _: u64, _: *mut u64, _: *mut u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _: &STGC) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, grfstatflag: &STATFLAG) -> windows::core::Result<()> {
        if pstatstg.is_null() {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        let it = &self.remote.man.items[self.idx as usize];
        let ft = FILETIME { dwLowDateTime: it.m as u32, dwHighDateTime: (it.m >> 32) as u32 };
        let mut st = STATSTG { r#type: STGTY_STREAM, cbSize: self.size, mtime: ft, ..Default::default() };
        if grfstatflag.0 & STATFLAG_NONAME == 0 {
            let name: Vec<u16> = it.p.rsplit('\\').next().unwrap_or("").encode_utf16().chain([0]).collect();
            // SAFETY：CoTaskMemAlloc 的内存由调用方 CoTaskMemFree；写满 name.len() 个 u16
            unsafe {
                let p = CoTaskMemAlloc(name.len() * 2) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(name.as_ptr(), p, name.len());
                    st.pwcsName = windows::core::PWSTR(p);
                }
            }
        }
        // SAFETY：调用方给的有效指针
        unsafe { *pstatstg = st };
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

// ---------------------------------------------------------------- 线程

/// 写入结果（交给 desktop）。
#[derive(Debug)]
pub struct Written {
    /// 写入时本机剪贴板的代次（desktop 据此丢掉过时的结果）。
    pub g: u64,
    pub ver: u64,
    /// 这是第几次重试（剪贴板被占时 desktop 定时重试）。
    pub tries: u32,
    pub remote: Arc<Remote>,
    pub r: Result<u32, crate::clip::WriteError>,
}

enum Cmd {
    Set { remote: Arc<Remote>, g: u64, ver: u64, tries: u32 },
    AddHdrop { mid: [u8; 16], hdrop: Arc<Vec<u8>> },
    Clear,
    Shutdown(Sender<()>),
}

type Job = Box<dyn FnOnce() + Send>;

static OLE_TID: AtomicU32 = AtomicU32::new(0);
static CMD: OnceLock<Sender<Cmd>> = OnceLock::new();
static MTA: OnceLock<Sender<Job>> = OnceLock::new();

/// 封送好的接口指针（只是一段内存流，可以跨线程交接）。没被解封就丢掉时（等的一方超时走了），
/// Drop 里解封一次再放掉，释放封送时存根持有的引用（不然数据对象永远不释放）。
struct SendStm(Option<IStream>);
// SAFETY：CoMarshalInterThreadInterfaceInStream 的结果就是为了交给另一个线程用的
unsafe impl Send for SendStm {}

impl Drop for SendStm {
    fn drop(&mut self) {
        if let Some(s) = self.0.take() {
            let _ = unmarshal_stream(s);
        }
    }
}

/// 进程的 COM 安全设好了（中完整性的程序能调进本进程交出去的对象）。
static SEC_OK: AtomicBool = AtomicBool::new(false);

/// 进程一启动就调（任何线程用 COM 之前）：起 files-mta 线程进入 MTA，并设进程的 COM 安全 ——
/// 允许本机中完整性及以上的程序调进本进程交出去的对象。kmlink 以管理员身份运行时（两台平时都是），
/// 不设这个，普通权限的资源管理器调不进虚拟文件对象（粘贴失败）。
/// 只放宽“谁能调我们交出去的对象”：交出去的只有剪贴板上的数据对象和文件流，本来就是给任何程序粘贴用的。
pub fn init() {
    if MTA.get().is_some() {
        return;
    }
    let (jtx, jrx) = unbounded::<Job>();
    let _ = MTA.set(jtx);
    let (rtx, rrx) = bounded(1);
    super::spawn("files-mta", move || {
        // SAFETY：本线程进入 MTA 并一直不退（进程里始终有 MTA，封送出去的对象不会被断开）
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            crate::log!("文件：MTA 初始化失败：{hr:?}");
        }
        let _ = rtx.send(security());
        for j in jrx {
            j();
        }
    });
    let ok = rrx.recv_timeout(Duration::from_secs(3)).unwrap_or(false);
    SEC_OK.store(ok, Ordering::SeqCst);
}

fn security() -> bool {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows::Win32::Security::PSECURITY_DESCRIPTOR;
    use windows::Win32::System::Com::{CoInitializeSecurity, EOAC_NONE, RPC_C_AUTHN_LEVEL_DEFAULT, RPC_C_IMP_LEVEL_IDENTIFY};
    // 所有人可本地调用（0x3 = COM_RIGHTS_EXECUTE | EXECUTE_LOCAL），完整性标签“中、不许往上写”：低完整性的进程仍调不进来
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY：sd 由系统分配，用完 LocalFree；CoInitializeSecurity 会自己拷一份
    unsafe {
        if let Err(e) = ConvertStringSecurityDescriptorToSecurityDescriptorW(w!("O:BAG:BAD:(A;;0x3;;;WD)S:(ML;;NW;;;ME)"), SDDL_REVISION_1, &mut sd, None) {
            crate::log!("文件：生成 COM 安全描述符失败：{e}");
            return false;
        }
        // CoInitializeSecurity 要“绝对格式”的描述符：先量各部分大小，再拆开（缓冲故意泄漏：进程级、只做一次）
        use windows::Win32::Security::{ACL, MakeAbsoluteSD, PSID};
        let (mut n_sd, mut n_dacl, mut n_sacl, mut n_owner, mut n_group) = (0u32, 0u32, 0u32, 0u32, 0u32);
        let _ = MakeAbsoluteSD(sd, None, &mut n_sd, None, &mut n_dacl, None, &mut n_sacl, None, &mut n_owner, None, &mut n_group);
        let buf = |n: u32| Box::leak(vec![0u64; (n as usize).div_ceil(8).max(1)].into_boxed_slice()).as_mut_ptr() as *mut core::ffi::c_void;
        let abs = PSECURITY_DESCRIPTOR(buf(n_sd));
        let made = MakeAbsoluteSD(
            sd,
            Some(abs),
            &mut n_sd,
            Some(buf(n_dacl) as *mut ACL),
            &mut n_dacl,
            Some(buf(n_sacl) as *mut ACL),
            &mut n_sacl,
            Some(PSID(buf(n_owner))),
            &mut n_owner,
            Some(PSID(buf(n_group))),
            &mut n_group,
        );
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        if let Err(e) = made {
            crate::log!("文件：转换 COM 安全描述符失败：{e}");
            return false;
        }
        let r = CoInitializeSecurity(Some(abs), -1, None, None, RPC_C_AUTHN_LEVEL_DEFAULT, RPC_C_IMP_LEVEL_IDENTIFY, None, EOAC_NONE, None);
        match r {
            Ok(()) => true,
            Err(e) => {
                crate::log!("文件：设置 COM 安全失败：{e}（以管理员身份运行时资源管理器粘贴不了文件）");
                false
            }
        }
    }
}

/// 虚拟文件能用吗：不是管理员身份运行，或者 COM 安全已放宽。
pub fn usable() -> bool {
    SEC_OK.load(Ordering::SeqCst) || !crate::elev::self_elevated()
}

pub fn start() {
    init();
    let (ctx, crx) = unbounded::<Cmd>();
    let _ = CMD.set(ctx);
    super::spawn("clip-ole", move || run(crx));
}

fn send(c: Cmd) -> bool {
    let Some(tx) = CMD.get() else { return false };
    if tx.send(c).is_err() {
        return false;
    }
    let tid = OLE_TID.load(Ordering::SeqCst);
    if tid != 0 {
        // SAFETY：只是投递一条线程消息
        unsafe {
            let _ = PostThreadMessageW(tid, WM_APP_WAKE, WPARAM(0), LPARAM(0));
        }
    }
    true
}

/// 把对方的清单放上剪贴板（异步，结果经 ToDesktop::FilesWritten 回来）。
pub fn set(remote: Arc<Remote>, g: u64, ver: u64, tries: u32) -> bool {
    send(Cmd::Set { remote, g, ver, tries })
}

/// 预取完成：剪贴板还是这份清单的话，换成带 CF_HDROP 的对象。
pub fn add_hdrop(mid: [u8; 16], hdrop: Vec<u8>) {
    send(Cmd::AddHdrop { mid, hdrop: Arc::new(hdrop) });
}

pub fn peer_restarted() {
    send(Cmd::Clear);
}

/// 剪贴板现在的主人是 clip-ole 线程（我们自己放的虚拟文件 / 退出时写的真文件）。
pub fn owns_clipboard() -> bool {
    let tid = OLE_TID.load(Ordering::SeqCst);
    if tid == 0 {
        return false;
    }
    // SAFETY：只读查询
    unsafe {
        let Ok(h) = GetClipboardOwner() else { return false };
        GetWindowThreadProcessId(h, None) == tid
    }
}

/// 退出前：剪贴板还是自己的就换成真文件或清空。至多等 limit。
pub fn shutdown(limit: Duration) {
    let (tx, rx) = bounded(1);
    if send(Cmd::Shutdown(tx)) {
        let _ = rx.recv_timeout(limit);
    }
}

struct Cur {
    obj: IDataObject,
    mid: [u8; 16],
    seq: u32,
    remote: Arc<Remote>,
    hdrop: Option<Arc<Vec<u8>>>,
}

fn make(remote: Arc<Remote>, hdrop: Option<Arc<Vec<u8>>>) -> windows::core::Result<IDataObject> {
    // 在 MTA 线程里建对象并封送，得到的流交给 clip-ole 解封
    let (tx, rx) = bounded::<windows::core::Result<SendStm>>(1);
    let job: Job = Box::new(move || {
        let obj: IDataObject = DataObj { remote, hdrop }.into();
        // SAFETY：在 MTA 线程里封送
        let r = unsafe { CoMarshalInterThreadInterfaceInStream(&IDataObject::IID, &obj) }.map(|s| SendStm(Some(s)));
        // 等的一方已超时走了：r 随 SendError 丢掉，SendStm 的 Drop 负责解封释放
        let _ = tx.send(r);
    });
    let mta = MTA.get().ok_or(windows::core::Error::from(E_ABORT))?;
    mta.send(job).map_err(|_| windows::core::Error::from(E_ABORT))?;
    let mut stm = rx.recv_timeout(Duration::from_secs(5)).map_err(|_| windows::core::Error::from(E_ABORT))??;
    // 超时后才到的会留在通道里，随 rx 丢掉时由 SendStm 的 Drop 解封释放
    unmarshal_stream(stm.0.take().ok_or(windows::core::Error::from(E_ABORT))?)
}

/// 解封并释放流。CoGetInterfaceAndReleaseStream 不管成败都会 Release 这个流，
/// 而 windows-rs 传 &IStream 时不加引用，所以这里交出所有权、不让 Rust 再 Release 一次。
fn unmarshal_stream(stm: IStream) -> windows::core::Result<IDataObject> {
    let stm = std::mem::ManuallyDrop::new(stm);
    // SAFETY：stm 是 CoMarshalInterThreadInterfaceInStream 封送出来的，只解封这一次；它的引用由本调用释放
    unsafe { CoGetInterfaceAndReleaseStream(&*stm) }
}

fn busy(e: &windows::core::Error) -> bool {
    e.code() == CLIPBRD_E_CANT_OPEN || e.code() == CLIPBRD_E_CANT_EMPTY
}

fn is_current(c: &Cur) -> bool {
    // SAFETY：只读查询
    unsafe { OleIsCurrentClipboard(&c.obj) }.is_ok()
}

fn run(rx: Receiver<Cmd>) {
    // SAFETY：本线程是 STA，一直跑消息循环
    if let Err(e) = unsafe { OleInitialize(None) } {
        crate::log!("文件：OleInitialize 失败：{e}，不能放虚拟文件");
        return;
    }
    // 退出时写真文件要一个窗口当剪贴板主人
    // SAFETY：message-only 窗口，用系统的 STATIC 类
    let hwnd = unsafe {
        CreateWindowExW(WINDOW_EX_STYLE(0), w!("STATIC"), w!("kmlink-ole"), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None)
    }
    .unwrap_or_default();
    // SAFETY：取本线程 id
    OLE_TID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    let hub = super::get().map(|f| f.hub.clone());
    let mut cur: Option<Cur> = None;
    let mut msg = MSG::default();
    loop {
        // 先处理已排着的命令（启动前就排进来的也不丢）
        while let Ok(c) = rx.try_recv() {
            match c {
                Cmd::Set { remote, g, ver, tries } => {
                    let r = if !crate::desktop::clip_current(g) {
                        // 排队期间剪贴板有了更新的内容：不放（结果交回去，desktop 会丢掉）
                        Err(crate::clip::WriteError::Failed)
                    } else {
                        match make(remote.clone(), None) {
                        // 建对象（MTA 往返）期间又有了更新的内容：不放
                        Ok(_) if !crate::desktop::clip_current(g) => Err(crate::clip::WriteError::Failed),
                        Ok(obj) => match unsafe { OleSetClipboard(&obj) } {
                            Ok(()) => {
                                // SAFETY：只读查询
                                let seq = unsafe { GetClipboardSequenceNumber() };
                                cur = Some(Cur { obj, mid: remote.mid, seq, remote: remote.clone(), hdrop: None });
                                Ok(seq)
                            }
                            Err(e) if busy(&e) => Err(crate::clip::WriteError::Busy),
                            Err(e) => {
                                crate::log!("文件：OleSetClipboard 失败：{e}");
                                Err(crate::clip::WriteError::Failed)
                            }
                        },
                        Err(e) => {
                            crate::log!("文件：建数据对象失败：{e}");
                            Err(crate::clip::WriteError::Failed)
                        }
                        }
                    };
                    if let Some(h) = &hub {
                        h.to_desktop(ToDesktop::FilesWritten(Written { g, ver, tries, remote, r }));
                    }
                }
                Cmd::AddHdrop { mid, hdrop } => {
                    let Some(c) = cur.as_ref() else { continue };
                    // SAFETY：只读查询
                    let same = c.mid == mid && unsafe { GetClipboardSequenceNumber() } == c.seq && is_current(c);
                    if !same {
                        continue; // 用户已复制了别的
                    }
                    let remote = c.remote.clone();
                    match make(remote.clone(), Some(hdrop.clone())).and_then(|obj| unsafe { OleSetClipboard(&obj) }.map(|_| obj)) {
                        Ok(obj) => {
                            // SAFETY：只读查询
                            let seq = unsafe { GetClipboardSequenceNumber() };
                            cur = Some(Cur { obj, mid, seq, remote, hdrop: Some(hdrop) });
                            crate::log!("文件：已预取完，剪贴板加上了真文件路径（微信 / QQ 也能粘贴）");
                            if let Some(h) = &hub {
                                h.to_desktop(ToDesktop::FilesSeq(seq));
                            }
                        }
                        Err(e) => crate::log!("文件：加 CF_HDROP 失败：{e}"),
                    }
                }
                Cmd::Clear => {
                    if let Some(c) = cur.take() {
                        if is_current(&c) {
                            // SAFETY：清掉自己放的
                            let _ = unsafe { OleSetClipboard(None) };
                        }
                    }
                }
                Cmd::Shutdown(done) => {
                    if let Some(c) = cur.take() {
                        if is_current(&c) {
                            match &c.hdrop {
                                Some(d) if !hwnd.0.is_null() => {
                                    // 立即数据：进程退出后照样能粘贴
                                    if crate::clip::write_hdrop(hwnd, d).is_err() {
                                        let _ = unsafe { OleSetClipboard(None) };
                                    }
                                }
                                // SAFETY：清掉自己放的（只有虚拟文件，退出后没人能提供内容）
                                _ => {
                                    let _ = unsafe { OleSetClipboard(None) };
                                }
                            }
                        }
                    }
                    let _ = done.send(());
                }
            }
        }
        // SAFETY：标准消息循环（OLE 的剪贴板窗口在本线程上）
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break;
        }
        if msg.message != WM_APP_WAKE {
            // SAFETY：同上
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
