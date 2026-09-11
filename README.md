# OpenUUYC

OpenUUYC 是基于对 UU 远程客户端及通信行为的逆向分析，使用 Rust 实现协议兼容和多平台适配的独立第三方客户端，提供设备管理、远程画面观看和串流设置。当前提供 Windows 客户端，macOS 和 Linux 的原生界面尚未实现。

UU 远程是兼容与研究对象，其协议不由本项目原创。项目不代表网易，也不表示获得官方授权、认证或背书。

## 功能

- 图形设备中心：扫码或短信登录、设备列表与详情、设备别名和账号设备管理。
- 远程协助：通过设备 ID 和验证码或对端确认连接，管理最近连接与收藏。
- 多显示器观看：屏幕标签切换、拖出独立窗口、同一会话多窗口播放和共享音频。
- 键鼠控制：Windows 被控端支持键盘按下/松开、长按、组合键与锁定键状态同步；鼠标支持移动、五键、拖拽、滚轮及智能、被控端和主控端模式。Ctrl+Shift+Alt+Z退出控制，Ctrl+Shift+Alt+F切换全屏，Ctrl+Shift+Alt+Q关闭当前串流窗口。
- 串流设置：自动、原画、高清、清晰、自定义码率与帧率；保存设备偏好，支持手动中转和可选自适应码率。
- 视频与音频：Windows D3D11/DXVA11 硬解 H.264/HEVC，Rust H.264 软件解码，以及 Opus 音频播放、音量和静音。

Windows 视频解码已使用 Rust 实现，通过系统 API 调用显卡驱动；音频等组件仍包含 C/C++ 依赖。软解时整个客户端最多播放一个窗口，硬解支持多窗口。

当前不提供剪贴板同步、文件传输、本机被控、云设备或网络代理功能。AV1、NVDEC 和 HEVC 软件解码暂不支持。macOS/Linux 的原生 GUI、呈现和键盘适配尚待分别实现。

## 构建

Windows x64 需要 Rust stable（MSVC 工具链）、Visual Studio 2022 C++ 构建工具、Windows SDK 和 CMake。C/C++ 工具链用于编译音频等依赖。

```powershell
git clone https://github.com/djkcyl/openuuyc.git
cd openuuyc
cargo build --release --locked --bin OpenUUYC
```

生成程序为 `target/release/OpenUUYC.exe`。应用不依赖官方 UU DLL，也不需要 FFmpeg 或 Opus 的独立 DLL。

## 使用

双击 `OpenUUYC.exe` 打开图形设备中心。GUI 和命令行使用同一个程序：

```powershell
# 显示命令帮助
./target/release/OpenUUYC.exe --help | Out-Host

# 登录并查看设备
./target/release/OpenUUYC.exe login | Out-Host
./target/release/OpenUUYC.exe devices | Out-Host

# 打开指定设备的观看窗口
./target/release/OpenUUYC.exe connect "设备名称" --fps 60 --codec auto | Out-Host
```

观看窗口内可切换画质、帧率、音量和屏幕。设置在点击应用并获远端确认后保存；画面尺寸由实际码流决定。

## 源码与检查

`src/` 包含客户端和视频解码实现，`vendor/` 包含经过适配的依赖源码及其原有测试。自建持久测试仅保留平台、原生后端和 CPU 指令集差异相关项目。

```powershell
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

仓库不包含本地分析记录、登录资料、运行日志、真实抓流或编译产物。

## 著作权、许可与服务边界

本项目仅对贡献者依法享有权利的原创内容保留未授予的权利，不主张拥有 UU 远程的软件、协议、商标、文档、素材或服务权益。源码公开、Rust 重写和平台适配均不意味着所有内容的权利归本项目所有，也不当然产生第三方授权。

原创内容未采用开放源码许可证。其使用、修改和分发条件，以及法定权利、既有许可和 LGPL 所需的例外，见 [著作权与许可声明](LICENSE)。第三方及派生代码继续适用原许可；尤其 FFmpeg 派生部分仍为 LGPL-2.1-or-later，不能被项目的限制覆盖。详见 [第三方来源与许可](THIRD_PARTY_NOTICES)、[LGPL 原文](src/decoder/COPYING.FFmpeg)及各组件保留的声明。

**源码许可不等于官方服务许可。** 当前可访问的 [UU 官方协议](https://adl.netease.com/d/g/uuremote/c/licenseandservice?type=pc&direct=1) 第 3.2.1、3.2.3 条分别限制未经认可的第三方兼容软件和逆向工程；条款是否适用及其效力须结合具体情况判断。协议兼容、免费或学习研究的表述不能代替所需授权，也不能保证避免合同、知识产权或其他争议。实际连接还须取得设备控制者的有效授权。

本程序包含静态链接的 LGPL 组件。分发时须实际提供相应许可文本及满足适用条款的对应源码、修改与可重新链接材料；公开仓库或声明本身不能替代这些义务。对外发布前应核查代码来源、实际发行材料和官方服务条款，必要时取得权利人授权及专业法律意见。

如有具体权利或署名问题，请通过 [Issues](https://github.com/djkcyl/openuuyc/issues) 提供文件、来源和权利依据；请勿提交账号凭据或其他不宜公开的资料。
