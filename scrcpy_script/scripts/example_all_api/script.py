"""演示所有 ScriptAPI 方法的完整示例。"""
import random


def script(api):
    api.log(f"==== 设备: {api.device_serial()} ({api.device_name()}) ====")

    # ── 帧信息 ──
    sw, sh = api.screen_size()
    vw, vh = api.video_size()
    api.log(f"屏幕分辨率: {sw}x{sh}, 视频分辨率: {vw}x{vh}")
    api.log(f"已旋转: {api.is_rotated()}")

    # ── 截图 ──
    api.screenshot("example_start.png")
    api.log("已保存初始截图 example_start.png")

    # ── capture 获取原始帧 ──
    frame = api.capture()
    if frame is not None:
        api.log(f"帧尺寸: {frame.shape}, dtype={frame.dtype}")

    # ── 全帧模板匹配 ──
    m = api.find("templates/template.png", threshold=0.8)
    if m:
        api.log(
            f"find 结果: ({m['x']},{m['y']}) "
            f"{m['w']}x{m['h']} conf={m['confidence']:.2f}"
        )

    # ── ROI 限定区域匹配 ──
    m = api.find("templates/template.png", threshold=0.7,
                 roi=(100, 200, 600, 800))
    if m:
        api.log(f"ROI 匹配: ({m['x']},{m['y']})")

    # ── 查找全部 ──
    matches = api.find_all("templates/template.png", threshold=0.7)
    api.log(f"find_all: 找到 {len(matches)} 个匹配")

    # ── 等待出现 ──
    api.log("wait_for: 等待目标出现 (5s 超时)...")
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

    # ── 触摸 — tap ──
    api.tap(sw // 2, sh // 2)
    api.log(f"tap 屏幕中心 ({sw // 2}, {sh // 2})")

    # ── 长按 ──
    api.long_press(sw // 2, sh // 2, duration_ms=800)
    api.log("long_press 中心 800ms")

    # ── 滑动 ──
    api.swipe(sw // 2, sh * 2 // 3, sw // 2, sh // 3, duration_ms=500)
    api.log("swipe 向上滑动")

    # ── 多点触控 ──
    api.multi_tap([
        (sw // 3, sh // 2),
        (sw * 2 // 3, sh // 2),
    ])
    api.log("multi_tap 双指点击")

    # ── 按键 ──
    api.press_back()
    api.log("press_back 返回")

    api.press_key(api.KEY_HOME)
    api.log("press_key HOME")

    api.press_key(api.KEY_POWER, long_press=True)
    api.log("press_key POWER 长按")

    api.press_key(api.KEY_VOLUME_UP)
    api.log("press_key VOLUME_UP")

    # ── 文本输入 ──
    api.input_text("Hello ScrcpyScript")
    api.log("input_text 已输入")

    # ── wait ──
    api.wait(1000)
    api.log("wait 1000ms")

    # ── 循环点击 5 次（随机偏移） ──
    for i in range(5):
        ox = random.randint(-5, 5)
        oy = random.randint(-5, 5)
        api.tap(sw // 2 + ox, sh // 2 + oy)
        api.log(f"第 {i + 1} 次点击 ({sw // 2 + ox}, {sh // 2 + oy})")
        api.wait(500)

    # ── 完成 ──
    api.warn("示例脚本执行完毕")
    api.log("==== 所有 API 调用完成 ====")
