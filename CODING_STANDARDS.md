# 项目编码规范

## 一、文件组织

### 1.1 目录结构
- 每个模块一个目录，目录名 = 模块名，小写下划线
- 模块内 `__init__.py` 只做导出，不放逻辑
- 配置文件放根目录，模板/资源放 `assets/` 或 `templates/`
- 测试文件放 `tests/`，与被测模块同名 + `_test` 后缀

### 1.2 文件大小
- 单文件不超过 300 行
- 超过时拆模块，不要犹豫

### 1.3 导入顺序
```python
# 1. 标准库
import os
from pathlib import Path

# 2. 第三方库
import numpy as np
import cv2

# 3. 项目内模块
from device.session import DeviceSession
from api import ScriptAPI
```
每组之间空一行。不用 `import *`。

---

## 二、命名规范

### 2.1 类名：大驼峰
```python
class DeviceManager:
class ScriptAPI:
```

### 2.2 函数/方法：小写下划线
```python
def connect_device(serial: str):
def get_latest_frame():
```

### 2.3 变量：小写下划线
```python
device_count = 0
latest_frame = None
```

### 2.4 常量：全大写下划线
```python
MAX_DEVICES = 8
DEFAULT_PORT = 27183
KEY_BACK = 4
```

### 2.5 私有成员：单下划线前缀
```python
self._running = False
self._frame_queue = queue.Queue(maxsize=1)

def _parse_header(self, data: bytes):
```

### 2.6 布尔变量：`is_` / `has_` / `should_` 前缀
```python
is_connected = True
has_frame = False
should_retry = True
```

---

## 三、函数设计

### 3.1 单一职责
一个函数只做一件事。能用一个动词描述。

```python
# ✅ 好
def deploy_jar(serial: str, jar_path: str):
    ...

def forward_port(serial: str, port: int, scid: str):
    ...

# ❌ 差
def setup_device(serial, jar, port, scid, max_size, codec):  # 做了 6 件事
    ...
```

### 3.2 参数不超过 4 个
超过时用配置对象或 dataclass：

```python
@dataclass
class ScrcpyConfig:
    max_size: int = 1280
    video_codec: str = "h264"
    bit_rate: int = 8_000_000

def launch_server(config: ScrcpyConfig):
    ...
```

### 3.3 返回值明确
始终返回同一种类型。不用 `None` / `int` / `dict` 混着返。

```python
# ✅ 好
def find_template(...) -> dict | None:  # 找到返回 dict，找不到 None

# ❌ 差
def find_template(...):  # 有时返回 dict，有时返回 False，有时返回 -1
```

### 3.4 无副作用标注
纯函数（不修改参数、不读写全局状态）强调出来：

```python
def scale_coordinates(x: float, y: float, scale_x: float, scale_y: float) -> tuple[float, float]:
    """纯函数：视频坐标 → 设备坐标"""
    return (x * scale_x, y * scale_y)
```

---

## 四、注释规范

### 4.1 只写"为什么"，不写"做什么"
代码本身说明"做什么"，注释解释"为什么这么做"。

```python
# ✅ 好
# 必须在 control socket 连接后才读 device info，
# 否则 server 阻塞在 accept() 上，永远不发数据
device_name = recv_exact(video_sock, 64)

# ❌ 差
# 读取 64 字节的设备名
device_name = recv_exact(video_sock, 64)
```

### 4.2 模块级 docstring：三行以内
```python
"""
scrcpy v4.0 wire protocol 实现。
负责 ADB 部署、socket 握手、H.264 帧接收和控制消息发送。
线程安全：单线程使用，调用方加锁。
"""
```

### 4.3 函数 docstring：写清参数和异常
```python
def inject_touch(action: int, x: int, y: int, screen_w: int, screen_h: int):
    """
    发送触控事件到设备。

    Args:
        action: 0=DOWN, 1=UP, 2=MOVE
        x, y: 设备屏幕坐标
        screen_w, screen_h: 设备物理分辨率

    Raises:
        ConnectionError: control socket 已断开
    """
```

### 4.4 复杂逻辑：写一行注释
```python
# PTS 是 61 位 big-endian，Java ByteBuffer.putLong 写入的格式
pts = struct.unpack('>Q', h[:8])[0] & 0x1FFFFFFFFFFFFFFF
```

### 4.5 不写的注释
- 不给显而易见的东西写注释
- 不写"修改者/日期"——让 git 记
- 不写被注释掉的旧代码——直接删

---

## 五、错误处理

### 5.1 明确异常类型
```python
# ✅ 好
class ScrcpyError(Exception): pass
class ConnectionTimeout(ScrcpyError): pass
class ProtocolError(ScrcpyError): pass

raise ConnectionTimeout(f"video socket connect timeout after {deadline}s")
```

