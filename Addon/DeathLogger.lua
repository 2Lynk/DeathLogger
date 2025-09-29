--[[
Death Logger (Retail + Classic compatible) — Money formatting added
- Records death info (killer from combat log, zone/subzone/coords, bags, equipped).
- Optional Screenshot() after death (toggle & delay).
- Classic-safe (no GetSpecialization on Classic; bag API fallbacks).
- NEW: Stores/prints money in gold/silver/copper.

SavedVariables:
- DeathLoggerDB.deaths (ring buffer)
- DeathLoggerDB.screenshotOn (default true)
- DeathLoggerDB.screenshotDelay (default 0.5)
- DeathLoggerDB.maxEntries (default 200)
--]]

local frame = CreateFrame("Frame")

-- --------------------- SavedVariables bootstrap ---------------------
local function EnsureDB()
    if not DeathLoggerDB then DeathLoggerDB = {} end
    if not DeathLoggerDB.deaths then DeathLoggerDB.deaths = {} end
    if DeathLoggerDB.maxEntries == nil then DeathLoggerDB.maxEntries = 200 end
    if DeathLoggerDB.screenshotOn == nil then DeathLoggerDB.screenshotOn = true end
    if DeathLoggerDB.screenshotDelay == nil then DeathLoggerDB.screenshotDelay = 0.5 end
end

-- --------------------- Utilities ---------------------
local function Truncate(num, places)
    if not num then return nil end
    local mult = 10 ^ (places or 2)
    return math.floor(num * mult + 0.5) / mult
end

local function PrettyTime(t)
    if not t then return "unknown" end
    return date("%Y-%m-%d %H:%M:%S", t) or tostring(t)
end

local function GetPlayerNameRealm()
    local name, realm = UnitName("player")
    realm = realm or GetRealmName() or ""
    return name, realm
end

-- Retail-only; Classic has no specializations
local function GetSpecID()
    if type(GetSpecialization) == "function" and type(GetSpecializationInfo) == "function" then
        local idx = GetSpecialization()
        if idx then
            local id = GetSpecializationInfo(idx)
            return id
        end
    end
    return nil
end

-- Money helpers
local function MoneyBreakdown(copper)
    copper = tonumber(copper) or 0
    local gold = math.floor(copper / 10000)
    local silver = math.floor((copper % 10000) / 100)
    local cop = copper % 100
    return gold, silver, cop
end

local function MoneyString(copper)
    local g, s, c = MoneyBreakdown(copper)
    return string.format("%dg %ds %dc", g, s, c)
end

-- --------------------- Location helpers ---------------------
local function GetLocation()
    local mapID = C_Map.GetBestMapForUnit and C_Map.GetBestMapForUnit("player") or nil
    local zoneName, subzoneName, x, y

    if mapID and C_Map.GetMapInfo and C_Map.GetPlayerMapPosition then
        local info = C_Map.GetMapInfo(mapID)
        if info then zoneName = info.name end
        local pos = C_Map.GetPlayerMapPosition(mapID, "player")
        if pos then
            x = Truncate(pos.x * 100, 2)
            y = Truncate(pos.y * 100, 2)
        end
    end

    subzoneName = GetMinimapZoneText() or GetSubZoneText() or ""

    if (not x or not y) and not zoneName then
        local _, _, _, mapIDAlt = UnitPosition("player")
        if mapIDAlt and C_Map.GetMapInfo then
            local infoAlt = C_Map.GetMapInfo(mapIDAlt)
            if infoAlt then zoneName = infoAlt.name end
        end
    end

    return {
        mapID  = mapID,
        zone   = zoneName or GetZoneText() or "",
        subzone = subzoneName or "",
        x = x, -- percent coords (0-100)
        y = y,
    }
end

-- --------------------- Inventory snapshot (Retail + Classic) ---------------------
local BAG_IDS = {0, 1, 2, 3, 4} -- backpack + 4 bags
local EQUIP_SLOTS = {1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17}

local function ContainerNumSlots(bagID)
    if C_Container and C_Container.GetContainerNumSlots then
        return C_Container.GetContainerNumSlots(bagID)
    elseif type(GetContainerNumSlots) == "function" then
        return GetContainerNumSlots(bagID)
    else
        return 0
    end
