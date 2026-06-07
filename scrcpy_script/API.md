# ScriptAPI Reference

`ScriptAPI` 对象通过 `script(api)` 传入。所有方法同步、线程安全，在脚本独立线程中执行。

## 常量

| 常量 | 值 | 说明 |
|------|-----|------|
| `api.KEY_BACK` | 4 | 返回键 |
| `api.KEY_HOME` | 3 | Home 键 |
| `api.KEY_ENTER` | 66 | 回车键 |
| `api.KEY_POWER` | 26 | 电源键 |
| `api.KEY_VOLUME_UP` | 24 | 音量+ |
| `api.KEY_VOLUME_DOWN` | 25 | 音量- |
| `api.KEY_MENU` | 82 | 菜单键 |

---

## 帧操作

### `capture() -> np.ndarray | None`

获取当前缓存帧，BGR 格式 numpy 数组 (H×W×3)。无帧时返回 `None`。

拿到帧后可在脚本里用 `cv2` / `numpy` 做任意自定义图像处理，与 `api.find()` 走同一段 C 扩展代码，速度无差别。

```python
frame = api.capture()
if frame is not None:
    gray = cv2.cvtColor(frame, cv2.COLOR_BGR2GRAY)
    # ... 自定义处理
```

### `screen_size() -> tuple[int, int]`

设备屏幕分辨率 `(宽, 高)`。旋转时自动更新。

### `video_size() -> tuple[int, int]`

视频流分辨率。不同于 device_size 当 `max_size` 缩放生效时。

---

## 模板匹配

模板图片放在 `templates/` 目录。内部用 `@lru_cache(maxsize=32)` 缓存加载——重复调用不读磁盘。

`roi` 参数（可选）：`(x1, y1, x2, y2)` 限定搜索区域。返回坐标自动转全屏。设为 `None` 搜全帧。

### `find(tpl_path: str, threshold: float = 0.8, roi: tuple | None = None) -> dict | None`

查找单个最佳匹配。高于阈值返回匹配字典，否则 `None`。

返回值：`{"x": 中心x, "y": 中心y, "w": 宽, "h": 高, "confidence": 置信度}`

```python
m = api.find("templates/btn.png", threshold=0.75)
m = api.find("templates/icon.png", roi=(100, 200, 500, 600))
if m:
    api.tap(m["x"], m["y"])
```

### `find_all(tpl_path: str, threshold: float = 0.8, roi: tuple | None = None) -> list[dict]`

查找所有匹配。返回列表（同 `find()` 格式）。

```python
for m in api.find_all("templates/icon.png"):
    api.log(f"({m['x']},{m['y']}) conf={m['confidence']:.2f}")
```

### `wait_for(tpl_path: str, timeout_ms: int = 10000, roi: tuple | None = None) -> dict | None`

每 50ms 轮询 `find()`，匹配成功或超时返回。检测 Stop 事件——点停止立即中断。

```python
m = api.wait_for("templates/loading.png", timeout_ms=30000)
if m is None:
    api.warn("超时未找到")
```

---

## 触摸操作

所有坐标为**设备屏幕坐标**。scrcpy 服务端用 `screen_width/height` 自动归一化。

### `tap(x: int, y: int)`

在 `(x, y)` 处点击（按下+抬起）。

### `swipe(x1, y1, x2, y2, duration_ms=300)`

从 `(x1,y1)` 滑到 `(x2,y2)`，间隔 16ms 插值。

### `long_press(x: int, y: int, duration_ms: int)`

长按。

### `multi_tap(points: list[tuple[int, int]])`

多点同时触控（先全部按下，再全部抬起）。

---

## 输入

### `press_key(keycode: int, long_press: bool = False)`

注入按键码。用 `api.KEY_*` 常量。

```python
api.press_key(api.KEY_HOME)
api.press_key(api.KEY_POWER, long_press=True)
```

### `press_back()`

按返回键。等价于 `api.press_key(api.KEY_BACK)`。

### `input_text(text: str)`

注入 UTF-8 文本。协议限制单次 ~300 字节。

---

## 流程控制

### `wait(ms: int)`

休眠 `ms` 毫秒。每 100ms 检查停止事件——点 Stop 抛出 `StopIteration`，脚本干净退出。不会残留多余操作。

### `repeat_until(fn: Callable[[], bool], timeout_ms: int) -> bool`

每 50ms 调用 `fn()`，直到返回 `True` 或超时。返回布尔值。

```python
def ready():
    return api.find("templates/ready.png") is not None

if api.repeat_until(ready, 10000):
    api.log("画面就绪")
```

---

## 输出

### `log(msg: str)`

写入信息日志到设备面板。

### `warn(msg: str)`

写入警告日志。

