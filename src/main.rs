#![cfg_attr(windows, windows_subsystem = "windows")]
#![allow(
    non_snake_case,
    reason = "The executable uses the OpenUUYC product name."
)]

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use openuuyc::account::api;
use openuuyc::account::client::AuthenticatedClient;
use openuuyc::account::login;
use openuuyc::application::app;
use openuuyc::diagnostics::logging;
use openuuyc::media;
use openuuyc::session::controller;

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
    #[cfg(windows)]
    #[command(hide = true)]
    Notification { uri: String },
    /// 打开卸载窗口，选择是否保留驱动和本机数据
    #[cfg(windows)]
    Uninstall {
        #[arg(long, hide = true)]
        parent: Option<u32>,
    },
    #[cfg(windows)]
    #[command(hide = true)]
    Service,
    #[cfg(windows)]
    #[command(hide = true)]
    HostResident {
        #[arg(long)]
        parent: u32,
    },
    #[cfg(windows)]
    #[command(hide = true)]
    DisplayAgent {
        #[arg(long)]
        parent: u32,
    },
    #[cfg(windows)]
    #[command(hide = true)]
    InputAgent {
        #[arg(long)]
        pipe: String,
        #[arg(long)]
        parent: u32,
    },
    #[cfg(windows)]
    #[command(hide = true)]
    CaptureAgent {
        #[arg(long)]
        pipe: String,
        #[arg(long)]
        parent: u32,
    },
    #[cfg(windows)]
    #[command(hide = true)]
    Component {
        #[arg(value_enum)]
        component: openuuyc::application::ComponentKind,
        #[arg(value_enum)]
        operation: openuuyc::application::ComponentOperation,
        #[arg(long)]
        allow_sas: bool,
        #[arg(long)]
        owner: Option<String>,
        #[command(flatten)]
        removal: openuuyc::application::RemovalOptions,
    },
    #[command(hide = true)]
    DisplayRecovery { token: String },
    #[command(hide = true)]
    PluginVideoHost,

    #[command(hide = true)]
    PluginHost { manifest: PathBuf },
    /// 打开完整图形设备中心
    Gui {
        #[arg(long)]
        background: bool,
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
    /// 恢复保存的登录态，或进行二维码登录
    Login,
    /// 以适合脚本处理的文本格式列出设备
    Devices,
    /// 打开指定设备的观看窗口，关闭窗口或按 Ctrl+C 退出
    Connect {
        /// 设备的完整名称（必须唯一且完全匹配）
        #[arg(required_unless_present = "device_id", conflicts_with = "device_id")]
        device: Option<String>,
        /// 按已核实的设备 ID 选择目标，避免重复或变化的别名选错设备
        #[arg(long)]
        device_id: Option<String>,
        /// 静音启动，只影响本地播放
        #[arg(long)]
        mute: bool,
        /// 仅接收远端声音，不显示或控制画面
        #[arg(long)]
        audio_only: bool,
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
}

fn main() -> Result<()> {
    let parsed = Cli::try_parse();

    if !parsed
        .as_ref()
        .is_ok_and(|cli| cli.command.as_ref().is_some_and(internal_role))
    {
        attach_parent_console();
    }
    let cli = parsed.unwrap_or_else(|error| error.exit());
    let command = cli.command.unwrap_or(Commands::Gui {
        background: false,
        fps: media::FrameRateChoice::Auto,
        codec: media::CodecPreference::Auto,
        hardware_decode: true,
        transport: media::TransportChoice::Auto,
    });

    #[cfg(windows)]
    if let Commands::Notification { uri } = &command {
        return app::notification_activation(uri);
    }
    if matches!(command, Commands::Gui { .. }) && openuuyc::application::route_installed_gui()? {
        return Ok(());
    }
    let _instance = if matches!(command, Commands::Gui { .. }) {
        match app::instance::acquire()? {
            Some(instance) => Some(instance),
            None => return Ok(()),
        }
    } else {
        None
    };
    #[cfg(windows)]
    let uninstalling = matches!(command, Commands::Uninstall { .. });
    #[cfg(not(windows))]
    let uninstalling = false;
    let _logging = if uninstalling {
        None
    } else {
        Some(logging::init(
            cli.log_level.as_deref(),
            cli.log_file.as_deref(),
        )?)
    };
    tracing::info!(target: "openuuyc", version = env!("CARGO_PKG_VERSION"), "application started");

    #[cfg(windows)]
    if let Commands::Component {
        component,
        operation,
        allow_sas,
        owner,
        removal,
    } = command
    {
        let result = openuuyc::application::component_operation(
            component,
            operation,
            allow_sas,
            owner.as_deref(),
            removal,
        );
        let reboot = match result {
            Ok(reboot) => reboot,
            Err(error) => {
                drop(_logging);
                if let Some(code) = openuuyc::application::component_error_code(&error) {
                    std::process::exit(code);
                }
                return Err(error);
            }
        };
        drop(_logging);
        if removal.remove_data && !reboot {
            openuuyc::application::purge_machine_data()?;
        }
        if reboot {
            std::process::exit(3010);
        }
        return Ok(());
    }
    let result = match command {
        #[cfg(windows)]
        Commands::Uninstall { parent } => openuuyc::application::uninstall_application(parent),
        #[cfg(windows)]
        Commands::Component { .. } => unreachable!(),
        #[cfg(windows)]
        Commands::Service => openuuyc::application::host_service(),
        #[cfg(windows)]
        Commands::HostResident { parent } => openuuyc::application::host_resident(parent),
        #[cfg(windows)]
        Commands::DisplayAgent { parent } => openuuyc::application::display_agent(parent),
        #[cfg(windows)]
        Commands::InputAgent { pipe, parent } => openuuyc::application::input_agent(&pipe, parent),
        #[cfg(windows)]
        Commands::CaptureAgent { pipe, parent } => {
            openuuyc::application::capture_agent(&pipe, parent)
        }
        Commands::DisplayRecovery { token } => openuuyc::application::display_recovery(&token),
        Commands::PluginVideoHost => openuuyc::plugins::video::host(),
        #[cfg(windows)]
        Commands::Notification { .. } => {
            unreachable!("notification activation is handled before application startup")
        }

        Commands::PluginHost { manifest } => openuuyc::plugins::host(&manifest),
        Commands::Gui {
            background,
            fps,
            codec,
            hardware_decode,
            transport,
        } => app::run(app::GuiOptions {
            background,
            media: media::ConnectionMediaOptions {
                audio_only: false,
                muted: false,
                frame_rate: fps,
                codec,
                hardware_decode,
                transport,
            },
        }),
        Commands::Login => {
            tokio::runtime::Runtime::new()?.block_on(login::interactive_login())?;
            Ok(())
        }
        Commands::Devices => tokio::runtime::Runtime::new()?.block_on(print_devices()),
        Commands::Connect {
            device,
            device_id,
            mute,
            audio_only,
            fps,
            codec,
            hardware_decode,
            transport,
        } => tokio::runtime::Runtime::new()?.block_on(connect_device(
            device,
            media::ConnectionMediaOptions {
                audio_only,
                muted: mute,
                frame_rate: fps,
                codec,
                hardware_decode,
                transport,
            },
            device_id,
        )),
    };
    if let Err(error) = &result {
        tracing::error!(target: "openuuyc", error = %format_args!("{error:#}"), "application stopped with an error");
    }
    drop(_instance);
    #[cfg(windows)]
    if result.is_ok() && openuuyc::application::take_installed_handoff() {
        openuuyc::application::launch_installed(std::env::args_os().skip(1))?;
    }
    result
}

/// Roles the executable starts itself in, which report to their parent
/// rather than to a console of their own.
fn internal_role(command: &Commands) -> bool {
    match command {
        Commands::PluginHost { .. }
        | Commands::PluginVideoHost
        | Commands::DisplayRecovery { .. } => true,
        #[cfg(windows)]
        Commands::Notification { .. }
        | Commands::Component { .. }
        | Commands::Service
        | Commands::HostResident { .. }
        | Commands::DisplayAgent { .. }
        | Commands::InputAgent { .. }
        | Commands::CaptureAgent { .. } => true,
        _ => false,
    }
}

fn attach_parent_console() {
    // Attach before printing clap output. Explorer has no parent console;
    // inherited STARTF_USESTDHANDLES pipes/files remain redirected on attach.
    // Never allocate a console just for launching the device center or viewer.
    #[cfg(windows)]
    {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
    // A Linux process is started from its terminal and keeps those streams.
}

async fn connect_device(
    device: Option<String>,
    options: media::ConnectionMediaOptions,
    device_id: Option<String>,
) -> Result<()> {
    controller::run_saved_viewer_window(
        device.unwrap_or_else(|| device_id.clone().expect("clap requires a target")),
        options,
        device_id,
    )
    .await
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
        "{prefix}{} [{}] — {}，{}，可控 {}，会话参与者 {}",
        device.alias,
        device.device_id,
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
