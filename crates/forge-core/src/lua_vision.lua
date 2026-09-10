-- ScrcpyForge declarative vision policy.
--
-- Matching stays in Rust/OpenCV. This layer owns per-target state: cooldown,
-- rearm, local/expanded/full search and stable-position tracking. Cooldown and
-- rearm observations are rate-limited so the target can still be rearmed while
-- avoiding a full template match on every frame.
local engines = {}

local function same_roi(a, b)
    if a == b then return true end
    if type(a) ~= "table" or type(b) ~= "table" then return false end
    return a[1] == b[1] and a[2] == b[2] and a[3] == b[3] and a[4] == b[4]
end

local function roi_key(roi)
    if roi == nil then return "full" end
    return table.concat({roi[1], roi[2], roi[3], roi[4]}, ":")
end

local function enabled(target, frame)
    if target.enabled == nil then return true end
    if type(target.enabled) == "function" then return target.enabled(frame, target) end
    return target.enabled == true
end

local function clamp_roi(roi, frame)
    if roi == nil then return nil end
    local width, height = tonumber(frame.width) or 0, tonumber(frame.height) or 0
    local x1 = math.max(0, math.min(width, math.floor(roi[1] or 0)))
    local y1 = math.max(0, math.min(height, math.floor(roi[2] or 0)))
    local x2 = math.max(0, math.min(width, math.floor(roi[3] or width)))
    local y2 = math.max(0, math.min(height, math.floor(roi[4] or height)))
    if x2 <= x1 or y2 <= y1 then return nil end
    return {x1, y1, x2, y2}
end

local function track_roi(track, frame, margin, now)
    local match = track.last_match
    if not match then return nil end
    local dt = math.max(0, (now or track.last_seen_ms or 0) - (track.last_seen_ms or 0)) / 1000
    local center_x = (match.x or 0) + (track.velocity_x or 0) * dt
    local center_y = (match.y or 0) + (track.velocity_y or 0) * dt
    local half_w = math.floor((match.w or 0) / 2)
    local half_h = math.floor((match.h or 0) / 2)
    return clamp_roi({
        center_x - half_w - margin,
        center_y - half_h - margin,
        center_x + math.ceil((match.w or 0) / 2) + margin,
        center_y + math.ceil((match.h or 0) / 2) + margin,
    }, frame)
end

local function distance(a, b)
    local dx, dy = (a.x or 0) - (b.x or 0), (a.y or 0) - (b.y or 0)
    return math.sqrt(dx * dx + dy * dy)
end

local function tolerance(frame, tracking)
    if tracking.position_tolerance_px then return tracking.position_tolerance_px end
    local width, height = tonumber(frame.width) or 0, tonumber(frame.height) or 0
    return math.max(8, math.min(24, math.floor(math.max(width, height) * 0.01)))
end

local function reset_track(track)
    track.last_match = nil
    track.history = {}
    track.stable_count = 0
    track.locked = false
    track.miss_count = 0
    track.last_confidence = 0
    track.last_seen_ms = 0
    track.velocity_x = 0
    track.velocity_y = 0
    track.expansion_level = 0
    track.next_observe_ms = 0
    track.just_rearmed = false
end