### `screenshot(path: str)`

保存当前帧为 PNG。

```python
api.screenshot("debug/before_tap.png")
```

### `device_serial() -> str`

设备 adb 序列号。

### `device_name() -> str`

设备名称（scrcpy 握手获取）。

### `is_rotated() -> bool`

屏幕是否旋转过。

---

## 脚本生命周期

```
ScriptRunner.run()
  log("Script started")
  try:
    script(api)    ← 你的代码
  except StopIteration:     # 正常停止（点了 Stop）
    pass
  except Exception as e:
    warn(f"错误: {e}")      # 脚本异常
  finally:
    log("Script stopped")
```

- 点 **Run**：创建 ScriptAPI + ScriptRunner，启动独立守护线程
- 点 **Stop**：设置 `threading.Event` → `api.wait()` 抛出 `StopIteration` → 线程退出
- 脚本崩溃不影响其他设备
- 热重载：编辑 `.py` 文件后手动 Stop/Run

## 线程模型

```
解码线程 → av.CodecContext → frame → queue.Queue(maxsize=2) → UI 线程渲染
                                   ↘ _cached_frame (原子赋值)
脚本线程 → api.capture() / api.find() / cv2.任意() / api.tap()
```

3 线程/设备。热路径（PyAV、OpenCV）释放 GIL，互不阻塞。

---

## 完整示例

```python
"""演示所有 API 方法的示例脚本。"""
import random


def script(api):
    api.log(f"==== 设备: {api.device_serial()} ({api.device_name()}) ====")

    # ── 帧信息 ──
    sw, sh = api.screen_size()
    vw, vh = api.video_size()
    api.log(f"屏幕分辨率: {sw}x{sh}, 视频分辨率: {vw}x{vh}")
    api.log(f"旋转: {api.is_rotated()}")

    # ── 截图 ──
    api.screenshot("example_start.png")
    api.log("已保存初始截图")

    # ── 帧操作 ──
    frame = api.capture()
    if frame is not None:
        api.log(f"帧尺寸: {frame.shape}, dtype={frame.dtype}")

    # ── 全帧模板匹配 ──
    m = api.find("templates/template.png", threshold=0.8)
    if m:
        api.log(f"find 结果: ({m['x']},{m['y']}) conf={m['confidence']:.2f}")

    # ── ROI 限定区域匹配 ──
    m = api.find("templates/template.png", threshold=0.7, roi=(100, 200, 600, 800))
    if m:
        api.log(f"ROI匹配: ({m['x']},{m['y']})")

    # ── 查找全部 ──
    matches = api.find_all("templates/template.png", threshold=0.7)
    api.log(f"找到 {len(matches)} 个匹配")

    # ── 等待出现 ──
    api.log("等待目标出现 (5s超时)...")
    m = api.wait_for("templates/template.png", timeout_ms=5000)
    if m:
        api.log(f"wait_for 成功: ({m['x']},{m['y']})")
    else:
        api.warn("wait_for 超时")

    # ── repeat_until ──
    def has_match():
        return api.find("templates/template.png", threshold=0.3) is not None
    ok = api.repeat_until(has_match, timeout_ms=3000)
    api.log(f"repeat_until: {ok}")

    # ── 触摸 ──
    api.tap(sw // 2, sh // 2)
    api.log(f"点按中心 ({sw//2},{sh//2})")

    api.long_press(sw // 2, sh // 2, duration_ms=800)
    api.log("长按中心 800ms")

    api.swipe(sw // 2, sh * 2 // 3, sw // 2, sh // 3, duration_ms=500)
    api.log("向上滑动")

    api.multi_tap([(sw // 3, sh // 2), (sw * 2 // 3, sh // 2)])
    api.log("双指点击")

    # ── 按键 ──
    api.press_back()
    api.log("按了返回键")

    api.press_key(api.KEY_HOME)
    api.log("按了 Home 键")

    api.press_key(api.KEY_POWER, long_press=True)
    api.log("长按电源键")

    api.press_key(api.KEY_VOLUME_UP)
    api.log("按了音量+")

    # ── 文本输入 ──
    api.input_text("Hello ScrcpyScript")
    api.log("已输入文本")

    # ── 等待 ──
    api.wait(1000)
    api.log("等了 1 秒")

    # ── 循环点击（点 5 次） ──
    for i in range(5):
        ox = random.randint(-5, 5)
        oy = random.randint(-5, 5)
        api.tap(sw // 2 + ox, sh // 2 + oy)
        api.log(f"第 {i+1} 次点击 ({sw//2+ox},{sh//2+oy})")
        api.wait(500)

    # ── 警告日志 ──
    api.warn("示例脚本执行完毕")

    api.log("==== 所有 API 调用完成 ====")
```

