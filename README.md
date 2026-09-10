# OpenUUYC

OpenUUYC 是使用 Rust 开发的 UU 远程第三方客户端，提供设备管理、远程画面观看和串流设置。当前提供 Windows 客户端，macOS 和 Linux 的原生界面尚未实现。

## 功能

- 图形设备中心：扫码或短信登录、设备列表与详情、设备别名和账号设备管理。
- 远程协助：通过设备 ID 和验证码或对端确认连接，管理最近连接与收藏。
- 多显示器观看：屏幕标签切换、拖出独立窗口、同一会话多窗口播放和共享音频。
- 串流设置：自动、原画、高清、清晰、自定义码率与帧率；保存设备偏好，支持手动中转和可选自适应码率。
- 视频与音频：Windows D3D11/DXVA11 硬解 H.264/HEVC，Rust H.264 软件解码，以及 Opus 音频播放、音量和静音。

Windows 视频解码已使用 Rust 实现，通过系统 API 调用显卡驱动；音频等组件仍包含 C/C++ 依赖。软解时整个客户端最多播放一个窗口，硬解支持多窗口。

当前不提供键鼠控制、剪贴板、文件传输、本机被控、云设备或网络代理功能。AV1、NVDEC 和 HEVC 软件解码暂不支持。macOS/Linux 的原生 GUI 和呈现尚待分别实现。

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

`src/` 包含客户端和视频解码实现，`vendor/` 包含随项目构建的依赖源码，`tests/` 与 `examples/` 包含回归测试和开发入口。

```powershell
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

仓库不包含本地分析记录、登录资料、运行日志、真实抓流或编译产物。

## 许可证

原创部分保留所有权利：未经权利人书面授权，不授予使用、修改、分发、出售或其他商用许可，具体范围及例外见 [LICENSE](LICENSE)。源码可见不等于获得上述授权，本项目不以开放源码许可发布。上面的构建与使用说明供权利人及获授权人员使用。

第三方代码和已有独立许可的部分不受上述声明重新许可。FFmpeg 衍生部分继续适用 LGPL-2.1-or-later，其余组件保留各自许可；这些既有权利不会被本项目的限制撤销。详见 [第三方声明](THIRD_PARTY_NOTICES)、[视频许可证](src/decoder/COPYING.FFmpeg)和 [H.264 核心许可证](src/decoder/h264-core/COPYING.LGPLv2.1)。

OpenUUYC 是独立第三方项目，与官方 UU 远程无隶属关系。
