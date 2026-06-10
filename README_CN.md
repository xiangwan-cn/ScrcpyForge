# ScrcpyForge

多设备 Android 自动化 — Python 版。通过 scrcpy v4.0 协议控制一台或多台 Android 设备，使用 OpenCV 模板匹配编写自动化脚本，基于 DearPyGui 桌面界面。


## 快速开始

```bash
# 安装依赖
pip install --break-system-packages -r requirements.txt

# 运行（自动检测所有已连接的 adb 设备）
python scrcpy_script/main.py

# 指定设备连接
python scrcpy_script/main.py --device <序列号>
```

**前置条件：** ADB 在 PATH 中，Android 设备已开启 USB 调试。

## 功能

| 功能 | |
|------|---|
| 自动检测 adb 设备并连接 | 热插拔（2 秒轮询） |
| 实时视频预览 (DearPyGui) | 每设备独立脚本线程 |
| 模板匹配 (OpenCV) | 触摸 / 按键注入 (scrcpy v4.0) |
| 单设备 Run/Stop | Start All / Stop All |
| 脚本热重载 (watchdog) | 截图 → 坐标 + 区域选取 |
| TCP 无线连接 | 原生 scrcpy 窗口 (subprocess) |
| 自适应 UI（宽度按画面比例，高度填充窗口） | 实时 FPS 显示 |

## 项目结构

```
scrcpy_script/
├── main.py                  # 入口，命令行参数，配置
├── api.py                   # ScriptAPI — 用户脚本接口
├── config.py                # key=value 配置文件读取器
├── scrcpy_launcher.py       # 启动原生 scrcpy 窗口
├── device/
│   ├── manager.py           # adb 设备发现、热插拔、自动连接
│   └── session.py           # 每设备：协议 + PyAV 解码 + 帧队列
├── protocol/
│   ├── control.py           # scrcpy v4.0 控制消息构建（大端序）
│   └── server.py            # 服务端启动、视频 socket、数据包读取
├── script/
│   └── runner.py            # 脚本线程，threading.Event 停止信号
├── scripts/
│   ├── match_tap/           # 示例：模板匹配 + 点击
│   │   ├── manifest.py      # 注册文件（NAME=显示名称）
│   │   └── script.py        # 实际脚本
│   └── example_all_api/     # 示例：所有 API 方法演示
├── ui/
│   ├── app.py               # DearPyGui 主窗口、工具栏
│   ├── device_card.py       # 设备卡片（预览、控制、日志）
│   ├── log_panel.py         # 可滚动日志（deque，最多 200 条）
│   └── region_picker.py     # 冻结帧 → 坐标选取 → 保存模板
├── scrcpy_config.conf       # 默认配置
└── templates/               # 模板图片 (.png)

tools/
└── build_win.py             # Windows .exe 打包脚本
```

17 个 Python 文件，约 1800 行。

## 架构

```
主线程 (DearPyGui)
  ├── Scan / Start All / Stop All / TCP 工具栏
  └── 设备网格 (横向排列)
       └── 每设备 DeviceCard
            ├── 预览 (raw_texture, 通过 queue.Queue 更新)
            ├── 脚本下拉框 + Run/Stop
            ├── Screenshot → RegionPicker 弹窗
            └── 日志面板

每设备（2 线程）：
  解码线程              脚本线程
  ┌─────────┐          ┌──────────┐
  │ PyAV    │          │ script() │
  │  解码    │──┐       │ api.*()  │
  └─────────┘  │       └──────────┘
       │       │            │
   _cached_frame│      _stop_event
       │       │            │
       └──────[queue.Queue(maxsize=2)]──→ UI 刷新
```

3 线程/设备（解码、脚本、UI 主线程）。热路径在 C 扩展中释放 GIL — 无性能瓶颈。

## 配置

`scrcpy_config.conf` 使用 `key=value` 格式，`#` 注释：

```ini
# 视频
video_codec=h264
max_size=1280
bit_rate=8000000
max_fps=60

# 连接
port_start=27183
port_end=27282
max_devices=10

# 路径
server_jar=scrcpy-server-v4.0.jar
scripts_dir=scrcpy_script/scripts
templates_dir=templates
```

命令行可覆盖配置：`--device`、`--config`、`--jar`。

## 编写脚本

脚本是带 `script(api)` 函数的 Python 模块。采用注册式目录结构：

```
scripts/<脚本名>/
├── manifest.py    # NAME = "显示名称"（可选，不定义则用目录名）
└── script.py      # def script(api): ...
```

只有含 `manifest.py` 的目录才会出现在 UI 下拉框中。

```python
# scripts/auto_login/script.py
def script(api):
    api.log(f"设备: {api.device_serial()}")
    while True:
        btn = api.find("templates/login.png", threshold=0.8)
        if btn:
            api.tap(btn["x"], btn["y"])
        api.wait(1000)
```

要点：
- `api.wait(ms)` 每 100ms 检查停止事件 — 点 Stop 立即中断循环。
- `api.find()` 内部使用 `@lru_cache` 缓存模板 — 首次加载后不读磁盘。
- 异常由 ScriptRunner 捕获并显示在日志面板。
- 脚本在守护线程中运行 — UI 保持响应。

完整 API 参考：[scrcpy_script/API.md](scrcpy_script/API.md)

## 协议

直接实现 scrcpy v4.0 有线协议：

1. `pkill` 清理旧服务 → 推送 `scrcpy-server-v4.0.jar` → `adb forward` 两个端口
2. 启动 `app_process`，参数 `tunnel_forward=true send_frame_meta=true send_dummy_byte=true`
3. 连接视频 socket → 读取 dummy byte → 连接控制 socket
4. 读取设备名称（64 字节）+ 编码 ID（4 字节）
5. PyAV `av.CodecContext("h264")` 解码 H.264 NAL 单元（去除 12 字节 scrcpy 头部，添加 Annex B 前缀）
6. 控制 socket 发送 14/32 字节大端序消息，用于触摸/按键/文本注入

所有整数大端序，符合 scrcpy v4.0 规范。

## 打包 Windows 版

打包独立 `.exe`，包含全部依赖：

```bash
# 在 Windows 上（或用 CI 交叉编译）：
python tools/build_win.py
```

输出目录 `win/scrcpy_script/`：

```
win/scrcpy_script/
├── scrcpy_script.exe         # 独立可执行文件
├── scrcpy_config.conf        # 默认配置（可编辑）
├── scrcpy-server-v4.0.jar   # 运行时需要，放这里
├── scripts/                  # 自动化脚本
│   └── match_tap/
│       ├── manifest.py
│       └── script.py
└── templates/                # 模板图片
```

分发整个 `win/scrcpy_script/` 文件夹。用户需要在 PATH 中配置 ADB 和 scrcpy。

CI 自动化构建参考 `tools/build_win.py`。

## 依赖

```
av>=12.0                    # PyAV — FFmpeg H.264 解码
opencv-python-headless>=4.8 # 模板匹配（headless 版无 GUI 后端，省 ~50MB）
numpy>=1.24                 # 帧数据数组
dearpygui>=1.10             # 即时模式 GPU 界面
watchdog>=4.0               # 热重载文件监听（可选）
```

## 许可证

MIT

## 相关项目

- [scrcpy](https://github.com/Genymobile/scrcpy) — 协议参考
- [py-scrcpy-client](https://github.com/leng-yue/py-scrcpy-client) — 另一个 Python scrcpy 客户端（v1.24 协议）