end

-- Returns: hyperlink, itemID, stackCount, icon, quality (some may be nil on Classic)
local function ContainerItemQuick(bagID, slot)
    if C_Container and C_Container.GetContainerItemInfo then
        local info = C_Container.GetContainerItemInfo(bagID, slot)
        if info then
            local itemID = info.itemID
            return info.hyperlink, itemID, info.stackCount, info.iconFileID, info.quality
        end
    elseif type(GetContainerItemInfo) == "function" then
        -- Classic pre-Dragonflight signature
        local texture, count, _, quality, _, _, link = GetContainerItemInfo(bagID, slot)
        local itemID
        if link then itemID = tonumber(string.match(link, "item:(%d+)")) end
        return link, itemID, count, texture, quality
    end
    return nil, nil, nil, nil, nil
end

local function SnapshotBags()
    local bags = {}
    for _, bagID in ipairs(BAG_IDS) do
        local numSlots = ContainerNumSlots(bagID)
        local bag = { bagID = bagID, slots = {} }
        for slot = 1, numSlots do
            local link, itemID, count, icon, quality = ContainerItemQuick(bagID, slot)
            if link or itemID then
                table.insert(bag.slots, {
                    slot = slot,
                    itemID = itemID,
                    stackCount = count,
                    hyperlink = link,
                    icon = icon,
                    quality = quality,
                })
            end
        end
        table.insert(bags, bag)
    end
    return bags
end

local function SnapshotEquipped()
    local equipped = {}
    for _, slot in ipairs(EQUIP_SLOTS) do
        local link = GetInventoryItemLink("player", slot)
        if link then
            table.insert(equipped, { slot = slot, hyperlink = link })
        end
    end
    return equipped
end

-- --------------------- Combat log tracking ---------------------
local recentDamage = {}
local recentWindowSeconds = 6.0
local playerGUID

local DAMAGE_EVENTS = {
    SWING_DAMAGE = true,
    RANGE_DAMAGE = true,
    SPELL_DAMAGE = true,
    SPELL_PERIODIC_DAMAGE = true,
    ENVIRONMENTAL_DAMAGE = true,
}

local function PruneRecent(now)
    local cutoff = now - recentWindowSeconds
    local i = 1
    while i <= #recentDamage do
        if recentDamage[i].timestamp < cutoff then
            table.remove(recentDamage, i)
        else
            i = i + 1
        end
    end
end

local function OnCombatLogEvent()
    local now = GetTime()

    local timestamp, subevent,
          _, sourceGUID, sourceName, _, _,
          destGUID, destName, _, _,
          arg12, arg13, arg14, arg15, arg16
        = CombatLogGetCurrentEventInfo()

    if not playerGUID then playerGUID = UnitGUID("player") end
    if destGUID ~= playerGUID then return end
    if not DAMAGE_EVENTS[subevent] then return end

    local e = {
        timestamp = timestamp or now,
        subevent = subevent,
        sourceGUID = sourceGUID,
        sourceName = sourceName,
    }

    if subevent == "SWING_DAMAGE" then
        e.amount, e.overkill = arg12, arg13
        e.spellName = "Melee"
    elseif subevent == "RANGE_DAMAGE" or subevent == "SPELL_DAMAGE" or subevent == "SPELL_PERIODIC_DAMAGE" then
        local spellID, spellName, _school, amount, overkill = arg12, arg13, arg14, arg15, arg16
        e.spellID, e.spellName, e.amount, e.overkill = spellID, spellName, amount, overkill
    elseif subevent == "ENVIRONMENTAL_DAMAGE" then
        local environmentalType, amount, overkill = arg12, arg13, arg14
        e.environmentalType, e.amount, e.overkill = environmentalType, amount, overkill
        e.sourceName = environmentalType and ("Environment: " .. environmentalType) or "Environment"
    end

    table.insert(recentDamage, e)
    PruneRecent(now)
end

