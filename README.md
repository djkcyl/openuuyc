# OpenUUYC

![OpenUUYC](assets/banner.png)

OpenUUYC 是用 Rust 编写的 UU 远程第三方客户端，支持 Windows 和 Linux，支持主控、被控和远程协助。

Windows 1.0 已进入 **RC** 阶段，重点完善兼容性、恢复和稳定性。**Windows 正式版完成后会适配 Linux 等其他平台。** 提前移植建议先通过 [Issue](https://github.com/djkcyl/openuuyc/issues) 沟通。

## 下载与使用

从 [Releases](https://github.com/djkcyl/openuuyc/releases) 下载 Windows x64 客户端，扫码或短信登录后连接设备，支持 UU 官方客户端；无需登录也可在登录页开启“接受远程协助”。

最新稳定版为 [v0.7.0](https://github.com/djkcyl/openuuyc/releases/tag/v0.7.0)，最新预发布为 [v1.0.0-rc.2](https://github.com/djkcyl/openuuyc/releases/tag/v1.0.0-rc.2)。

接收测试版更新提醒：在“关于”中开启“允许测试版”（默认关闭）。

**从 alpha.6 或更早版本升级需重新登录，设备 ID 会改变，旧设备条目需手动删除。**

## 能力表

以下对应 **v1.0.0-rc.2** 在 **Windows x64** 上的能力，功能取决于双方能力与权限；Linux x64 需自行构建，差异见[下文](#linux-版差异)。

| 功能 | 主控端 | 被控端 | 说明 |
| --- | --- | --- | --- |
| 同账号远程连接 | 支持同时连接多台设备 | 支持 | 本机在连接设置中开启“允许被控” |
| 设备 ID / 验证码协助 | 支持主动连接 | 支持账号及游客协助 | 随机 / 自定义密码、本机确认；主控支持最近连接和收藏 |
| 画面传输 | 接收与播放 | 采集与编码 | H.264 / H.265 / AV1、真彩及 HDR，可调画质、码率和帧率；AV1 与 SDR 自动 10 位需双方 OpenUUYC 支持 |
| 编解码 | DXVA11 硬解 / Rust 软解 | NVENC、AMF、QSV 硬编 / Rust H.264 软编 | 可选格式与显卡首选；软解支持 H.264 / AV1，整个客户端最多一个软解播放窗口 |
| 键鼠与触摸 | 键盘、鼠标控制 | 键鼠及移动端原生触摸接收 | 相对/绝对鼠标、组合键、光标同步；可选 125 / 500 / 1000 Hz 鼠标节流（默认关闭） |
| 多显示器与显示设置 | 切屏、多窗口、分辨率及 DPI 调整 | 多屏采集与设置接收 | 屏幕标签可拖出独立窗口，显示设置按目标支持的配置应用 |
| 虚拟屏、超级屏与无屏接管 | 支持操作远端 | 支持 | 需安装显示驱动；自动兜底屏断线保留，其他可用屏幕接入后回收 |
| 桌面声音 | 播放与远程音质调整 | 采集与发送 | 默认或指定设备，设备变化后恢复；64 / 128 / 192 / 256 kbps，远程调音质需双方 alpha.6+ |
| 仅音频连接 | 支持 | 支持 | 默认 256 kbps，提供波形、电平、网络 RTT、精简模式，可恢复画面 |
| 麦克风与虚拟声卡 | 选择麦克风、调整音质并发送 | 虚拟扬声器及远程麦克风接收 | 被控端需选装虚拟音频驱动，可配置默认音频设备切换策略 |
| 剪贴板与文件拖放 | 支持 | 支持 | 文字、富文本、图片及文件复制；OpenUUYC 双端支持双向拖放，与官方端连接时使用其支持的发送方式 |
| 独立文件传输 | 支持 | 支持官方 Windows / Android / iOS 收发 | 目录浏览、收发、文件管理及暂停续传；被控端需登录 Windows |
| 端口转发 | 支持 | 支持 | TCP 与官方互通；UDP 需两端 OpenUUYC，复用可靠通道；被控需开启许可 |
| 批注与指示工具 | 支持发送 | 支持接收 | 画笔、形状、撤销重做、白板、激光笔和鼠标指示；被控端需登录 Windows |
| 远程电源操作 | 开机、关机和重启 | 关机、重启、远程开机设置与局域网唤醒协助 | 提供有线网卡检查/配置；WoL 需 BIOS 开启及同网协助设备或已配置的 UU 路由器 |

被控通知可选程序浮窗或 Windows 通知，支持连接确认、断开及文件收发提醒。

向官方被控端拖入文件，需开启键鼠控制、剪贴板同步和文件复制。

另支持设备管理、自定义快捷键、本机诊断、性能监控、异常图标及[插件和节点图](plugins/README.md)。

支持本机导出及远端获取脱敏诊断包（ZIP / zstd，最多 32 MiB）；远端获取需双方 beta.3+，并开启被控端文件访问权限。

## 安装与服务

主页“安装服务”会将程序安装到 `Program Files\OpenUUYC`，安装后台服务、输入和显示驱动，创建快捷方式并配置托盘自启动。安装需管理员授权及驱动证书信任；关闭主窗口进入托盘，托盘“退出”结束运行。

安装服务并开启被控后，支持锁屏、PIN 界面及重启后未登录 Windows 时接入。

更新时运行新版 EXE，点击“更新并打开”即可更新已有安装。虚拟声卡在“连接设置 → 服务管理”中选装；卸载可选择保留虚拟显示驱动、虚拟声卡及用户数据。

### 虚拟音频驱动的使用条件

**虚拟音频驱动使用测试签名，尚未取得 Microsoft 正式签名。** 使用虚拟扬声器或接收远程麦克风，需要手动准备测试启动环境；普通画面、键鼠和桌面声音不需要此设置。

- 临时测试：Shift + 重启 → 疑难解答 → 高级选项 → 启动设置 → 重启 → **7 / F7 禁用驱动程序强制签名**，仅本次启动有效。[Windows 说明](https://support.microsoft.com/en-us/windows/experience/startup-boot/windows-startup-settings)
- 持续测试：管理员执行 `bcdedit /set testsigning on` 后重启；恢复时执行 `bcdedit /set testsigning off` 并重启。Secure Boot、BitLocker 或组织策略可能限制操作。[微软说明](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/the-testsigning-boot-configuration-option)

测试启动会降低驱动加载保护，程序不会自动修改这些设置。

## Linux 版差异

Linux 版可以登录、管理设备、观看和控制远端桌面，界面走 wgpu（Vulkan，缺失时回退 OpenGL），X11 与 Wayland 均可运行。本机被控在 Xorg 会话中可用。与 Windows 版相比：

- **解码**：H.264 通过 VA-API 硬件解码（Constrained Baseline、Main、High，8 位 4:2:0），其余 H.264 格式与 AV1 使用 Rust 软件解码；AV1 软解只声明 8 位 4:2:0（播放窗口按 NV12 绘制），10 位与 4:4:4 不向对端声明。H.265 暂不支持。双方都有硬件编解码时协商优先选硬件。硬件解码需要对应的 VA-API 驱动。编解码首选中的解码方式与编码方式（硬件优先、仅硬件、仅软件）可用，但不能指定显卡：VA-API 与 NVENC 都使用桌面所在的显卡，显卡一栏显示为“自动”。
- **剪贴板**：主控与被控两端都支持文字、图片与文件双向同步。粘贴远端复制的文件时，文件通过挂载在 `$XDG_RUNTIME_DIR` 下的只读 FUSE 文件系统按需读取，需要安装 `fuse3`。文件拖放在 Xorg 会话中可用，经 XDND 协议完成：作为主控，本地文件可拖进播放窗口（OpenUUYC 对端为实时拖放，官方被控为松手后投放），也可从 OpenUUYC 被控的桌面拖出到本地桌面，官方被控用队列浮窗发来的文件保存到下载目录；作为被控，主控拖入的文件在落点处投放给本机程序，向官方 Windows 主控发送文件时，在桌面上拖动文件会于屏幕顶部弹出待发送队列浮窗。远端文件以 FUSE 挂载的路径交给目标程序，读取时才按需传输；目标读完全部文件即视为完成，五分钟未读取则停止。Wayland 会话不支持文件拖放（提示“Wayland 桌面暂不支持拖放文件”）。拖出到本地或远端后不能再拖回原窗口接续原拖动（Windows 版经 OLE 支持），Linux 端不向对端声明这项能力，两端都把拖出视为结束；X11 拖放也没有拖动预览图。
- **被控文件传输与电源**：独立文件传输在本机桌面用户身份下运行，路径为 Unix 路径，根目录列出 `/` 与 xdg 用户目录（文档、桌面、下载等），不允许经过符号链接。远程关机与重启经 systemd-logind 执行，受 polkit 策略约束（有其他用户登录时可能需要管理员授权）。远程开机的网络登记、局域网唤醒协助与网卡检查可用（检查只读：唤醒权限取自 sysfs，魔术包状态需安装 `ethtool`）；配置网卡需要 root，程序内不提供，请用 `sudo ethtool -s <网卡> wol g` 设置；开启远程开机时写入 `~/.config/autostart/openuuyc.desktop` 登录自启动。
- **本机被控**：在已登录的桌面内运行。画面采集按 Sunshine 的优先级选择：Xorg 下先用 NVIDIA NvFBC（画面直接抓进显存交给 NVENC，仅在 NVENC 可用时选用），再到 X11 MIT-SHM，最后是 XDG 屏幕共享门户（PipeWire）；Wayland 下只用门户。门户第一次使用时需要在本机屏幕上点“共享”，授权会被记住（`~/.local/share/OpenUUYC/screencast-restore-token`，在系统隐私设置中可撤销）；可用环境变量 `OPENUUYC_CAPTURE`（如 `nvfbc`、`x11,portal`）指定顺序。编码优先用 NVENC（H.264/H.265，4:2:0 与 4:4:4，8 位；AV1 为 4:2:0 8 位，需要带 AV1 编码单元的显卡，尚未在这类显卡上实测），不可用时回退 Rust H.264 软件编码（没有 HDR 采集，因此没有 10 位）；桌面声音参照 Sunshine 经 PulseAudio 客户端接口录制所选播放设备的监听源（PulseAudio 与 PipeWire 均可）；该设备静音或音量为 0 时可能录不到声音；键鼠经 XTest 注入（按物理键位映射，文字输入不依赖键盘布局，移动端触摸按单指指针模拟），因此键鼠目前只在 Xorg 会话可用；支持物理多屏与通过 RandR 切换分辨率，退出时恢复。暂无 AMD/Intel 硬件编码、10 位与 HDR、KMS 采集、虚拟屏、超级屏、无显示器兜底屏、按显示器 DPI、远端麦克风与调整默认音频设备（OpenUUYC Audio 虚拟声卡是 Windows 驱动；Linux 上开启时会提示暂不支持），也没有后台服务，因此无法在登录界面或锁屏时被控。
- **访问通知**：「系统通知」走桌面的 freedesktop 通知服务（GNOME、KDE 等），允许、拒绝、查看按钮直接回到正在运行的程序；「程序浮窗」在 X11 下放在主显示器右下角，Wayland 下由合成器决定位置。
- **设备资料**：上报本机真实硬件，取自主机名、`/etc/os-release` 与内核版本、`/proc`、DMI 主板信息、PCI 显卡（按 `pci.ids` 命名）和默认路由所在网卡。SMBIOS 系统 UUID 只有 root 能读，因此系统标识由 `/etc/machine-id` 派生，重装系统后视为新设备。壁纸从 GNOME、Cinnamon、MATE 的 GSettings 或 KDE Plasma 配置读取，其他桌面会在诊断页提示暂不支持。
- **安装与托盘**：没有“安装服务”，直接运行构建出的程序。关闭窗口隐藏到托盘（StatusNotifierItem；GNOME 需 AppIndicator 扩展，Ubuntu 默认启用），桌面没有托盘时关闭即退出。
- **未接入**：批注与白板（主控发送和被控接收；被控收到批注请求会拒绝）、插件节点图、多显示器独立窗口、HDR、全局快捷键（快捷键仅在播放窗口获得焦点时生效），以及诊断页中的解码检查。
- **凭据**：登录态保存在系统密钥环（Secret Service），需要运行 gnome-keyring、KWallet 等服务；没有明文回退。

Wayland 下窗口的拖动与缩放由合成器接管，因此不支持窗口吸附等 Windows 专有行为。

## 构建

### Windows

需要 Rust stable（MSVC）、Visual Studio C++ 构建工具、Windows SDK、CMake 和 NASM，命令行工具需加入 PATH。H.264 软件编解码和 AV1 软件解码使用项目 Rust 核心及 SIMD 汇编。

```powershell
git clone https://github.com/djkcyl/openuuyc.git
cd openuuyc
cargo dist
```

产物为 `target/dist/` 下的单文件 EXE。仅构建原生 EXE 可用 `cargo dist --native`；命令行参数见 `--help`。

源码已包含签名驱动包和公钥证书，构建主程序不需要签名私钥。修改驱动及发布前验证见 [驱动构建说明](drivers/README.md)。

### Linux

需要 Rust stable（edition 2024）与 C/C++ 工具链。Ubuntu 22.04 及以上：

```bash
sudo apt install build-essential cmake clang nasm pkg-config libva-dev \
    libasound2-dev libdbus-1-dev libxkbcommon-dev libxkbcommon-x11-dev \
    libwayland-dev libx11-dev libxcb1-dev libxrandr-dev libxi-dev libxcursor-dev \
    libgl1-mesa-dev libvulkan-dev libudev-dev libssl-dev fonts-noto-cjk fuse3
git clone https://github.com/djkcyl/openuuyc.git
cd openuuyc
cargo build --release
./target/release/OpenUUYC gui
```

`cargo dist` 只用于 Windows 打包，Linux 直接用 `cargo build`。

打包为 deb（GitHub Actions 的 “Linux deb” 工作流在 Ubuntu 22.04 上构建同样的包，产物见运行页面的 Artifacts，打 `v*` 标签时附到 Release）：

```bash
cargo install cargo-deb
cargo build --release --bin OpenUUYC
cargo deb --no-build
sudo apt install ./target/debian/openuuyc_*.deb
```

安装后从应用菜单启动，或运行 `OpenUUYC`。

## 命令行更新

从新下载的程序包运行 `OpenUUYC.exe update --silent`，静默更新已有安装。会中断当前连接，完成后不自动打开控制中心或重启 Windows；非管理员运行时仍需确认系统 UAC。

无人值守脚本使用 `update --silent --no-elevate`，由安装所属用户的管理员终端执行；权限不足返回 740，不弹 UAC。退出码：0 成功、3010 需重启、170 已有安装操作、1605 未安装、2404 驱动占用，其他失败为 1。不要同时保留其他更新窗口。

## 反馈与许可

问题和建议请提交到 [Issues](https://github.com/djkcyl/openuuyc/issues)。报告问题时附上双方版本、操作步骤、大致发生时间及导出的诊断包，分享前检查是否含私人信息。

OpenUUYC 非网易官方项目。源码公开，但项目整体未采用开源许可证，使用与分发条件见 [LICENSE](LICENSE)。第三方及派生代码保留各自的许可，详见 [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES)。
