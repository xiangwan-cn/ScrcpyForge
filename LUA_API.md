# Lua 实时脚本 API

每台设备拥有独立的 Lua 状态和脚本任务。脚本处理期间到达的新画面会覆盖旧画面，
所以下一次回调始终取得最新帧，不会因识别速度低于视频帧率而不断增加延迟。

脚本有两种等价的写法：

- `forge.vision { ... }`：声明目标和策略，由通用调度层处理优先级、批量匹配、
  节流和动作。适合绝大多数模板识别脚本。
- `on_frame(frame)`：直接使用每帧底层 API，适合状态机、像素算法或其他特殊逻辑。

声明式 API 是 Lua 标准库，不包含应用或游戏的业务硬编码。完整可运行的中文注释
见 `scripts/example_all_api/script.lua`。

## 可移植资源

命名脚本位于 `scripts/<脚本名>/script.lua`，模板建议放在相同目录或其子目录：

```lua
local button = forge.asset("images/button.png")
```

`forge.asset()` 只接受脚本目录内的相对路径，拒绝绝对路径和 `..`，因此脚本包可在
Linux、Windows、macOS 间直接迁移。`forge.script_dir` 提供当前脚本目录。通过
`/api/v1/scripts/run` 直接提交的匿名源码没有可信资源根目录，不能使用该函数。

## 声明式识别

```lua
forge.vision {
    threshold = 0.85,                 -- 默认匹配阈值
    roi = {0, 0, 1280, 720},          -- 可省略，格式为 x1,y1,x2,y2
    interval_ms = 30,                 -- 未命中后的识别间隔
    after_match_ms = 500,             -- 任一目标命中后的全局间隔
    action = {type = "tap", radius = 4},
    targets = {                       -- 顺序就是匹配优先级
        {name = "确认", template = forge.asset("confirm.png")},
        {name = "返回", template = forge.asset("back.png"), threshold = 0.9},
    },
    on_match = function(target, match, frame)
        forge.log(target.name .. " " .. match.confidence)
    end,
}
```

目标可覆盖 `threshold`、`roi`、`multiscale`、`cooldown_ms`、`action`，也可设置
`enabled = bool/function` 和 `on_match(match, frame, target)`。目标 `on_match` 会替代
默认动作；顶层 `on_match(target, match, frame)` 是动作完成后的统一通知。

内置动作支持：

- `{type="tap", radius=4, offset_x=0, offset_y=0}`
- `{type="key", code=forge.KEY_BACK, long_press=false}`
- `{type="swipe", x1=0, y1=0, x2=0, y2=0, duration_ms=300}`
- `"none"` 或 Lua 函数

当所有可用目标使用相同阈值、ROI 和单尺度模式时，调度器自动调用一次
`frame:find_first()`，共享帧颜色转换并按目标顺序返回首个命中。目标覆盖了不同参数、
处于独立冷却或动态禁用时会回退为按优先级逐项匹配，保证语义正确。

`forge.vision()` 返回 engine，可手动调用 `engine:process(frame)`，或读取
`engine:stats()` 的 `scans`、`matches`。注册后默认自动生成 `on_frame`；脚本随后定义
自己的 `on_frame` 即可接管调度，底层能力不会被限制。

## 帧 API

- `frame.width`、`frame.height`、`frame.pts_us`
- `frame:pixel(x, y)`：返回 RGBA
- `frame:find(path, threshold [, roi])`：单尺度最佳匹配
- `frame:find_fast(path, threshold [, roi])`：只精修两个粗候选的极速单尺度匹配
- `frame:find_gray(path, threshold [, roi])` / `frame:find_fast_gray(...)`：直接匹配 I420 亮度平面，跳过整帧 RGB 转换
- `frame:find_first(paths, threshold [, roi])`：按优先级批量匹配
- `frame:find_multiscale(path, threshold [, roi])`：多尺度匹配
- `frame:find_all(path, threshold [, roi])`：全部匹配
- `frame:save(path)`、`frame:crop(path, x1, y1, x2, y2)`

匹配结果包含中心坐标 `x/y`、模板尺寸 `w/h` 和 `confidence`；批量结果还包含
从 1 开始的 `index`。

## 设备与输入 API

- `forge.tap(x, y [, radius_px])`：精确点击或圆形半径内随机点击
- `forge.swipe(...)`、`forge.long_press(...)`、`forge.multi_tap(...)`
- `forge.press_key(...)`、`forge.press_back()`、`forge.input_text(text)`
- `forge.screen_size()`、`forge.wait(ms)`
- `forge.serial()`、`forge.device_name`、`forge.monotonic_ms()`、`forge.log(text)`
- `forge.performance_profile()`：`auto/eco/balanced/realtime`
- `forge.recommended_interval_ms()`：当前性能档位建议的未命中扫描间隔
- `forge.KEY_BACK`、`KEY_HOME`、`KEY_ENTER` 等 Android 键值常量

输入优先使用当前 scrcpy 控制通道，不为每次动作创建 ADB 子进程。脚本文件每次启动
时重新读取，修改后无需重启后端；可用 `SCRCPYFORGE_SCRIPTS_DIR` 更改脚本根目录。

声明式脚本未指定 `interval_ms` 时会自动采用性能档位建议值；明确填写后以脚本配置
为准。基准测试脚本直接使用底层 API，不受档位节流影响。

脚本性能与预览性能彼此独立。`script-profile` 只影响上述 Lua 建议间隔，
`preview-profile` 只影响 JPEG/WebSocket 刷新率；关闭或降低预览不会降低解码帧率，
脚本仍然接收最新解码帧。
