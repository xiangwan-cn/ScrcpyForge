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
`frame:find_candidates()`，共享帧颜色转换并为每个模板保留最佳候选。冷却或未重新武装时
只按 `observe_interval_ms` 低频观察，正常状态才逐帧更新轨迹；目标覆盖不同参数、动态禁用
或启用多尺度时会按对应策略单独匹配。

调度器为每个目标独立记录最近中心、稳定历史、局部 ROI 和 miss 次数。锁定后优先搜索
上次命中附近，局部失败会扩大 ROI，持续失败或周期校正时回到全屏。`tracking = false`
可以关闭位置记忆并保留全屏路径。目标默认 `rearm = "disappear"`，点击后需连续若干次
未命中才允许同一实例再次点击；需要固定周期点击且目标持续存在时可改为
`rearm = "timer"`。设置 `rescan_interval_ms` 后，声明式引擎才会在没有新视频帧时
低频重检同一最新帧（非零值限制在 50–60000ms）；默认路径等待新帧或取消信号。

`forge.vision()` 返回 engine，可手动调用 `engine:process(frame)`，或读取
`engine:stats()` 的 `scans`、`matches`、`below_threshold`、
`duplicate_frames_skipped`、`scene_gate_skipped`、`full_backoff_skipped`、
`observation_backoff_skipped` 和逐目标 `scan_mode`、`last_decision`、`last_score` 状态。
注册后默认自动生成 `on_frame`；脚本随后定义
自己的 `on_frame` 即可接管调度，底层能力不会被限制。

## 帧 API

- `frame.width`、`frame.height`、`frame.pts_us`、`frame.frame_seq`
- `frame.rescan` / `frame.rescan_reason`：静止画面的定时重检及原因
- `frame:pixel(x, y)`：返回 RGBA
- `frame:find(path, threshold [, roi])`：单尺度最佳匹配
- `frame:find_fast(path, threshold [, roi])`：只精修两个粗候选的极速单尺度匹配
- `frame:find_gray(path, threshold [, roi])` / `frame:find_fast_gray(...)`：直接匹配 I420 亮度平面，跳过整帧 RGB 转换
- `frame:find_first(paths, threshold [, roi])`：按优先级批量匹配
- `frame:find_candidates(paths, threshold [, roi])`：为每个模板返回最佳候选，结果含
  `index`、`position`、`x`、`y`、`w`、`h`、`confidence`
- `frame:find_multiscale(path, threshold [, roi])`：多尺度匹配
- `frame:find_all(path, threshold [, roi])`：全部匹配
- `frame:save(path)`、`frame:crop(path, x1, y1, x2, y2)`

视觉策略可按目标启用 `gray_gate = true`（或在顶层配置），先用 Y 平面快速筛选，
只有候选通过 `gray_threshold` 才执行彩色确认；该开关默认关闭以保持召回率。

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

声明式视觉配置可选 `scene_gate = true` 跳过相同亮度场景，也可设置
`rescan_interval_ms` 对静止画面按需重检；相同 `frame_seq` 不会重复执行模板匹配。
