//! kmlink：两台 Windows 电脑共用一套键鼠 + 剪贴板同步。单进程、无 Electron。
//!
//! 用法：
//!   kmlink.exe              运行（托盘）
//!   kmlink.exe --console    运行并把日志打到控制台
//!   kmlink.exe settings     打开设置窗口（排布、对方 IP、配对密钥）
//!   kmlink.exe keygen       打印一个新密钥
//!   kmlink.exe guard <pid>  （内部）主程序退出后恢复系统光标

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clip;
mod config;
mod crypto;
mod cursor;
mod desktop;
mod elev;
mod geom;
mod hub;
mod inject;
mod input;
mod log;
mod net;
mod proto;
mod settings;
mod sys;

use std::sync::Arc;
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AllocConsole, AttachConsole};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::w;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("guard") => {
            if let Some(pid) = args.get(2).and_then(|s| s.parse().ok()) {
                cursor::guard_main(pid);
            }
        }
        Some("settings") => {
            sys::dpi_aware();
            log::init("settings");
            settings::run();
        }
        Some("keygen") => {
            console();
            println!("{}", config::new_key_hex());
        }
        _ => daemon(args.iter().any(|a| a == "--console")),
    }
}

fn console() {
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            let _ = AllocConsole();
        }
    }
}

fn daemon(with_console: bool) {
    if with_console {
        console();
        log::CONSOLE.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    sys::dpi_aware();
    log::init("kmlink");

    // 只允许一个实例
    unsafe {
        let _m = CreateMutexW(None, true, w!("Local\\kmlink-daemon"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            log!("已经在运行，退出");
            log::flush(500);
            return;
        }
        std::mem::forget(_m);
        let _ = timeBeginPeriod(1);
    }

    let (cfg, first_run) = config::Config::load_or_create();
    log!("kmlink {} 启动：本机 {}，端口 {}，对方 {}", env!("CARGO_PKG_VERSION"), cfg.name, cfg.port, if cfg.peer.is_empty() { "（等对方连接）" } else { &cfg.peer });

    // 上次若异常退出，光标可能还是透明的
    cursor::show();
    cursor::spawn_guard();

    let (hub, rx) = hub::Hub::new(cfg);
    let hub = Arc::new(hub);
    let injector = Arc::new(inject::Injector::new(hub.clone()));
    desktop::spawn(hub.clone(), rx.desktop, first_run);
    net::spawn(hub.clone(), rx.net, injector.clone());
    input::run(hub.clone(), rx.input); // 阻塞到“退出”

    injector.release_all();
    cursor::show();
    log!("退出");
    log::flush(500);
    std::process::exit(0);
}