local function push_history(track, match, max_length)
    track.history[#track.history + 1] = {x = match.x, y = match.y}
    while #track.history > max_length do table.remove(track.history, 1) end
end

local function update_track(track, match, frame, now, tracking)
    local history_length = tracking.stable_history or 4
    local needed = tracking.stable_hits or 3
    local limit = tolerance(frame, tracking)
    if track.last_match then
        local old_w, old_h = math.max(track.last_match.w or 0, 1), math.max(track.last_match.h or 0, 1)
        local width_change = math.abs((match.w or 0) - old_w) / old_w
        local height_change = math.abs((match.h or 0) - old_h) / old_h
        if math.max(width_change, height_change) > (tracking.size_change_ratio or 0.15) then
            -- A scale/asset change invalidates the old ROI and stability
            -- history; the new observation starts a fresh VERIFY sequence.
            track.history = {}
            track.stable_count = 0
            track.locked = false
            track.velocity_x = 0
            track.velocity_y = 0
        end
    end
    if track.last_match and track.last_seen_ms and now > track.last_seen_ms then
        local dt = math.max(0.001, (now - track.last_seen_ms) / 1000)
        local measured_x = ((match.x or 0) - (track.last_match.x or 0)) / dt
        local measured_y = ((match.y or 0) - (track.last_match.y or 0)) / dt
        track.velocity_x = (track.velocity_x or 0) * 0.65 + measured_x * 0.35
        track.velocity_y = (track.velocity_y or 0) * 0.65 + measured_y * 0.35
    end
    push_history(track, match, history_length)
    local stable = 0
    for _, point in ipairs(track.history) do
        if distance(point, match) <= limit then stable = stable + 1 end
    end
    track.stable_count = stable
    track.locked = #track.history >= history_length and stable >= needed
    track.last_match = match
    track.last_confidence = match.confidence or 0
    track.last_seen_ms = now
    track.miss_count = 0
    track.absent_count = 0
    track.expansion_level = 0
end

local function apply_action(target, match, frame, defaults)
    if target.on_match then return target.on_match(match, frame, target) end
    local action = target.action or defaults.action
    if action == nil or action == "none" then return end
    if type(action) == "function" then return action(match, frame, target) end
    if action == "tap" then action = {type = "tap"} end
    if type(action) ~= "table" then
        error("vision action must be a table, function, 'tap', or 'none'")
    end

    local kind = action.type or "tap"
    if kind == "tap" then
        forge.tap(
            match.x + (action.offset_x or 0),
            match.y + (action.offset_y or 0),
            action.radius or target.radius or defaults.radius or 0
        )
    elseif kind == "key" then
        forge.press_key(assert(action.code, "key action requires code"), action.long_press or false)
    elseif kind == "swipe" then
        forge.swipe(action.x1, action.y1, action.x2, action.y2, action.duration_ms)
    elseif kind ~= "none" then
        error("unsupported vision action type: " .. tostring(kind))
    end
end

local function new_engine(config)
    assert(type(config) == "table", "forge.vision expects a configuration table")
    assert(type(config.targets) == "table" and #config.targets > 0,
        "vision.targets must not be empty")
    local tracking = type(config.tracking) == "table" and config.tracking or {}
    if config.tracking == true then tracking.enabled = true end
    if forge._set_rescan_interval_ms then
        forge._set_rescan_interval_ms(config.rescan_interval_ms or 0)
    end
    local engine = {
        config = config,
        tracks = {},
        next_scan_ms = 0,
        last_frame_seq = 0,
        last_scene_signature = nil,
        idle_backoff_ms = 0,
        scans = 0,
        matches = 0,
        full_scans = 0,
        local_scans = 0,
        expanded_scans = 0,
        local_hits = 0,
        fallbacks = 0,
        track_locks = 0,
        track_resets = 0,
        cooldown_suppressed = 0,
        rearm_suppressed = 0,
        action_failures = 0,
        below_threshold = 0,
        duplicate_frames_skipped = 0,
        scene_gate_skipped = 0,
        full_backoff_skipped = 0,
        observation_backoff_skipped = 0,
        last_full_scan_ms = -math.huge,
        next_full_scan_ms = 0,
        frame_size = nil,
    }

    for i, target in ipairs(config.targets) do
        assert(type(target) == "table", "each vision target must be a table")
        target.name = target.name or tostring(i)
        target.template = assert(target.template or target.path,
            "vision target requires template")
        target.rearm = target.rearm or "disappear"
        assert(target.rearm == "disappear" or target.rearm == "timer",
            "vision target rearm must be 'disappear' or 'timer'")
        engine.tracks[i] = {
            history = {},
            next_action_ms = 0,
            next_observe_ms = 0,
            just_rearmed = false,
            armed = true,
            absent_count = 0,
            miss_count = 0,
            locked = false,
            stable_count = 0,
            velocity_x = 0,
            velocity_y = 0,
            expansion_level = 0,
            last_frame_width = 0,
            last_frame_height = 0,
            last_scan_mode = "none",
            last_scanned_seq = 0,
            last_decision = "none",
            last_roi = nil,
            last_score = 0,
        }
    end

    local function scan_one(frame, target, threshold, roi)
        local mode = target.mode or config.mode or "color"
        local fast = target.fast == true or (target.fast == nil and config.fast == true)
        local multi = target.multiscale == true or
            (target.multiscale == nil and config.multiscale == true)
        if multi then return frame:find_multiscale(target.template, threshold, roi) end
        local gray_gate = target.gray_gate == true or
            (target.gray_gate == nil and config.gray_gate == true)
        if mode == "color" and gray_gate then
            local gray_threshold = target.gray_threshold or config.gray_threshold or
                math.max(0, threshold - 0.12)
            local candidate = frame:find_fast_gray(target.template, gray_threshold, roi)
            if not candidate then return nil end
        end
        if mode == "gray" then
            if fast then return frame:find_fast_gray(target.template, threshold, roi) end
            return frame:find_gray(target.template, threshold, roi)
        end
        if fast then return frame:find_fast(target.template, threshold, roi) end
        return frame:find(target.template, threshold, roi)
    end

    function engine:process(frame)
        local now = forge.monotonic_ms()
        local frame_seq = tonumber(frame.frame_seq or 0) or 0
        if frame_seq > 0 and frame_seq == self.last_frame_seq and not frame.rescan then
            self.duplicate_frames_skipped = self.duplicate_frames_skipped + 1
            return nil
        end
        if frame_seq > 0 then self.last_frame_seq = frame_seq end
        if now < self.next_scan_ms then return nil end
        local cfg, targets = self.config, self.config.targets
        local observation_due = false
        for i, target in ipairs(targets) do
            local track = self.tracks[i]
            local suppressed = not track.armed or now < track.next_action_ms
            if enabled(target, frame) and suppressed and
                now >= (track.next_observe_ms or 0) then
                observation_due = true
                break
            end
        end
        if config.scene_gate == true and frame.scene_signature ~= nil and
            frame.scene_signature == self.last_scene_signature and not observation_due then
            self.scene_gate_skipped = self.scene_gate_skipped + 1
            self.next_scan_ms = now + math.max(config.scene_gate_interval_ms or 100, 100)
            return nil
        end
        self.last_scene_signature = frame.scene_signature
        local geometry_changed = self.frame_size == nil or
            self.frame_size[1] ~= frame.width or self.frame_size[2] ~= frame.height
        if geometry_changed then
            self.frame_size = {frame.width, frame.height}
            for _, track in ipairs(self.tracks) do
                reset_track(track)
                track.armed = true
                track.next_action_ms = 0
            end
            self.track_resets = self.track_resets + 1
        end

        local tracking_enabled = tracking.enabled ~= false
        local full_scan_round = not tracking_enabled
        local full_scan_started = false
        local full_backoff_wait = false

        local plans, groups = {}, {}
        for i, target in ipairs(targets) do
            local active = enabled(target, frame)
            local track = self.tracks[i]
            local threshold = target.threshold or cfg.threshold or 0.8
            local roi = clamp_roi(target.roi or cfg.roi, frame)
            local mode = target.mode or cfg.mode or "color"
            local fast = target.fast == true or (target.fast == nil and cfg.fast == true)
            local multi = target.multiscale == true or
                (target.multiscale == nil and cfg.multiscale == true)
            local scan_mode = "full"
            local observe_interval = tonumber(target.observe_interval_ms or
                tracking.observe_interval_ms or cfg.observe_interval_ms or 250) or 250
            observe_interval = math.max(50, observe_interval)
            local suppressed = not track.armed or now < track.next_action_ms
            local observe_backoff = active and suppressed and now < (track.next_observe_ms or 0)
            -- A due cooldown/rearm probe is intentionally independent from
            -- the global full-recovery cadence. Otherwise a target clicked
            -- before it became locked could take several seconds to collect
            -- its disappearance frames and become armed again.
            local observation_probe = active and suppressed and not observe_backoff
            local rearm_wake = active and track.just_rearmed == true
            local skip = not active or observe_backoff
            if observe_backoff then
                scan_mode = "observe_backoff"
                self.observation_backoff_skipped = self.observation_backoff_skipped + 1
            elseif active and tracking_enabled and target.roi == nil and cfg.roi == nil and track.locked then
                local margin = tracking.margin or 64
                if track.miss_count == 0 then
                    scan_mode = "local"
                    roi = track_roi(track, frame, margin, now)
                else
                    local margins = tracking.expanded_margins or {
                        tracking.expanded_margin or math.max(margin * 2, 128),
                        math.max(margin * 4, 256),
                        math.max(margin * 8, 512),
                    }
                    if type(margins) ~= "table" or #margins == 0 then
                        margins = {math.max(margin * 2, 128), math.max(margin * 4, 256),
                            math.max(margin * 8, 512)}
                    end
                    local level = math.min(track.miss_count, #margins)
                    margin = tonumber(margins[level] or margins[#margins]) or
                        math.max(margin * 2, 128)
                    scan_mode = "expanded"
                    roi = track_roi(track, frame, margin, now)
                    if track.miss_count >= (tracking.miss_before_reset or 2) then
                        if now >= self.next_full_scan_ms then
                            scan_mode = "full"
                            roi = nil
                            self.fallbacks = self.fallbacks + 1
                            full_scan_started = true
                        else
                            skip = true
                            scan_mode = "backoff"
                            full_backoff_wait = true
                            self.full_backoff_skipped = self.full_backoff_skipped + 1
                        end
                    end
                end
                if roi == nil and scan_mode ~= "full" then
                    scan_mode = "full"
                    self.fallbacks = self.fallbacks + 1
                    full_scan_started = true
                end
            elseif active and tracking_enabled and target.roi == nil and cfg.roi == nil and not track.locked then
                if observation_probe or rearm_wake or now >= self.next_full_scan_ms then
                    scan_mode = "full"
                    full_scan_started = not observation_probe and not rearm_wake
                else
                    skip = true
                    scan_mode = "backoff"
                    full_backoff_wait = true
                    self.full_backoff_skipped = self.full_backoff_skipped + 1
                end
            end
            if rearm_wake and not skip then track.just_rearmed = false end
            if not skip and scan_mode == "local" then self.local_scans = self.local_scans + 1 end
            if not skip and scan_mode == "expanded" then self.expanded_scans = self.expanded_scans + 1 end
            if not skip and scan_mode == "full" then full_scan_round = true end
            plans[i] = {
                active = active,
                skip = skip,
                threshold = threshold,
                roi = roi,
                mode = mode,
                fast = fast,
                multi = multi,
                scan_mode = scan_mode,
                observation_skipped = observe_backoff,
            }
            if active and skip then
                track.last_scan_mode = scan_mode
                track.last_decision = scan_mode == "observe_backoff" and
                    "observation_backoff" or "scan_backoff"
            end
            if active and not skip and not multi and not fast and mode == "color" and
                target.gray_gate ~= true and cfg.gray_gate ~= true then
                local key = tostring(threshold) .. "|" .. roi_key(roi)
                groups[key] = groups[key] or {indexes = {}, paths = {}, threshold = threshold, roi = roi}
                groups[key].indexes[#groups[key].indexes + 1] = i
                groups[key].paths[#groups[key].paths + 1] = target.template
            end
        end

        local found = {}
        for _, group in pairs(groups) do
            if frame.find_candidates then
                local candidates = frame:find_candidates(group.paths, group.threshold, group.roi)
                for _, match in ipairs(candidates or {}) do
                    local local_index = match.index or 0
                    local target_index = group.indexes[local_index]
                    if target_index then found[target_index] = match end
                end
            elseif #group.paths > 1 then
                local match = frame:find_first(group.paths, group.threshold, group.roi)
                if match then
                    local target_index = group.indexes[match.index or 0]
                    if target_index then found[target_index] = match end
                end
            else
                local index = group.indexes[1]
                local match = scan_one(frame, targets[index], group.threshold, group.roi)
                if match then found[index] = match end
            end
        end
        for i, plan in ipairs(plans) do
            if plan.active and not plan.skip and not found[i] and
                (plan.multi or plan.fast or plan.mode ~= "color") then
                local match = scan_one(frame, targets[i], plan.threshold, plan.roi)
                if match then found[i] = match end
            end
        end

        if full_scan_started then
            self.last_full_scan_ms = now
            self.next_full_scan_ms = now + (tracking.full_scan_interval_ms or 1500)
        end
        if full_scan_round then self.full_scans = self.full_scans + 1 end
        self.scans = self.scans + 1
        local selected, had_match = nil, false
        local rearmed_any = false
        for i, target in ipairs(targets) do
            local track, match = self.tracks[i], found[i]
            if not plans[i].active or plans[i].skip then
                goto continue
            end
            track.last_scan_mode = plans[i].scan_mode
            track.last_scanned_seq = frame_seq
            track.last_roi = plans[i].roi
            track.last_score = match and (match.confidence or 0) or 0
            if match then
                match.index, match.name = i, target.name
                self.matches = self.matches + 1
                had_match = true
                if plans[i].scan_mode == "local" then self.local_hits = self.local_hits + 1 end
                local was_locked = track.locked
                update_track(track, match, frame, now, tracking)
                if not was_locked and track.locked then self.track_locks = self.track_locks + 1 end
                if not track.armed then
                    self.rearm_suppressed = self.rearm_suppressed + 1
                    track.last_decision = "rearm"
                elseif now < track.next_action_ms then
                    self.cooldown_suppressed = self.cooldown_suppressed + 1
                    track.last_decision = "cooldown"
                else
                    local ok, error_message = pcall(apply_action, target, match, frame, cfg)
                    if not ok then
                        self.action_failures = self.action_failures + 1
                        track.locked = false
                        track.last_decision = "error"
                        forge.log(string.format("目标 %s 动作失败：%s", target.name, tostring(error_message)))
                    else
                        local cooldown = target.cooldown_ms or cfg.cooldown_ms or 0
                        track.next_action_ms = now + cooldown
                        if target.rearm == "disappear" then track.armed = false end
                        track.last_decision = "hit"
                    end
                    if cfg.on_match then cfg.on_match(target, match, frame) end
                    selected = selected or match
                end
            else
                self.below_threshold = self.below_threshold + 1
                track.last_decision = track.armed and "below_threshold" or "rearm_wait"
                track.miss_count = track.miss_count + 1
                if not track.armed then
                    track.absent_count = track.absent_count + 1
                    if track.absent_count >= (target.disappear_frames or 3) then
                        track.armed = true
                        track.absent_count = 0
                        track.just_rearmed = true
                        rearmed_any = true
                    end
                end
                if track.miss_count >= (tracking.miss_before_reset or 2) and track.locked then
                    track.locked = false
                    track.history = {}
                    track.stable_count = 0
                    self.track_resets = self.track_resets + 1
                end
            end
            local observe_interval = tonumber(target.observe_interval_ms or
                tracking.observe_interval_ms or cfg.observe_interval_ms or 250) or 250
            observe_interval = math.max(50, observe_interval)
            if not track.armed then
                track.next_observe_ms = now + observe_interval
            elseif now < track.next_action_ms then
                track.next_observe_ms = track.next_action_ms
            else
                track.next_observe_ms = 0
            end
            ::continue::
        end

        -- Keep the vision module usable with a minimal forge table (for
        -- example, an isolated script test) while using the host's profile
        -- recommendation whenever it is available.
        local recommended_interval = forge.recommended_interval_ms and
            forge.recommended_interval_ms() or 50
        local base_interval = cfg.interval_ms or recommended_interval
        local interval = base_interval
        if interval <= 0 then interval = 16 end
        if selected and cfg.after_match_ms and cfg.after_match_ms > interval then
            interval = cfg.after_match_ms
        end
        if had_match then
            self.idle_backoff_ms = interval
        else
            self.idle_backoff_ms = math.min(
                math.max(interval, self.idle_backoff_ms > 0 and self.idle_backoff_ms * 2 or interval),
                cfg.max_backoff_ms or 8000
            )
            interval = self.idle_backoff_ms
        end
        if rearmed_any then
            -- A disappearance observation is a wake-up edge. Do not let a
            -- previously accumulated no-match backoff postpone the first scan
            -- of the newly re-armed target.
            self.idle_backoff_ms = 0
            interval = math.min(interval, math.max(base_interval, 16))
        end
        if full_backoff_wait and self.next_full_scan_ms > now then
            interval = math.min(interval, math.max(16, self.next_full_scan_ms - now))
        end
        for i, _ in ipairs(targets) do
            if plans[i].active then
                local wake = self.tracks[i].next_observe_ms or 0
                if wake > now then interval = math.min(interval, wake - now) end
            end
        end
        self.next_scan_ms = now + interval
        if not selected and cfg.on_idle then cfg.on_idle(frame, self) end
        return selected
    end

    function engine:stats()
        return {
            scans = self.scans,
            matches = self.matches,
            full_scans = self.full_scans,
            local_scans = self.local_scans,
            expanded_scans = self.expanded_scans,
            local_hits = self.local_hits,
            fallbacks = self.fallbacks,
            track_locks = self.track_locks,
            track_resets = self.track_resets,
            cooldown_suppressed = self.cooldown_suppressed,
            rearm_suppressed = self.rearm_suppressed,
            action_failures = self.action_failures,
            below_threshold = self.below_threshold,
            duplicate_frames_skipped = self.duplicate_frames_skipped,
            scene_gate_skipped = self.scene_gate_skipped,
            full_backoff_skipped = self.full_backoff_skipped,
            observation_backoff_skipped = self.observation_backoff_skipped,
            idle_backoff_ms = self.idle_backoff_ms,
            last_full_scan_ms = self.last_full_scan_ms,
            next_scan_ms = self.next_scan_ms,
            targets = (function()
                local result = {}
                for index, track in ipairs(self.tracks) do
                    result[index] = {
                        locked = track.locked,
                        armed = track.armed,
                        miss_count = track.miss_count,
                        absent_count = track.absent_count,
                        stable_count = track.stable_count,
                        last_confidence = track.last_confidence,
                        last_match = track.last_match,
                        last_scan_mode = track.last_scan_mode,
                        last_scanned_seq = track.last_scanned_seq,
                        last_decision = track.last_decision,
                        last_roi = track.last_roi,
                        last_score = track.last_score,
                        next_action_ms = track.next_action_ms,
                        next_observe_ms = track.next_observe_ms,
                    }
                end
                return result
            end)(),
        }
    end
    return engine
end

local function register(config)
    local engine = new_engine(config)
    engines[#engines + 1] = engine
    _G.on_frame = function(frame)
        for _, item in ipairs(engines) do item:process(frame) end
    end
    return engine
end

local vision_api = {compile = register}
setmetatable(vision_api, {__call = function(_, config) return register(config) end})
forge.vision = vision_api
