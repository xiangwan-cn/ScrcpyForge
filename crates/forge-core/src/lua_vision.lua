-- ScrcpyForge declarative vision policy.
--
-- There are deliberately only two matching paths here:
--   * scan_global: full-frame matching while no persistent position is known;
--   * scan_persistent_roi: matching in the saved 2x3-template ROI.
--
-- A target enters the ROI path after three consecutive matches whose centers
-- are close enough to one another. The host writes the state next to the
-- named script in a device-scoped file, so a script restart can reuse the
-- correct device's ROI. A miss never widens the ROI back to a full-frame
-- search; deleting that device's state file is the explicit reset.
local engines = {}
local MAX_ENGINES = 16

local function bounded(value, fallback, low, high, label)
    if value == nil then return fallback end
    assert(type(value) == "number" and value == value and value > -math.huge and
        value < math.huge, label .. " must be a finite number")
    return math.floor(math.min(high, math.max(low, value)))
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

local function roi_key(roi)
    if roi == nil then return "full" end
    return table.concat({roi[1], roi[2], roi[3], roi[4]}, ":")
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

-- The state file is intentionally a small, line-oriented format. Lua in the
-- sandbox has no file or JSON library; the host provides only the two bounded
-- state-file calls used below.
local function state_escape(value)
    return tostring(value)
        :gsub("%%", "%%25")
        :gsub("|", "%%7C")
        :gsub("\r", "%%0D")
        :gsub("\n", "%%0A")
end

local function state_unescape(value)
    return tostring(value)
        :gsub("%%0D", "\r")
        :gsub("%%0A", "\n")
        :gsub("%%7C", "|")
        :gsub("%%25", "%%")
end

local function split_state_line(line)
    local fields, start = {}, 1
    for index = 1, 8 do
        local separator = string.find(line, "|", start, true)
        if not separator then return nil end
        fields[index] = string.sub(line, start, separator - 1)
        start = separator + 1
    end
    fields[9] = string.sub(line, start)
    return fields
end

local function state_number(value, minimum)
    local number = tonumber(value)
    if number == nil or number ~= number or number == math.huge or number == -math.huge then
        return nil
    end
    if minimum ~= nil and number < minimum then return nil end
    return math.floor(number)
end

local function load_persistent_states()
    local states = {}
    if type(forge._vision_state_load) ~= "function" then return states end
    local ok, content = pcall(forge._vision_state_load)
    if not ok or type(content) ~= "string" then return states end
    for line in string.gmatch(content, "[^\r\n]+") do
        if line ~= "version=1" then
            local fields = split_state_line(line)
            if fields then
                local identity = state_unescape(fields[1])
                local record = {
                    frame_width = state_number(fields[2], 1),
                    frame_height = state_number(fields[3], 1),
                    template_width = state_number(fields[4], 1),
                    template_height = state_number(fields[5], 1),
                    roi = {
                        state_number(fields[6], 0), state_number(fields[7], 0),
                        state_number(fields[8], 1), state_number(fields[9], 1),
                    },
                }
                if identity ~= "" and record.frame_width and record.frame_height and
                    record.template_width and record.template_height and record.roi[1] and
                    record.roi[2] and record.roi[3] and record.roi[4] and
                    record.roi[3] > record.roi[1] and record.roi[4] > record.roi[2] then
                    states[identity] = record
                end
            end
        end
    end
    return states
end

local function save_persistent_states(states)
    if type(forge._vision_state_save) ~= "function" then return false end
    local lines = {"version=1"}
    for identity, record in pairs(states) do
        if record and record.roi then
            lines[#lines + 1] = table.concat({
                state_escape(identity),
                record.frame_width, record.frame_height,
                record.template_width, record.template_height,
                record.roi[1], record.roi[2], record.roi[3], record.roi[4],
            }, "|")
        end
    end
    local ok, error_message = pcall(forge._vision_state_save, table.concat(lines, "\n") .. "\n")
    if not ok and type(forge.log) == "function" then
        pcall(forge.log, "视觉位置状态保存失败：" .. tostring(error_message))
    end
    return ok
end

