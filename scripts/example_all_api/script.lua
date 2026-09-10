-- ScrcpyForge Lua 实时脚本 API 完整示例
--
-- 运行模型：
--   * 每台设备独立运行一个脚本实例，实例之间变量和输入互不共享。
--   * on_frame(frame) 永远收到当前最新帧；处理较慢时旧帧会被丢弃，不会累积延迟。
--   * 坐标以当前视频帧左上角为 (0, 0)，x 向右、y 向下。

-- 将自己的 template.png 放在本脚本目录；不依赖安装路径或用户名。
local template = forge.asset("template.png")
local last_action_ms = 0
local demonstrated = false

-- forge.serial()：返回当前设备的 ADB serial，例如 10.0.0.2:5555。
forge.log("设备 serial：" .. forge.serial())

-- forge.device_name：scrcpy 握手获得的设备名称。
forge.log("设备名称：" .. forge.device_name)

-- forge.monotonic_ms()：脚本启动后的单调毫秒数，适合节流，不受系统时间调整影响。
-- forge.log(text)：向后端事件流输出日志。
-- forge.wait(ms)：阻塞当前脚本指定毫秒；等待期间仍可响应停止请求。
forge.log(string.format("脚本启动时间：%.1f ms", forge.monotonic_ms()))
forge.log(string.format("性能档位：%s，建议扫描间隔：%dms", forge.performance_profile(), forge.recommended_interval_ms()))

-- forge.script_dir：当前命名脚本的目录。
-- forge.asset(relative_path)：安全拼接脚本目录内的模板/资源，不依赖操作系统和安装位置。
-- 例如：local portable_template = forge.asset("templates/button.png")

-- forge.vision(config)：声明式实时识别 API。它不是硬编码；targets、优先级、
-- 阈值、ROI、节流和动作都由脚本配置。相同参数的目标自动走 candidates 批处理。
-- 声明后运行器会自动创建 on_frame；需要完全自由的逻辑时，仍可使用下方底层 API。
if false then -- 仅展示，不与本示例后面的 on_frame 同时启用
    forge.vision {
        threshold = 0.85,                 -- 所有目标的默认阈值
        roi = {0, 0, 1280, 720},          -- 可省略，即全屏
        interval_ms = 30,                 -- 未命中后至少等待多久再识别，降低功耗
        after_match_ms = 500,             -- 命中后全局冷却
        action = {type = "tap", radius = 4}, -- 默认点击匹配中心
        targets = {
            {name = "按钮A", template = forge.asset("button_a.png")},
            {name = "按钮B", template = forge.asset("button_b.png"),
             -- 单个目标可覆盖默认动作，也可用 on_match 编写任意逻辑。
             on_match = function(match)
                 forge.tap(match.x, match.y, 8)
                 forge.log("命中按钮B")
             end},
        },
        on_match = function(target, match)
            forge.log(string.format("命中 %s，置信度 %.3f", target.name, match.confidence))
        end,
    }
end

-- 可用 Android 键值常量：
-- forge.KEY_BACK / KEY_HOME / KEY_ENTER / KEY_POWER
-- forge.KEY_VOLUME_UP / KEY_VOLUME_DOWN / KEY_MENU

