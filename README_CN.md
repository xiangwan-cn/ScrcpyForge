# ScrcpyForge

简体中文 | [English](README.md)

ScrcpyForge 是基于 ADB、scrcpy 4.0、FFmpeg 和内嵌 Lua 5.4 构建的跨平台
Android 自动化运行时。无界面后端统一持有设备及媒体/控制连接，桌面端、命令行、
浏览器界面和其他客户端均通过同一套有版本的本地 REST/WebSocket API 工作。

## 功能

- 发现 USB 和无线 ADB 设备，包括配对码无线配对和通过 mDNS 连接已配对设备。
- 使用 H.264、H.265 或 AV1 建立 scrcpy 视频会话，并始终提供最新画面。
- 设备连接后自动启动会话，新会话默认使用五秒一图预览。
- 支持桌面端、模板获取浏览器页面、命令行和纯 API 使用方式。
- 为每台设备独立运行 Lua 自动化，提供原生模板匹配和输入控制。
- 脚本性能档位与预览性能档位相互独立。
- 支持实时 JPEG WebSocket 预览、每五秒一帧或关闭预览。
- 运行资源路径可移植，不包含构建设备或特定机型配置。

## 环境要求

- Rust stable 工具链和 C/C++ 编译器。
- `adb` 位于 `PATH` 中（也可设置 `SCRCPYFORGE_ADB`）。
- `ffmpeg-next` 所需的 FFmpeg 开发库和运行库。
- 原生视觉桥接所需的 OpenCV 头文件和库。
- 已开启 USB 调试或无线调试的 Android 设备。

不同操作系统和发行版的 FFmpeg/OpenCV 软件包名称不同。仓库不提交 scrcpy
server；启动会话前需下载并校验固定版本的官方 v4.0 文件。

## 快速开始

```sh
git clone https://github.com/xiangwan-cn/ScrcpyForge.git
cd ScrcpyForge
./tools/fetch-server.sh
cargo build --release --workspace
./target/release/forge-daemon
```

在另一个终端中运行：

```sh
cargo run -p forge-cli -- scan
cargo run -p forge-cli -- devices
cargo run -p forge-desktop
```

后端默认监听 `0.0.0.0:27180`，内置模板获取页面可从同一局域网的其他设备访问。
在浏览器打开 `http://<后端设备局域网IP>:27180/`；也可以通过
`SCRCPYFORGE_ADDR` 限制监听地址。内置浏览器页面只保留模板获取功能，其他设备和脚本
控制请使用桌面端、命令行或 API。

## 典型流程

1. 启动后端；后端会自动扫描已连接设备。
2. 如设备尚未配对，在桌面端选择“无线配对”，或通过 API 输入 Android 显示的六位
   配对码。
3. 后端会为每个已连接设备自动启动 scrcpy 会话。
4. 预览默认每五秒一帧；只有需要时再选择其他预览模式和性能档位。
5. 运行命名 Lua 脚本，或通过 API 直接提交 Lua 源码。
6. 通过 `/api/v1/events` 接收脚本日志和生命周期事件。

如需重试或覆盖采集参数，仍可显式调用启动会话端点：

```sh
curl -X POST http://127.0.0.1:27180/api/v1/sessions/DEVICE_SERIAL/start \
  -H 'content-type: application/json' \
  -d '{"codec":"h264","max_size":1280,"bit_rate":8000000,"max_fps":60}'
```

全部端点及请求格式见 [API.md](API.md)。Lua 自动化说明见
[LUA_API.md](LUA_API.md) 和 `scripts/example_all_api/script.lua`。

## 组件

- `forge-core`：ADB 发现、会话生命周期、解码、视觉、控制及 Lua 运行时。
- `forge-daemon`：REST/WebSocket 后端及外部集成边界。
- `forge-cli`：可移植的命令行 API 客户端。
- `forge-desktop`：独立桌面客户端。

后端是 ADB 和 scrcpy 连接的唯一持有者。客户端可以重启或同时运行，而不会终止
正在工作的设备会话。

## 配置

