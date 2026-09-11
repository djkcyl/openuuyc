#![cfg_attr(windows, windows_subsystem = "windows")]
#![allow(
    non_snake_case,
    reason = "The executable uses the OpenUUYC product name."
)]

use std::{path::PathBuf, time::Duration};

use anyhow::Result;
use clap::{Parser, Subcommand};
use openuuyc::{
    api, app, client::AuthenticatedClient, controller, logging, login, media, rtc, signal,
};

#[derive(Parser)]
#[command(
    name = "OpenUUYC",
    version,
    about = "OpenUUYC — 第三方 UU 远程协议兼容客户端"
)]
struct Cli {
    /// 日志过滤器，例如 info、debug、trace 或 openuuyc=trace
    #[arg(long, global = true)]
    log_level: Option<String>,
    /// 指定诊断日志文件（默认写入用户日志目录并自动轮转）
    #[arg(long, global = true)]
    log_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// 打开完整图形设备中心
    Gui {
        /// 自动刷新设备列表的间隔秒数（最小 2 秒）
        #[arg(
            long,
            default_value_t = 5,
            value_parser = clap::value_parser!(u64).range(2..)
        )]
        refresh_seconds: u64,
        /// 初始码流帧率：auto、144、90、60 或 30
        #[arg(long, default_value = "auto")]
        fps: media::FrameRateChoice,
        /// 初始视频编码：auto、h264 或 h265
        #[arg(long, default_value = "auto")]
        codec: media::CodecPreference,
        /// 是否优先使用平台原生硬件解码器
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        hardware_decode: bool,
        /// 传输策略：auto、p2p 或 relay
        #[arg(long, default_value = "auto")]
        transport: media::TransportChoice,
    },
    /// 显示原生传输实现状态
    NativeStatus,
    /// 在本地创建 WebRTC offer 并检查媒体能力
    RtcSelftest,
    /// 查看平台凭据存储与本地登录态
    AuthStatus,
    /// 恢复保存的登录态，或进行二维码登录
    Login,
    /// 仅删除本地登录会话，保留虚拟设备身份
    LogoutLocal,
    /// 显示当前已知的 REST 合约
    Contracts,
    /// 以适合脚本处理的文本格式列出设备
    Devices,
    /// 连接指定设备并保持会话，按 Ctrl+C 正常退出
    Connect {
        /// 设备的完整名称（必须唯一且完全匹配）
        device: String,
        #[arg(long, hide = true)]
        owner_control: Option<String>,
        /// 按已核实的设备 ID 选择目标，避免重复或变化的别名选错设备
        #[arg(long)]
        device_id: Option<String>,
        /// Local GUI child payload; never put a device verification code in argv.
        #[arg(long, hide = true, conflicts_with = "device_id")]
        assist_stdin: bool,
        /// 静音启动，只影响本地播放
        #[arg(long)]
        mute: bool,
        /// 码流帧率：auto、144、90、60 或 30
        #[arg(long, default_value = "auto")]
        fps: media::FrameRateChoice,
        /// 视频编码：auto、h264 或 h265
        #[arg(long, default_value = "auto")]
        codec: media::CodecPreference,
        /// 是否优先使用平台原生硬件解码器
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        hardware_decode: bool,
        /// 传输策略：auto、p2p 或 relay
        #[arg(long, default_value = "auto")]
        transport: media::TransportChoice,
    },
    /// 检查解密 RTP 捕获的流、包数量与原始字节可重放性
    RtpCaptureInfo { path: PathBuf },
    /// 通过官方同构接收流水线离线回放解密 RTP 捕获
    RtpReplay {
        path: PathBuf,
        /// 丢弃所有在捕获中有 RTX 副本的原包，用于验证恢复链的字节一致性
        #[arg(long)]
        drop_repairable_originals: bool,
    },
}