local function DetermineKiller()
    local lethal
    for i = #recentDamage, 1, -1 do
        local e = recentDamage[i]
        if type(e.overkill) == "number" and e.overkill > 0 then
            lethal = e
            break
        end
    end
    local e = lethal or recentDamage[#recentDamage]
    if not e then
        return { sourceName = "Unknown", detail = "No recent damage events", subevent = nil }
    end

    local spell = e.spellName or e.environmentalType or "Unknown"
    local src = e.sourceName or "Unknown"
    local detail
    if e.subevent == "ENVIRONMENTAL_DAMAGE" and e.environmentalType then
        detail = ("Environmental (%s)"):format(e.environmentalType)
    elseif e.subevent == "SWING_DAMAGE" then
        detail = "Melee"
    else
        detail = spell
    end

    return {
        sourceName = src,
        subevent = e.subevent,
        spellName = spell,
        amount = e.amount,
        overkill = e.overkill,
        timestamp = e.timestamp,
        detail = detail,
    }
end

-- --------------------- Recording a death ---------------------
local function RecordDeath()
    EnsureDB()

    local name, realm = GetPlayerNameRealm()
    local loc = GetLocation()
    local killer = DetermineKiller()
    local bags = SnapshotBags()
    local equipped = SnapshotEquipped()

    local instName, _, diffID, diffName, _, _, _, instID = GetInstanceInfo()
    local moneyCopper = GetMoney()
    local g, s, c = MoneyBreakdown(moneyCopper)

    local entry = {
        at = time(),
        player = name,
        realm = realm,
        location = loc,
        killer = killer,
        bags = bags,
        equipped = equipped,
        moneyCopper = moneyCopper,     -- raw copper
        moneyGold = g,                 -- breakdown
        moneySilver = s,
        moneyCopperOnly = c,
        level = UnitLevel("player"),
        class = select(2, UnitClass("player")),
        specID = GetSpecID(), -- nil on Classic, numeric on Retail
        mapDifficultyID = diffID,
        instanceID = instID,
        instanceName = instName,
        instanceDifficulty = diffName,
    }

    table.insert(DeathLoggerDB.deaths, entry)

    local maxEntries = DeathLoggerDB.maxEntries or 200
    while #DeathLoggerDB.deaths > maxEntries do
        table.remove(DeathLoggerDB.deaths, 1)
    end

    local zoneStr = (entry.location.zone or "Unknown")
    local subStr = entry.location.subzone and entry.location.subzone ~= "" and (" - " .. entry.location.subzone) or ""
    local coordStr = (entry.location.x and entry.location.y) and (" (%.2f, %.2f)"):format(entry.location.x, entry.location.y) or ""
    print(("|cffff5555DeathLogger:|r Recorded death in %s%s%s. Killer: %s (%s). Money: %s")
        :format(zoneStr, subStr, coordStr, killer.sourceName or "Unknown", killer.detail or "", MoneyString(moneyCopper)))

    -- Optional screenshot
    if DeathLoggerDB.screenshotOn and type(Screenshot) == "function" then
        local delay = tonumber(DeathLoggerDB.screenshotDelay) or 0.5
        if delay > 0 then
            C_Timer.After(delay, function()
                Screenshot()
                print("|cffff5555DeathLogger:|r Screenshot captured.")
            end)
        else
            Screenshot()
            print("|cffff5555DeathLogger:|r Screenshot captured.")
        end
    end
end

-- --------------------- Slash commands ---------------------
SLASH_DEATHLOGGER1 = "/deathlog"
SlashCmdList["DEATHLOGGER"] = function(msg)
    EnsureDB()
    msg = (msg or ""):lower()

    if msg == "wipe" or msg == "clear" then
        DeathLoggerDB.deaths = {}
        print("|cffff5555DeathLogger:|r cleared all records.")
        return
    end

    if msg == "last" or msg == "" then
        local n = #DeathLoggerDB.deaths
        if n == 0 then
            print("|cffff5555DeathLogger:|r no deaths recorded yet.")
            return
        end
        local e = DeathLoggerDB.deaths[n]
        local loc = e.location or {}
        local killer = e.killer or {}
        print("|cffff5555DeathLogger last death|r")
        print(("  When: %s"):format(PrettyTime(e.at)))
        print(("  Where: %s%s %s"):format(
            loc.zone or "Unknown",
            (loc.subzone and loc.subzone ~= "" and (" - " .. (loc.subzone or "")) or ""),
            (loc.x and loc.y) and ("(%.2f, %.2f)"):format(loc.x, loc.y) or ""))
        print(("  Killer: %s (%s)"):format(killer.sourceName or "Unknown", killer.detail or ""))
        if e.moneyCopper ~= nil then
            print(("  Money: %s"):format(MoneyString(e.moneyCopper)))
        end
        print(("  Items: bags=%d, equipped=%d"):format(
            (e.bags and #e.bags or 0),
            (e.equipped and #e.equipped or 0)))
        return
    end

    if msg == "count" or msg == "list" then
        print(("|cffff5555DeathLogger:|r %d death records."):format(#DeathLoggerDB.deaths))
        return
    end

    if msg == "export" then
        local n = #DeathLoggerDB.deaths
        if n == 0 then
            print("|cffff5555DeathLogger:|r nothing to export.")
            return
        end
        print("|cffff5555DeathLogger export (summary)|r")
        for i, e in ipairs(DeathLoggerDB.deaths) do
            local loc = e.location or {}
            local killer = e.killer or {}
            local moneyStr = e.moneyCopper and MoneyString(e.moneyCopper) or "unknown"
            print(("[%d] {time:\"%s\", zone:\"%s\", subzone:\"%s\", x:%.2f, y:%.2f, killer:\"%s\", detail:\"%s\", money:\"%s\"}"):format(
                i, PrettyTime(e.at),
                loc.zone or "", loc.subzone or "",
                loc.x or -1, loc.y or -1,
                killer.sourceName or "", killer.detail or "",
                moneyStr))
        end
        print("|cffff5555DeathLogger:|r For full data (bags/equipped), use the SavedVariables file.")
        return
    end

    if msg == "help" then
        print("|cffff5555DeathLogger commands:|r")
        print("  /deathlog                 - show last death")
        print("  /deathlog last            - show last death")
        print("  /deathlog count           - count records")
        print("  /deathlog export          - print a summary")
        print("  /deathlog wipe            - clear all records")
        print("  /deathlog screenshot on   - enable auto screenshot")
        print("  /deathlog screenshot off  - disable auto screenshot")
        print("  /deathlog screenshot delay <seconds> - set capture delay")
        return
    end

    -- screenshot toggles
    if msg:match("^screenshot%s+on$") then
        DeathLoggerDB.screenshotOn = true
        print("|cffff5555DeathLogger:|r Auto-screenshot ON.")
        return
    elseif msg:match("^screenshot%s+off$") then
        DeathLoggerDB.screenshotOn = false
        print("|cffff5555DeathLogger:|r Auto-screenshot OFF.")
        return
    end

    local delayVal = msg:match("^screenshot%s+delay%s+([%d%.]+)$")
    if delayVal then
        local val = tonumber(delayVal)
        if val then
            DeathLoggerDB.screenshotDelay = val
            print(("|cffff5555DeathLogger:|r Screenshot delay set to %.2fs."):format(val))
        else
            print("|cffff5555DeathLogger:|r Invalid delay value.")
        end
        return
    end

    print("|cffff5555DeathLogger:|r unknown command. Use '/deathlog help'.")
end

-- --------------------- Event wiring ---------------------
frame:RegisterEvent("PLAYER_LOGIN")
frame:RegisterEvent("PLAYER_DEAD")
frame:RegisterEvent("COMBAT_LOG_EVENT_UNFILTERED")

frame:SetScript("OnEvent", function(_, event)
    if event == "PLAYER_LOGIN" then
        EnsureDB()
        playerGUID = UnitGUID("player")
        print("|cffff5555DeathLogger loaded.|r Use /deathlog help for commands.")
        return
    end
    if event == "COMBAT_LOG_EVENT_UNFILTERED" then
        OnCombatLogEvent()
        return
    end
    if event == "PLAYER_DEAD" then
        RecordDeath()
        recentDamage = {}
        return
    end
end)