| 环境变量 | 默认值 | 用途 |
| --- | --- | --- |
| `SCRCPYFORGE_ADDR` | `0.0.0.0:27180` | 后端监听地址，命令行也使用该地址。 |
| `SCRCPYFORGE_API` | `http://127.0.0.1:27180/api/v1` | 桌面启动辅助脚本使用的 API 地址。 |
| `SCRCPYFORGE_ADB` | `adb` | ADB 可执行文件路径或命令名。 |
| `SCRCPYFORGE_SERVER_JAR` | 自动发现 | scrcpy server v4.0 文件。 |
| `SCRCPYFORGE_DATA_DIR` | 平台用户数据目录 | 可变运行数据根目录。 |
| `SCRCPYFORGE_SCRIPTS_DIR` | `<数据目录>/scripts` | 命名 Lua 脚本包目录。 |
| `SCRCPYFORGE_TEMPLATES_DIR` | `<数据目录>/templates` | 保存的截图区域/模板目录。 |
| `SCRCPYFORGE_ADB_TIMEOUT_MS` | `15000` | 单次 ADB 子进程命令超时（毫秒）。 |
| `SCRCPYFORGE_MDNS_TIMEOUT_MS` | `2000` | 单次 mDNS 发现等待时间，限制在 500–4000 毫秒。 |
| `SCRCPYFORGE_CV_THREADS` | 依主机而定，默认最多 `4` | OpenCV 模板匹配线程预算。 |
| `SCRCPYFORGE_DECODE_THREADS` | 主机并行度，最多 `2` | 每个会话的 FFmpeg 解码线程数。 |
| `SCRCPYFORGE_DECODE_THREAD_TYPE` | `slice` | FFmpeg 线程模式：`slice`、`frame` 或 `none`。 |
| `SCRCPYFORGE_AUTH_TOKEN` | 未设置 | 可选；设置后所有非公开 API 需要 Bearer token（至少 16 字节）。 |
| `SCRCPYFORGE_FONT` | 自动查找平台中文字体 | 可选的界面字体文件。 |
| `RUST_LOG` | 后端 info 日志 | 标准 tracing 日志过滤器。 |

便携包可在可执行文件旁放置 `scripts/`、`templates/` 和
`resources/scrcpy-server-v4.0.jar`。除可移植 API 示例外，仓库默认忽略用户脚本
及其图片资源。

## 自动化与性能

每台设备拥有独立 Lua 状态。画面通过可替换的“最新帧槽”传递：回调速度低于视频
帧率时，旧的待处理帧会被丢弃，不会堆积延迟。画面解码后保持 I420，仅在视觉识别
或预览需要时延迟生成 RGB/JPEG。静止画面默认不会唤醒帧回调；脚本显式设置
`rescan_interval_ms` 后才会按配置重检。

声明式视觉脚本默认每帧全屏匹配；目标在相近位置连续命中三次后，记录模板宽度 2 倍、
高度 3 倍的 ROI，并按设备写入脚本目录下的专属状态文件。之后只在该 ROI 匹配。
不同设备不会共享位置记忆，移动脚本目录时位置记忆也会随之移动；不会因 miss 自动扩大
或回退全屏，删除对应设备的状态文件即可重新学习位置。冷却默认关闭，只有显式配置
正数且动作成功才进入冷却等待。脚本可选择用最新帧低频重检静止画面，不建立历史帧队列。

脚本档位（`auto`、`eco`、`balanced`、`realtime`）和预览档位分别配置。性能取决于
主机、设备编码器、传输方式、画面尺寸、模板和场景；编译或运行时均不会选择针对
某个机型的优化。

## 文档

- [本地 API v1](API.md)
- [Lua API](LUA_API.md)
- [架构](ARCHITECTURE.md)
- [性能验证](PERFORMANCE.md)

## 安全说明

默认 API 监听所有网卡，用于局域网模板获取。未设置 token 时，带有不匹配
`Origin` 的浏览器请求会被拒绝，但局域网内的直接客户端仍可访问 API。设置至少
16 字节的随机 `SCRCPYFORGE_AUTH_TOKEN` 后，所有非公开路由都必须通过
`Authorization: Bearer <token>` 认证。Lua 使用受限标准库，不能访问 `io`、`os`、
`package` 或动态加载模块；脚本仍可控制已连接设备，应按高权限自动化代码管理。

## 许可证

Rust crates 使用 MIT 许可证。下载的 scrcpy server 仍遵循 scrcpy 上游许可证。