fn main() -> Result<()> {
    #[cfg(windows)]
    attach_parent_console();
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Commands::Gui {
        refresh_seconds: 5,
        fps: media::FrameRateChoice::Auto,
        codec: media::CodecPreference::Auto,
        hardware_decode: true,
        transport: media::TransportChoice::Auto,
    });

    #[cfg(windows)]
    let _instance = if matches!(command, Commands::Gui { .. }) {
        match app::instance::acquire()? {
            Some(instance) => Some(instance),
            None => return Ok(()),
        }
    } else {
        None
    };
    let _logging = logging::init(cli.log_level.as_deref(), cli.log_file.as_deref())?;
    tracing::info!(target: "openuuyc", version = env!("CARGO_PKG_VERSION"), "application started");

    let result = match command {
        Commands::Gui {
            refresh_seconds,
            fps,
            codec,
            hardware_decode,
            transport,
        } => app::run(app::GuiOptions {
            refresh_interval: Duration::from_secs(refresh_seconds),
            media: media::ConnectionMediaOptions {
                muted: false,
                frame_rate: fps,
                codec,
                hardware_decode,
                transport,
            },
        }),
        Commands::NativeStatus => {
            let sample = signal::encode_event("soac", &[], Some(1))?;
            let _ = signal::decode(&sample)?;
            let _ = signal::encode_pong("");
            println!("transport: native HTTPS + Socket.IO + WebRTC/DTLS-SRTP");
            println!(
                "media: decrypted RTP -> complete Annex-B frames -> native platform decode -> Rust GUI"
            );
            println!(
                "decode backends: Windows Rust DXVA11 / Rust H.264 software, macOS VideoToolbox, Linux VA-API"
            );
            println!("signal events: {}", signal::KNOWN_EVENTS.join(", "));
            println!(
                "signal headers: {}, {}, {}",
                signal::AUTH_HEADER,
                signal::RECONNECT_HEADER,
                signal::CONTROLLING_HEADER
            );
            println!("official DLL dependency: none");
            Ok(())
        }
        Commands::RtcSelftest => tokio::runtime::Runtime::new()?.block_on(async {
            let peer = rtc::NativePeer::new(Vec::new(), media::TransportChoice::Auto).await?;
            let _tracks = peer
                .install_rtp_forwarder(rtc::RtpForwardConfig::default())
                .await?;
            let sdp = peer.create_offer().await?;
            println!("H.265: {}", sdp.contains("H265/90000"));
            println!("H.264: {}", sdp.contains("H264/90000"));
            println!("Opus: {}", sdp.contains("opus/48000/2"));
            println!("SCTP: {}", sdp.contains("webrtc-datachannel"));
            peer.close().await
        }),
        Commands::AuthStatus => login::auth_status(),
        Commands::Login => {
            tokio::runtime::Runtime::new()?.block_on(login::interactive_login())?;
            Ok(())
        }
        Commands::LogoutLocal => login::clear_local_session(),
        Commands::Contracts => {
            for (name, contract) in api::CONTRACTS {
                println!("{name:<22} {:?} {}", contract.method, api::url(contract));
            }
            Ok(())
        }
        Commands::Devices => tokio::runtime::Runtime::new()?.block_on(print_devices()),
        Commands::Connect {
            device,
            owner_control,
            device_id,
            assist_stdin,
            mute,
            fps,
            codec,
            hardware_decode,
            transport,
        } => tokio::runtime::Runtime::new()?.block_on(connect_device(
            device,
            media::ConnectionMediaOptions {
                muted: mute,
                frame_rate: fps,
                codec,
                hardware_decode,
                transport,
            },
            owner_control,
            device_id,
            assist_stdin,
        )),
        Commands::RtpCaptureInfo { path } => {
            print!("{}", openuuyc::rtp_capture::inspect_capture(path)?);
            Ok(())
        }
        Commands::RtpReplay {
            path,
            drop_repairable_originals,
        } => {
            print!(
                "{}",
                openuuyc::official_receiver::replay_capture(
                    path,
                    openuuyc::official_receiver::ReplayOptions {
                        drop_repairable_originals,
                    },
                )?
            );
            Ok(())
        }
    };
    if let Err(error) = &result {
        tracing::error!(target: "openuuyc", error = %format_args!("{error:#}"), "application stopped with an error");
    }
    result
}

#[cfg(windows)]
fn attach_parent_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

    // Run before clap/stdout initialization. Explorer has no parent console;
    // inherited STARTF_USESTDHANDLES pipes/files remain redirected on attach.
    // Never allocate a console just for launching the device center or viewer.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

async fn connect_device(
    device: String,
    options: media::ConnectionMediaOptions,
    owner: Option<String>,
    device_id: Option<String>,
    assist_stdin: bool,
) -> Result<()> {
    if assist_stdin {
        let request = openuuyc::assist::read_launch_request()?;
        return controller::run_assist_viewer_window(device, request, options, owner).await;
    }
    controller::run_saved_viewer_window(device, options, owner, device_id).await
}

async fn print_devices() -> Result<()> {
    let client = AuthenticatedClient::from_saved_session()?;
    let devices = client.list_devices().await;
    client.close().await;
    let devices = devices?;

    println!("当前虚拟设备：");
    print_device(None, &devices.current_device);
    println!("\n我的设备：");
    for (index, device) in devices.my_binded_devices.iter().enumerate() {
        print_device(Some(index + 1), device);
    }
    if devices.my_binded_devices.is_empty() {
        println!("  （无）");
    }
    Ok(())
}

fn print_device(index: Option<usize>, device: &api::DeviceInfo) {
    let prefix = index.map_or_else(|| "  ".to_owned(), |value| format!("  [{value}] "));
    println!(
        "{prefix}{} — {}，{}，可控 {}，会话参与者 {}",
        device.alias,
        device.status_label(),
        device.platform_label(),
        if device.controlled_support && device.controllable {
            "是"
        } else {
            "否"
        },
        device.participant_count()
    );
}
