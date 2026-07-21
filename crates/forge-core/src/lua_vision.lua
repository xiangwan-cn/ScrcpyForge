-- ScrcpyForge's declarative vision layer. This is deliberately implemented in
-- Lua: applications describe policy here while Rust only provides portable,
-- accelerated frame and input primitives.
local engines = {}

local function same_roi(a, b)
    if a == b then return true end
    if type(a) ~= "table" or type(b) ~= "table" then return false end
    return a[1] == b[1] and a[2] == b[2] and a[3] == b[3] and a[4] == b[4]
end

local function enabled(target, frame)
    if target.enabled == nil then return true end
    if type(target.enabled) == "function" then return target.enabled(frame, target) end
    return target.enabled == true
end

local function apply_action(target, match, frame, defaults)
    if target.on_match then
        return target.on_match(match, frame, target)
    end
    local action = target.action or defaults.action
    if action == nil or action == "none" then return end
    if type(action) == "function" then return action(match, frame, target) end
    if action == "tap" then action = { type = "tap" } end
    if type(action) ~= "table" then error("vision action must be a table, function, 'tap', or 'none'") end

    local kind = action.type or "tap"
    if kind == "tap" then
        forge.tap(
            match.x + (action.offset_x or 0),
            match.y + (action.offset_y or 0),
            action.radius or defaults.radius or 0
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
    assert(type(config.targets) == "table" and #config.targets > 0, "vision.targets must not be empty")
    local engine = { config = config, next_scan_ms = 0, target_ready_ms = {}, scans = 0, matches = 0, tracking_match = nil, tracking_misses = 0, last_full_scan_ms = 0 }

    for i, target in ipairs(config.targets) do
        assert(type(target) == "table", "each vision target must be a table")
        target.name = target.name or tostring(i)
        target.template = assert(target.template or target.path, "vision target requires template")
    end

    function engine:process(frame)
        local now = forge.monotonic_ms()
        if now < self.next_scan_ms then return nil end
        local cfg, targets = self.config, self.config.targets
        local candidates, candidate_indexes = {}, {}
        local common_threshold, common_roi = cfg.threshold or 0.8, cfg.roi
        local tracking = cfg.tracking
        if common_roi == nil and tracking and tracking.enabled ~= false and self.tracking_match then
            local full_interval = tracking.full_scan_interval_ms or 1000
            if now - self.last_full_scan_ms < full_interval then
                local m, margin = self.tracking_match, tracking.margin or 80
                common_roi = {
                    math.max(0, m.x - math.floor(m.w / 2) - margin),
                    math.max(0, m.y - math.floor(m.h / 2) - margin),
                    math.min(frame.width, m.x + math.ceil(m.w / 2) + margin),
                    math.min(frame.height, m.y + math.ceil(m.h / 2) + margin),
                }
            else
                self.last_full_scan_ms = now
            end
        end
        local can_batch = cfg.multiscale ~= true and cfg.fast ~= true

        for i, target in ipairs(targets) do
            local is_enabled = enabled(target, frame)
            if target.threshold ~= nil and target.threshold ~= common_threshold then can_batch = false end
            if target.roi ~= nil and not same_roi(target.roi, common_roi) then can_batch = false end
            if target.multiscale == true then can_batch = false end
            if target.fast == true then can_batch = false end
            if now < (self.target_ready_ms[i] or 0) or not is_enabled then can_batch = false end
            if now >= (self.target_ready_ms[i] or 0) and is_enabled then
                candidates[#candidates + 1] = target.template
                candidate_indexes[#candidate_indexes + 1] = i
            end
        end

        self.scans = self.scans + 1
        local match, target_index
        if can_batch and #candidates == #targets then
            match = frame:find_first(candidates, common_threshold, common_roi)
            if match then target_index = match.index end
        else
            for n, path in ipairs(candidates) do
                local target = targets[candidate_indexes[n]]
                local threshold, roi = target.threshold or common_threshold, target.roi or common_roi
                local mode = target.mode or cfg.mode or "color"
                if target.multiscale == true or (target.multiscale == nil and cfg.multiscale == true) then
                    match = frame:find_multiscale(path, threshold, roi)
                elseif mode == "gray" and (target.fast == true or (target.fast == nil and cfg.fast == true)) then
                    match = frame:find_fast_gray(path, threshold, roi)
                elseif mode == "gray" then
                    match = frame:find_gray(path, threshold, roi)
                elseif target.fast == true or (target.fast == nil and cfg.fast == true) then
                    match = frame:find_fast(path, threshold, roi)
                else
                    match = frame:find(path, threshold, roi)
                end
                if match then target_index = candidate_indexes[n]; break end
            end
        end

        if match then
            local target = targets[target_index]
            match.index, match.name = target_index, target.name
            self.matches = self.matches + 1
            self.tracking_match, self.tracking_misses = match, 0
            apply_action(target, match, frame, cfg)
            local cooldown = target.cooldown_ms or cfg.cooldown_ms or 0
            self.target_ready_ms[target_index] = forge.monotonic_ms() + cooldown
            self.next_scan_ms = forge.monotonic_ms() + (cfg.after_match_ms or 0)
            if cfg.on_match then cfg.on_match(target, match, frame) end
            return match
        end

        if tracking and self.tracking_match then
            self.tracking_misses = self.tracking_misses + 1
            if self.tracking_misses >= (tracking.miss_before_reset or 2) then self.tracking_match = nil end
        end
        self.next_scan_ms = now + (cfg.interval_ms or forge.recommended_interval_ms())
        if cfg.on_idle then cfg.on_idle(frame, self) end
        return nil
    end

    function engine:stats()
        return { scans = self.scans, matches = self.matches, next_scan_ms = self.next_scan_ms }
    end
    return engine
end

local function register(config)
    local engine = new_engine(config)
    engines[#engines + 1] = engine
    -- A low-level script may replace on_frame after registration. Otherwise all
    -- declared engines are dispatched in registration order.
    _G.on_frame = function(frame)
        for _, item in ipairs(engines) do item:process(frame) end
    end
    return engine
end

-- `compile` makes the intent explicit for reusable libraries. Templates and
-- native workspaces are cached by Rust/OpenCV; the returned plan only carries policy.
local vision_api = { compile = register }
setmetatable(vision_api, { __call = function(_, config) return register(config) end })
forge.vision = vision_api