local function target_identity(target)
    local template = tostring(target.template)
    -- forge.asset() exposes an absolute path. Store the path relative to the
    -- script directory so copying/moving a whole script package keeps its ROI.
    local root = forge.script_dir
    if type(root) == "string" and
        (template:sub(1, #root + 1) == root .. "/" or
            template:sub(1, #root + 1) == root .. "\\") then
        template = template:sub(#root + 1):gsub("^[\\/]", "")
    end
    return tostring(target.name) .. "\t" .. template
end

local function persistent_roi_from_history(track, match, frame, tracking)
    local template_width = math.max(1, math.floor(tonumber(match.w) or 1))
    local template_height = math.max(1, math.floor(tonumber(match.h) or 1))
    local center_x, center_y = tonumber(match.x) or 0, tonumber(match.y) or 0
    if #track.history > 0 then
        center_x, center_y = 0, 0
        for _, point in ipairs(track.history) do
            center_x = center_x + (point.x or 0)
            center_y = center_y + (point.y or 0)
        end
        center_x = center_x / #track.history
        center_y = center_y / #track.history
    end
    local roi_width = template_width * (tracking.roi_width_multiplier or 2)
    local roi_height = template_height * (tracking.roi_height_multiplier or 3)
    return clamp_roi({
        center_x - roi_width / 2,
        center_y - roi_height / 2,
        center_x + roi_width / 2,
        center_y + roi_height / 2,
    }, frame), template_width, template_height
end

local function reset_track(track)
    track.last_match = nil
    track.history = {}
    track.stable_count = 0
    track.locked = false
    track.persistent_roi = nil
    track.miss_count = 0
    track.absent_count = 0
    track.last_confidence = 0
    track.last_seen_ms = 0
    track.next_observe_ms = 0
    track.next_scan_ms = 0
    track.just_rearmed = false
    track.pending_match = nil
    track.pending_action_at = 0
    track.wait_until_ms = 0
end

local function update_track(track, match, frame, now, tracking)
    local needed = tracking.stable_hits or 3
    local history_length = math.max(needed, tracking.stable_history or needed)
    local invalidated = false
    if track.last_match then
        local old_w = math.max(track.last_match.w or 0, 1)
        local old_h = math.max(track.last_match.h or 0, 1)
        local width_change = math.abs((match.w or 0) - old_w) / old_w
        local height_change = math.abs((match.h or 0) - old_h) / old_h
        if math.max(width_change, height_change) > (tracking.size_change_ratio or 0.15) then
            track.history = {}
            track.stable_count = 0
            if track.persistent_roi then
                track.persistent_roi = nil
                track.locked = false
                invalidated = true
            end
        end
    end

    if not track.persistent_roi then
        local limit = tolerance(frame, tracking)
        local anchor = track.history[1]
        if anchor and distance(anchor, match) > limit then track.history = {} end
        track.history[#track.history + 1] = {x = match.x, y = match.y}
        while #track.history > history_length do table.remove(track.history, 1) end
        track.stable_count = #track.history
        if #track.history >= needed then
            local roi = persistent_roi_from_history(track, match, frame, tracking)
            if roi then
                track.persistent_roi = roi
                track.locked = true
            end
        end
    else
        track.stable_count = needed
        track.locked = true
    end

    track.last_match = match
    track.last_confidence = match.confidence or 0
    track.last_seen_ms = now
    track.miss_count = 0
    track.absent_count = 0
    return track.locked, invalidated
end

-- These are the only two matching functions selected by the policy. Batch
-- wrappers retain the native multi-template fast path for simple color
-- templates without introducing another search strategy.
local function scan_one(frame, target, threshold, roi, defaults)
    defaults = defaults or {}
    local mode = target.mode or defaults.mode or "color"
    local fast = target.fast == true or (target.fast == nil and defaults.fast == true)
    local multi = target.multiscale == true or
        (target.multiscale == nil and defaults.multiscale == true)
    if multi then return frame:find_multiscale(target.template, threshold, roi) end
    local gray_gate = target.gray_gate == true or
        (target.gray_gate == nil and defaults.gray_gate == true)
    if mode == "color" and gray_gate then
        local gray_threshold = target.gray_threshold or defaults.gray_threshold or
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

local function scan_global(frame, target, threshold, defaults)
    return scan_one(frame, target, threshold, nil, defaults)
end

local function scan_persistent_roi(frame, target, threshold, roi, defaults)
    return scan_one(frame, target, threshold, roi, defaults)
end

local function scan_batch(frame, group, roi)
    if frame.find_candidates then
        return frame:find_candidates(group.paths, group.threshold, roi) or {}
    end
    if #group.paths > 1 and frame.find_first then
        local match = frame:find_first(group.paths, group.threshold, roi)
        return match and {match} or {}
    end
    local match = scan_one(frame, group.target, group.threshold, roi, group.defaults)
    if not match then return {} end
    match.index = 1
    return {match}
end

local function scan_global_batch(frame, group)
    return scan_batch(frame, group, nil)
end

local function scan_persistent_roi_batch(frame, group)
    return scan_batch(frame, group, group.roi)
end

local function apply_action(target, match, frame, defaults)
    if target.on_match then
        return target.on_match(match, frame, target) ~= false
    end
    local action = target.action or defaults.action
    if action == nil or action == "none" then return false end
    if type(action) == "function" then return action(match, frame, target) ~= false end
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
    else
        return false
    end
    -- Host input calls return only after the input was accepted. This is the
    -- success edge that starts cooldown/wait; a mere template match does not.
    return true
end

local function copy_match(match)
    local copy = {}
    for key, value in pairs(match) do copy[key] = value end
    return copy
end

local function new_engine(config)
    assert(type(config) == "table", "forge.vision expects a configuration table")
    assert(type(config.targets) == "table" and #config.targets > 0,
        "vision.targets must not be empty")
    assert(#config.targets <= 64, "vision.targets may contain at most 64 targets")
    local tracking = type(config.tracking) == "table" and config.tracking or {}
    if config.tracking == true then tracking.enabled = true end
    config.interval_ms = bounded(config.interval_ms, nil, 0, 60000, "interval_ms")
    config.after_match_ms = bounded(config.after_match_ms, 0, 0, 600000, "after_match_ms")
    config.cooldown_ms = bounded(config.cooldown_ms, 0, 0, 600000, "cooldown_ms")
    config.rescan_interval_ms = bounded(config.rescan_interval_ms, 0, 0, 60000,
        "rescan_interval_ms")
    tracking.observe_interval_ms = bounded(tracking.observe_interval_ms, 250, 50, 60000,
        "tracking.observe_interval_ms")
    tracking.stable_hits = bounded(tracking.stable_hits, 3, 1, 32, "tracking.stable_hits")
    tracking.stable_history = bounded(tracking.stable_history, tracking.stable_hits,
        tracking.stable_hits, 32, "tracking.stable_history")
    tracking.position_tolerance_px = bounded(tracking.position_tolerance_px, nil, 1, 4096,
        "tracking.position_tolerance_px")
    tracking.roi_width_multiplier = bounded(tracking.roi_width_multiplier, 2, 1, 16,
        "tracking.roi_width_multiplier")
    tracking.roi_height_multiplier = bounded(tracking.roi_height_multiplier, 3, 1, 16,
        "tracking.roi_height_multiplier")
    tracking.size_change_ratio = tracking.size_change_ratio or 0.15
    tracking.persistence = tracking.persistence ~= false
    if forge._set_rescan_interval_ms then
        forge._set_rescan_interval_ms(config.rescan_interval_ms or 0)
    end

    local states = tracking.persistence and load_persistent_states() or {}
    local engine = {
        config = config,
        tracks = {},
        scans = 0,
        matches = 0,
        full_scans = 0,
        global_scans = 0,
        local_scans = 0,
        roi_scans = 0,
        expanded_scans = 0,
        local_hits = 0,
        fallbacks = 0,
        track_locks = 0,
        roi_locks = 0,
        track_resets = 0,
        cooldown_suppressed = 0,
        rearm_suppressed = 0,
        action_failures = 0,
        below_threshold = 0,
        duplicate_frames_skipped = 0,
        scene_gate_skipped = 0,
        full_backoff_skipped = 0,
        observation_backoff_skipped = 0,
        target_backoff_skipped = 0,
        roi_misses = 0,
        persisted_loads = 0,
        persisted_saves = 0,
        next_scan_ms = 0,
        idle_backoff_ms = 0,
        last_full_scan_ms = -math.huge,
        frame_size = nil,
    }

    for i, target in ipairs(config.targets) do
        assert(type(target) == "table", "each vision target must be a table")
        target.name = target.name or tostring(i)
        target.template = assert(target.template or target.path,
            "vision target requires template")
        assert(type(target.template) == "string" and #target.template <= 1024,
            "vision target template path is invalid")
        local threshold = target.threshold or config.threshold or 0.8
        assert(type(threshold) == "number" and threshold == threshold and
            threshold >= 0 and threshold <= 1, "vision threshold must be between 0 and 1")
        target.rearm = target.rearm or "disappear"
        assert(target.rearm == "disappear" or target.rearm == "timer",
            "vision target rearm must be 'disappear' or 'timer'")
        target.cooldown_ms = bounded(target.cooldown_ms, 0, 0, 600000,
            "target.cooldown_ms")
        target.action_delay_ms = bounded(target.action_delay_ms,
            config.action_delay_ms or 0, 0, 600000, "target.action_delay_ms")
        target.disappear_frames = bounded(target.disappear_frames, 3, 1, 32,
            "target.disappear_frames")
        local identity = target_identity(target)
        engine.tracks[i] = {
            identity = identity,
            persisted_record = states[identity],
            history = {},
            next_observe_ms = 0,
            next_scan_ms = 0,
            pending_match = nil,
            pending_action_at = 0,
            wait_until_ms = 0,
            armed = true,
            absent_count = 0,
            miss_count = 0,
            locked = false,
            stable_count = 0,
            last_scan_mode = "none",
            last_scanned_seq = 0,
            last_decision = "none",
            last_roi = nil,
            last_score = 0,
            last_match = nil,
            last_confidence = 0,
            last_seen_ms = 0,
            persistent_roi = nil,
            next_action_ms = 0,
        }
    end

    local function restore_persisted(track, frame)
        if not tracking.persistence or not track.persisted_record then return false end
        local record = track.persisted_record
        if record.frame_width ~= tonumber(frame.width) or
            record.frame_height ~= tonumber(frame.height) then
            return false
        end
        local roi = clamp_roi(record.roi, frame)
        if not roi then return false end
        track.persistent_roi = roi
        track.locked = true
        track.stable_count = tracking.stable_hits
        track.last_roi = roi
        track.last_decision = "roi_loaded"
        engine.persisted_loads = engine.persisted_loads + 1
        return true
    end

    local function persist_track(track, match, frame)
        if not tracking.persistence or not track.persistent_roi then return end
        local template_width = math.max(1, math.floor(tonumber(match.w) or 1))
        local template_height = math.max(1, math.floor(tonumber(match.h) or 1))
        local record = {
            frame_width = math.max(1, math.floor(tonumber(frame.width) or 1)),
            frame_height = math.max(1, math.floor(tonumber(frame.height) or 1)),
            template_width = template_width,
            template_height = template_height,
            roi = {track.persistent_roi[1], track.persistent_roi[2],
                track.persistent_roi[3], track.persistent_roi[4]},
        }
        states[track.identity] = record
        track.persisted_record = record
        if save_persistent_states(states) then engine.persisted_saves = engine.persisted_saves + 1 end
    end

    function engine:process(frame)
        local now = forge.monotonic_ms()
        local frame_seq = tonumber(frame.frame_seq or 0) or 0
        -- Do not reject duplicate frame_seq or equal scene signatures. Static
        -- screens and repeated decoded frames are valid matching opportunities.
        local cfg, targets = self.config, self.config.targets
        local geometry_changed = self.frame_size == nil or
            self.frame_size[1] ~= frame.width or self.frame_size[2] ~= frame.height
        if geometry_changed then
            self.frame_size = {frame.width, frame.height}
            for _, track in ipairs(self.tracks) do
                reset_track(track)
                track.armed = true
                track.next_action_ms = 0
                restore_persisted(track, frame)
            end
            self.track_resets = self.track_resets + 1
        end

        -- Complete delayed clicks before processing this frame. A delayed
        -- action is not a match cooldown: matches continue during the 500 ms
        -- pre-click delay and refresh the stored center.
        local selected = nil
        local function finish_pending(target, track)
            if not track.pending_match or now < track.pending_action_at or
                not enabled(target, frame) then
                return
            end
            local pending = track.pending_match
            local ok, did_action = pcall(apply_action, target, pending, frame, cfg)
            track.pending_match = nil
            track.pending_action_at = 0
            if not ok then
                self.action_failures = self.action_failures + 1
                track.last_decision = "error"
                forge.log(string.format("目标 %s 动作失败：%s", target.name, tostring(did_action)))
            elseif did_action then
                local cooldown = target.cooldown_ms or cfg.cooldown_ms or 0
                track.next_action_ms = now + cooldown
                track.wait_until_ms = track.next_action_ms
                if target.rearm == "disappear" then track.armed = false end
                track.last_decision = "hit"
                selected = selected or track.last_match or pending
                if cfg.on_match then
                    local callback_ok, callback_error = pcall(cfg.on_match, target,
                        track.last_match or pending, frame)
                    if not callback_ok then
                        forge.log(string.format("目标 %s on_match 回调失败：%s",
                            target.name, tostring(callback_error)))
                    end
                end
            else
                track.last_decision = "matched_no_action"
            end
        end
        for i, target in ipairs(targets) do
            finish_pending(target, self.tracks[i])
        end

        local plans, groups = {}, {}
        local any_global_scan = false
        for i, target in ipairs(targets) do
            local active = enabled(target, frame)
            local track = self.tracks[i]
            local threshold = target.threshold or cfg.threshold or 0.8
            local configured_roi = clamp_roi(target.roi or cfg.roi, frame)
            local tracking_enabled = tracking.enabled ~= false
            local roi = configured_roi or (tracking_enabled and track.persistent_roi or nil)
            local scan_mode = roi and "roi" or "global"
            local waiting_after_action = now < (track.wait_until_ms or 0)
            -- A successful action owns the wait window. No target matching is
            -- performed during that window; disappear rearm resumes probing
            -- after the cooldown and then requires the configured misses.
            local skip = not active or waiting_after_action
            local scan_mode_for_stats = scan_mode
            if waiting_after_action then scan_mode_for_stats = "wait" end
            if not skip then
                if scan_mode == "global" then
                    any_global_scan = true
                    self.global_scans = self.global_scans + 1
                else
                    self.roi_scans = self.roi_scans + 1
                end
            end
            plans[i] = {
                active = active,
                skip = skip,
                threshold = threshold,
                roi = roi,
                mode = target.mode or cfg.mode or "color",
                fast = target.fast == true or (target.fast == nil and cfg.fast == true),
                multi = target.multiscale == true or
                    (target.multiscale == nil and cfg.multiscale == true),
                gray_gate = target.gray_gate == true or
                    (target.gray_gate == nil and cfg.gray_gate == true),
                scan_mode = scan_mode,
                scan_mode_for_stats = scan_mode_for_stats,
            }
            if active and not skip and not plans[i].multi and not plans[i].fast and
                not plans[i].gray_gate and plans[i].mode == "color" then
                local key = tostring(threshold) .. "|" .. roi_key(roi) .. "|" .. scan_mode
                groups[key] = groups[key] or {
                    indexes = {}, paths = {}, threshold = threshold, roi = roi,
                    scan_mode = scan_mode, target = target, defaults = cfg,
                }
                groups[key].indexes[#groups[key].indexes + 1] = i
                groups[key].paths[#groups[key].paths + 1] = target.template
            end
        end

        local found = {}
        for _, group in pairs(groups) do
            local candidates = group.scan_mode == "global" and
                scan_global_batch(frame, group) or scan_persistent_roi_batch(frame, group)
            for _, match in ipairs(candidates or {}) do
                local local_index = match.index or 0
                local target_index = group.indexes[local_index]
                if target_index then found[target_index] = match end
            end
        end
        for i, plan in ipairs(plans) do
            if plan.active and not plan.skip and not found[i] and
                (plan.multi or plan.fast or plan.gray_gate or plan.mode ~= "color") then
                local match
                if plan.scan_mode == "global" then
                    match = scan_global(frame, targets[i], plan.threshold, cfg)
                else
                    match = scan_persistent_roi(frame, targets[i], plan.threshold, plan.roi, cfg)
                end
                if match then found[i] = match end
            end
        end

        if any_global_scan then
            self.last_full_scan_ms = now
            self.full_scans = self.full_scans + 1
        end
        self.scans = self.scans + 1
        local rearmed_any = false
        for i, target in ipairs(targets) do
            local track, match = self.tracks[i], found[i]
            local plan = plans[i]
            if not plan.active or plan.skip then
                goto continue
            end
            track.last_scan_mode = plan.scan_mode_for_stats
            track.last_scanned_seq = frame_seq
            track.last_roi = plan.roi
            track.last_score = match and (match.confidence or 0) or 0
            if match then
                match.index, match.name = i, target.name
                self.matches = self.matches + 1
                local was_locked = track.locked
                local locked, invalidated = update_track(track, match, frame, now, tracking)
                if invalidated then
                    states[track.identity] = nil
                    track.persisted_record = nil
                    if save_persistent_states(states) then
                        self.persisted_saves = self.persisted_saves + 1
                    end
                end
                if not was_locked and locked then
                    self.track_locks = self.track_locks + 1
                    self.roi_locks = self.roi_locks + 1
                    persist_track(track, match, frame)
                end
                if not track.armed then
                    self.rearm_suppressed = self.rearm_suppressed + 1
                    track.last_decision = "rearm"
                elseif now < track.wait_until_ms then
                    self.cooldown_suppressed = self.cooldown_suppressed + 1
                    track.last_decision = "cooldown"
                elseif track.pending_match then
                    -- Keep the delayed click aligned with the newest match
                    -- while the 500ms confirmation window is open.
                    track.pending_match = copy_match(match)
                else
                    track.pending_match = copy_match(match)
                    local delay = target.action_delay_ms or cfg.action_delay_ms or 0
                    track.pending_action_at = now + delay
                    track.last_decision = delay > 0 and "action_delay" or "action_pending"
                end
            else
                self.below_threshold = self.below_threshold + 1
                if not track.armed then self.rearm_suppressed = self.rearm_suppressed + 1 end
                track.last_decision = track.persistent_roi and "roi_miss" or
                    (track.armed and "below_threshold" or "rearm_wait")
                track.miss_count = track.miss_count + 1
                if track.persistent_roi then self.roi_misses = self.roi_misses + 1 end
                if not track.persistent_roi then
                    track.history = {}
                    track.stable_count = 0
                    track.locked = false
                end
                if not track.armed then
                    track.absent_count = track.absent_count + 1
                    if track.absent_count >= (target.disappear_frames or 3) then
                        track.armed = true
                        track.absent_count = 0
                        rearmed_any = true
                    end
                end
            end
            if target.rearm == "disappear" and not track.armed then
                track.next_observe_ms = track.wait_until_ms
                track.next_scan_ms = track.wait_until_ms
            elseif now < track.wait_until_ms then
                track.next_observe_ms = track.wait_until_ms
                track.next_scan_ms = track.wait_until_ms
            else
                track.next_observe_ms = 0
                track.next_scan_ms = now
            end
            ::continue::
        end

        -- A zero-delay action is queued by the match phase above. Complete it
        -- now so the declarative API remains synchronous for that mode.
        for i, target in ipairs(targets) do
            finish_pending(target, self.tracks[i])
        end

        local recommended_interval = forge.recommended_interval_ms and
            forge.recommended_interval_ms() or 16
        local interval = cfg.interval_ms or recommended_interval
        if interval <= 0 then interval = 16 end
        if selected and cfg.after_match_ms and cfg.after_match_ms > interval then
            interval = cfg.after_match_ms
        end
        self.idle_backoff_ms = interval
        self.next_scan_ms = now + interval
        if rearmed_any then self.next_scan_ms = now end
        if not selected and cfg.on_idle then
            local idle_ok, idle_error = pcall(cfg.on_idle, frame, self)
            if not idle_ok then forge.log("on_idle 回调失败：" .. tostring(idle_error)) end
        end
        return selected
    end

    function engine:stats()
        return {
            scans = self.scans,
            matches = self.matches,
            full_scans = self.full_scans,
            global_scans = self.global_scans,
            local_scans = self.local_scans,
            roi_scans = self.roi_scans,
            expanded_scans = self.expanded_scans,
            local_hits = self.local_hits,
            fallbacks = self.fallbacks,
            track_locks = self.track_locks,
            roi_locks = self.roi_locks,
            track_resets = self.track_resets,
            cooldown_suppressed = self.cooldown_suppressed,
            rearm_suppressed = self.rearm_suppressed,
            action_failures = self.action_failures,
            below_threshold = self.below_threshold,
            duplicate_frames_skipped = self.duplicate_frames_skipped,
            scene_gate_skipped = self.scene_gate_skipped,
            full_backoff_skipped = self.full_backoff_skipped,
            observation_backoff_skipped = self.observation_backoff_skipped,
            target_backoff_skipped = self.target_backoff_skipped,
            roi_misses = self.roi_misses,
            persisted_loads = self.persisted_loads,
            persisted_saves = self.persisted_saves,
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
                        persistent_roi = track.persistent_roi,
                        last_scan_mode = track.last_scan_mode,
                        last_scanned_seq = track.last_scanned_seq,
                        last_decision = track.last_decision,
                        last_roi = track.last_roi,
                        last_score = track.last_score,
                        pending_action_at = track.pending_action_at,
                        wait_until_ms = track.wait_until_ms,
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
    if #engines >= MAX_ENGINES then
        error("vision may contain at most " .. MAX_ENGINES .. " compiled engines")
    end
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
