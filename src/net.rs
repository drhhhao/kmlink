//! 网络：一个会话 = 一条 TCP（可靠、加密、分帧）+ 一个 UDP 端口（鼠标位置）。
//!
//! 线程：
//! - listener：TcpListener 绑 0.0.0.0:cfg.port；dialer：cfg.peer 有值且当前没有会话时，每 2 秒（加 0~500ms 抖动）连一次，超时 2 秒。
//! - 每个会话：reader 线程（读帧、分发）+ writer 线程（从 hub 的 net 通道取 Out，可靠消息优先，剪贴板分片低优先级穿插）。
//! - udp：UdpSocket 绑 0.0.0.0:cfg.port，一个接收线程；发送在 writer 里做（Out::Move 只发 channel 里最新的那个）。
//!
//! 握手见 crypto.rs。之后双方先发 Hello（sys::monitors()、cfg.name、udp_port = cfg.port），再发 Layout（本机配置里的排布与 rev）。
//! TCP：set_nodelay(true)。读超时 6 秒；每 1 秒发 Ping；6 秒收不到任何帧就断开。
//! 同时只保留一个会话：新会话认证成功时，若旧会话不到 2 秒，保留 nonce_c 字典序更大的那个（两边会选同一个），否则用新的替换旧的。
//!
//! 收到的消息：
//!   Hello → config::save_peer_desk；hub.to_input(PeerUp{name, desk})；hub.set_status(connected=true, peer_name, peer_addr)
//!   Layout{rev 比本机新} → 镜像（Layout::mirrored）后写回 hub.cfg 和 config.toml（rev 保持对方的值），再 hub.to_input(Config(..))
//!   Enter/Leave/Button/Wheel/Key → injector
//!   Yield → hub.to_input(Yield)
//!   ClipText → hub.to_desktop(ClipText)；ClipImage + 分片 → 拼齐后 miniz_oxide 解压（上限 raw_len ≤ max_image_mb）→ hub.to_desktop(ClipImage)
//!   Ping → 回 Pong
//! 会话结束：injector.release_all()；hub.to_input(PeerDown)；hub.set_status(connected=false)。
//! UDP：对方地址 = TCP 对端 IP + Hello 里的 udp_port；之后以最近一个认证通过的 UDP 包源地址为准。每秒发 UdpMsg::Hi。
//!   收到 Move → injector.moved。没有会话时丢弃所有 UDP。
//! 配置变化（desktop 线程重新加载后会 send(Msg::Layout)），net 只管转发。
//! 发送端 Out::Msg 在没有会话时直接丢弃（不排队），Out::ClipImage 同样丢弃。

use crate::hub::{Hub, Out};
use crate::inject::Injector;
use crossbeam_channel::Receiver;
use std::sync::Arc;

pub fn spawn(hub: Arc<Hub>, rx: Receiver<Out>, injector: Arc<Injector>) {
    let _ = (hub, rx, injector);
    todo!("net::spawn")
}
