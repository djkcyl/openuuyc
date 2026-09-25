# OpenUUYC

![OpenUUYC](assets/banner.png)

OpenUUYC 是用 Rust 编写的 UU 远程第三方 Windows 客户端。使用已有的 UU 账号登录，连接和控制远端设备。

## 下载与使用

从 [Releases](https://github.com/djkcyl/openuuyc/releases) 下载 Windows x64 客户端，双击运行，扫码或短信登录后选择设备连接。被控设备需要运行 UU 远程。

## 功能

- **设备管理**：设备列表与详情、修改别名、远程开关机和重启；支持通过设备 ID 连接，以及最近连接和收藏。
- **远程控制**：键鼠操作、多种鼠标模式、自定义快捷键和设备快速切换。
- **麦克风**：将本机麦克风发送到远端 Windows 设备；支持选择输入设备或跟随系统默认，在播放器标题栏开启或关闭。
- **文件传输**：双栏浏览、文件和文件夹双向传输、目录管理、暂停续传及同名文件处理；关闭窗口后可继续传输。
- **剪贴板同步**：文字、图片及文件复制粘贴。
- **端口转发**：TCP 映射、自定义监听地址、连通性探测、网速与流量统计；支持后台运行。
- **多显示器**：切屏、拖出独立窗口、调整分辨率与 DPI，支持虚拟屏和超级屏。
- **画质设置**：画质档位、自定义码率、帧率、YUV 4:4:4 和 HDR；支持硬件解码、音频播放与性能监控。
- **批注**：画笔、形状、擦除、撤销重做，以及激光笔和鼠标指示；笔迹在远端可见。
- **插件**：用节点图组合画面处理效果，提供[滤镜示例与插件 SDK](plugins/README.md)。

目前支持 Windows x64。当前开发版新增本机桌面画面共享，需在本机明确开启；已发布版本的具体功能以所用 Release 说明为准。

## 构建

需要 Rust stable（MSVC）、Visual Studio C++ 构建工具、Windows SDK、CMake 和 UPX，确保 `upx` 在 PATH 中。软件 H.264 编解码使用项目 Rust 核心。

```powershell
git clone https://github.com/djkcyl/openuuyc.git
cd openuuyc
cargo dist
```

程序位于 `target/dist/upx/`，打包时自动进行 UPX 压缩、完整性和启动检查。命令行用法可通过程序的 `--help` 查看。

## 反馈与许可

问题和建议请提交到 [Issues](https://github.com/djkcyl/openuuyc/issues)。报告问题时附上版本、操作步骤和相关日志，注意去掉账号、验证码等私人信息。

OpenUUYC 非网易官方项目。源码公开，但项目整体未采用开源许可证，使用与分发条件见 [LICENSE](LICENSE)。第三方及派生代码保留各自的许可，详见 [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES)。
