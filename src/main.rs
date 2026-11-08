//! kmlink：两台 Windows 电脑共用一套键鼠 + 剪贴板同步。单进程、无 Electron。
//!
//! 用法：
//!   kmlink.exe              运行（托盘）
//!   kmlink.exe --console    运行并把日志打到控制台
//!   kmlink.exe settings     打开设置窗口（排布、对方 IP、配对密钥）
//!   kmlink.exe keygen       打印一个新密钥
//!   kmlink.exe guard <pid>  （内部）主程序退出后恢复系统光标

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod audio_cap;
mod audio_play;
mod audio_probe;
mod cable;
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
mod jitter;
mod island;
mod log;
mod net;
mod panel;
mod proto;
mod settings;
mod smooth;
mod sys;
mod ui;
mod update;

use std::sync::Arc;
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AllocConsole, AttachConsole};

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
            ui::load_fonts();
            settings::run();
        }
        Some("audio-probe") => {
            console();
            audio_probe::run();
        }
        // 测试工具：kmlink audio-rec <pid> <秒> <输出.csv> / kmlink audio-tone <秒> [频率] [幅度]
        Some("audio-rec") => {
            console();
            audio_probe::rec(&args[2..]);
        }
        Some("audio-sim") => {
            console();
            audio_probe::sim(&args[2..]);
        }
        Some("audio-tone") => {
            console();
            audio_probe::tone(&args[2..]);
        }
        // 调试截图：kmlink panel-demo <状态> [秒]
        Some("panel-demo") => {
            sys::dpi_aware();
            ui::load_fonts();
            panel::demo(args.get(2).map(String::as_str).unwrap_or("idle"), args.get(3).and_then(|s| s.parse().ok()).unwrap_or(6));
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
    ui::load_fonts();

    // 刚被旧版本更新换上来：先等旧进程退出（它还占着单实例互斥量）
    let updated_from = update::after_update(&std::env::args().collect::<Vec<_>>());
    // 只允许一个实例（另一个以管理员身份运行时也算）
    if sys::already_running(sys::DAEMON_MUTEX, true) {
        log!("已经在运行，退出");
        log::flush(500);
        return;
    }
    unsafe {
        let _ = timeBeginPeriod(1);
        no_power_throttling();
    }
    sys::raise_process_priority();

    let (cfg, first_run) = config::Config::load_or_create();
    if let Some(old) = &updated_from {
        log!("更新：{old} → {}", update::VERSION);
    }
    log!("kmlink {} 启动：本机 {}，端口 {}，对方 {}", env!("CARGO_PKG_VERSION"), cfg.name, cfg.port, if cfg.peer.is_empty() { "（等对方连接）" } else { &cfg.peer });

    // 上次若异常退出，光标可能还是透明的
    cursor::show();
    cursor::spawn_guard();

    let (hub, rx) = hub::Hub::new(cfg);
    let hub = Arc::new(hub);
    let injector = Arc::new(inject::Injector::new(hub.clone()));
    injector.start();
    desktop::spawn(hub.clone(), rx.desktop, first_run);
    let audio = audio::Port::new(hub.clone());
    audio::spawn(audio.clone());
    net::spawn(hub.clone(), rx.net, injector.clone(), audio.clone());
    update::spawn(hub.clone(), updated_from);
    input::run(hub.clone(), rx.input); // 阻塞到“退出”

    injector.release_all();
    audio.shutdown();

    cursor::show();
    desktop::save_exit_status(&hub);
    log!("退出");
    log::flush(500);
    std::process::exit(0);
}

/// Windows 11 对没有可见窗口的后台进程：会忽略 timeBeginPeriod（计时退回 15.6ms），还可能放进“效率模式”
/// （EcoQoS，排到小核、降频）。这两样都会让鼠标转发变钝，明确声明本进程不要这两种节能限制。
fn no_power_throttling() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling,
        SetProcessInformation,
    };
    let st = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0, // 0 = 这两项都“不节流”
    };
    let r = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &st as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    if let Err(e) = r {
        log!("关闭电源节流失败：{e}");
    }
}