### 5.2 不吞异常
```python
# ✅ 好
try:
    frame = decoder.decode(nal)
except av.InvalidDataError:
    logger.warning("decode failed, skipping frame")
    return None

# ❌ 差
try:
    frame = decoder.decode(nal)
except:
    pass  # 出错了也假装没事
```

### 5.3 资源用上下文管理器
```python
# ✅ 好
with socket.socket() as sock:
    sock.connect(addr)
    ...

# ❌ 差
sock = socket.socket()
sock.connect(addr)
...
sock.close()  # 异常时不执行
```

---

## 六、依赖管理

### 6.1 最小依赖
能用标准库就不装第三方包。标准库能做的不额外引入。

```python
# ✅ 标准库就够
import json
import struct
import subprocess

# ❌ 为了一个函数装整个库
from some_lib import to_json  # 别
```

### 6.2 requirements.txt 锁定主版本
```
av>=12.0,<13.0
numpy>=1.24,<2.0
opencv-python-headless>=4.8,<5.0
dearpygui>=1.10,<2.0
watchdog>=4.0,<5.0
```

---

## 七、线程安全

### 7.1 线程边界明确
每行代码一眼看出在哪个线程：

```python
# ── 在 scrcpy 回调线程 ──
def _on_frame(self, frame: np.ndarray):
    self._cached_frame = frame       # 单写
    self._frame_queue.put(frame)     # 线程安全

# ── 在脚本线程 ──
def find(self, tpl_path: str):
    frame = self._cached_frame       # 多读（原子引用赋值）

# ── 在 UI 主线程 ──
def _render(self):
    try:
        frame = self._frame_queue.get_nowait()  # 不阻塞
    except queue.Empty:
        pass
```

### 7.2 锁的规则
- 能不用就不用——用 `queue.Queue`、`threading.Event`、`atomic` 赋值替代
- 用就写清楚保护什么
- 持锁时不调外部代码（防止死锁）

```python
# ✅ 好
with self._lock:
    self._sessions[serial] = session  # 只保护这一行

# ❌ 差
with self._lock:
    self._sessions[serial] = session
    session.connect(...)  # 持锁调外部的阻塞操作 = 死锁温床
```

---

## 八、可测试性

### 8.1 依赖注入
```python
# ✅ 好：可 mock
class ScrcpyProtocol:
    def __init__(self, adb_path="adb"):
        self._adb = adb_path

# ❌ 差：硬编码
class ScrcpyProtocol:
    def _adb(self, *args):
        subprocess.run(["adb", *args])  # 测试时改不了路径
```

### 8.2 IO 和逻辑分离
```python
# ✅ 纯逻辑，无 socket/文件读写
def parse_packet_header(data: bytes) -> PacketHeader:
    ...

# ✅ 纯 IO，无逻辑
def read_packet(sock: socket.socket) -> bytes:
    ...
```

---

## 九、代码风格

### 9.1 类型注解
所有公开函数都写：

```python
def find(self, tpl_path: str, threshold: float = 0.8) -> dict | None:
```

### 9.2 不写 else 如果 if 里已经 return
```python
# ✅
def check(x):
    if x > 0:
        return True
    return False

# ❌
def check(x):
    if x > 0:
        return True
    else:
        return False
```

### 9.3 字典/列表推导超过一行就拆
```python
# ✅ 一行
names = [d.name for d in devices if d.online]

# ✅ 多行拆
devices = [
    d for d in all_devices
    if d.online
    and d.serial not in failed
]
```

### 9.4 魔法数字命名
```python
# ❌
time.sleep(0.1)  # 0.1 是什么意思？

# ✅
RECONNECT_INTERVAL = 0.1  # 100ms 重连间隔
time.sleep(RECONNECT_INTERVAL)
```

---

## 十、Git 规范

### 10.1 Commit message
```
<type>: <简短描述>

type: feat / fix / refactor / docs / chore
```

```
feat: 添加 ScriptAPI.find_all() 多目标匹配
fix: 修复 control socket 双连接导致触控失效
refactor: 提取 protocol.py 独立模块
docs: 补充线程安全模型注释
chore: 升级 av 到 12.0
```

### 10.2 一个 commit 做一件事
不把"修 bug + 重构 + 加功能"混一个 commit。

### 10.3 不提交
- `__pycache__/`
- `.pyc`
- 虚拟环境
- IDE 配置（除非团队统一用同一个）

`.gitignore`:
```
__pycache__/
*.pyc
venv/
.venv/
.env
*.egg-info/
dist/
build/
.DS_Store
```