function on_frame(frame)
    -- frame.width / frame.height：当前解码帧尺寸。
    -- frame.pts_us：手机端视频时间戳（微秒）。
    -- frame.frame_seq：解码帧序号；frame.rescan=true 表示静止画面的定时重检。
    local width, height = frame.width, frame.height

    -- forge.screen_size()：控制通道当前坐标尺寸，通常与 frame 尺寸相同。
    local control_width, control_height = forge.screen_size()

    -- frame:pixel(x, y)：读取 RGBA 四个通道，坐标越界会报错。
    local r, g, b, a = frame:pixel(math.floor(width / 2), math.floor(height / 2))

    -- frame:find(path, threshold [, roi])：默认单尺度模板匹配，返回最佳结果或 nil。
    -- roi 格式为 {x1, y1, x2, y2}；不传即搜索全屏。
    local roi = {0, 0, width, math.floor(height / 2)}
    local match = frame:find(template, 0.85, roi)
    -- frame:find_fast(...)：相同返回值，只验证两个粗候选；适合高辨识度模板和低延迟场景。
    -- frame:find_fast_gray(...)：直接使用视频 Y 平面，是形状/UI识别的最低功耗路径。

    -- frame:find_first(paths, threshold [, roi])：批量按优先级匹配。
    -- 所有模板共享一次帧颜色转换，返回值 index 为命中的路径序号。
    -- local first = frame:find_first({"a.png", "b.png"}, 0.85, roi)
    -- frame:find_candidates(paths, threshold [, roi])：返回每个模板的最佳命中，
    -- 每项含 index、x、y、w、h、confidence，适合多个目标同时追踪。

    -- frame:find_multiscale(path, threshold [, roi])：显式多尺度全屏/ROI 搜索。
    -- 当前会测试 1x、1.5x、2x、2.5x；比单尺度更慢，应仅在尺寸未知时使用。
    -- local scaled_match = frame:find_multiscale(template, 0.85)

    -- frame:find_all(path, threshold [, roi])：返回所有达到阈值的单尺度匹配。
    -- 每项包含 x、y、w、h、confidence；结果按置信度从高到低排列。
    -- local matches = frame:find_all(template, 0.90, roi)

    if match then
        forge.log(string.format(
            "匹配：中心=(%d,%d)，尺寸=%dx%d，置信度=%.4f",
            match.x, match.y, match.w, match.h, match.confidence
        ))

        -- forge.tap(x, y [, radius_px])：点击坐标。
        -- radius_px > 0 时，在以 (x,y) 为圆心、指定像素半径的圆内均匀随机点击；
        -- 最终坐标会限制在当前设备屏幕范围内。省略半径或传 0 即精确点击。
        if forge.monotonic_ms() - last_action_ms >= 1000 then
            forge.tap(match.x, match.y, 6)
            last_action_ms = forge.monotonic_ms()
        end
    end

    -- 以下输入 API 会真实操作手机。本示例只演示一次，并默认关闭。
    local ENABLE_INPUT_DEMO = false
    if ENABLE_INPUT_DEMO and not demonstrated then
        demonstrated = true

        forge.tap(100, 200)                 -- 精确点击
        forge.tap(100, 200, 12)             -- 12 px 圆形半径内随机点击
        forge.swipe(100, 600, 100, 200, 350) -- 起点、终点、持续毫秒数
        forge.long_press(300, 400, 800)      -- 长按 800 ms

        -- forge.multi_tap(points)：多指同时按下和抬起。
        forge.multi_tap({{200, 300}, {400, 300}})

        forge.press_key(forge.KEY_HOME)          -- 短按按键
        forge.press_key(forge.KEY_VOLUME_UP, true) -- 长按按键
        forge.press_back()                       -- 返回/亮屏
        forge.input_text("ScrcpyForge")          -- 输入 UTF-8 文本
        forge.wait(200)
    end

    -- 以下帧文件 API 默认关闭，避免每帧写盘。
    local ENABLE_FILE_DEMO = false
    if ENABLE_FILE_DEMO and not demonstrated then
        frame:save(forge.asset("scrcpyforge-frame.png"))
        frame:crop(forge.asset("scrcpyforge-region.png"), 0, 0, width / 2, height / 2)
    end

    -- 这些变量仅用于展示 API，避免 Lua 静态检查器认为未使用。
    if false then
        forge.log(string.format(
            "frame=%dx%d control=%dx%d pts=%d rgba=%d,%d,%d,%d",
            width, height, control_width, control_height, frame.pts_us, r, g, b, a
        ))
    end
end
