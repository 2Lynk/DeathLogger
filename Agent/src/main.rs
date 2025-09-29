use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use dialoguer::{Confirm, Input, Select};
use dirs::{data_dir, home_dir};
use glob::glob;
use mlua::{Lua, Value as LuaValue};
use notify::{Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use reqwest::multipart;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

// ---------- Configuration ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    /// Full path to the WoW root folder; e.g.
    ///   C:\Program Files (x86)\World of Warcraft
    wow_root: String,
    /// Which branch inside WoW to use: one of "_retail_", "_classic_", "_classic_era_", "_classic_ptr_"
    wow_branch: String,

    /// Server endpoint to upload to (e.g., https://example.com/api/death)
    api_url: String,
    /// Optional API token (sent as header "Authorization: Bearer <token>" if not empty)
    api_token: String,

    /// Whether agent starts with Windows
    start_with_windows: bool,

    /// Seconds window to pair screenshots with deaths
    pair_window_secs: i64,

    /// Whether to auto-update addon files from GitHub at launch
    update_addon_on_start: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wow_root: String::new(),
            wow_branch: "_retail_".into(),
            api_url: "https://your-server.example/upload".into(),
            api_token: String::new(),
            start_with_windows: false,
            pair_window_secs: 120,
            update_addon_on_start: true,
        }
    }
}

fn config_dir() -> Result<PathBuf> {
    let d = data_dir()
        .or_else(|| home_dir().map(|h| h.join("AppData/Roaming")))
        .ok_or_else(|| anyhow!("Cannot determine writable config directory"))?;
    Ok(d.join("DeathLoggerAgent"))
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

fn state_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("state.json"))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// Last uploaded death timestamp (the "at" field) per account/realm/player
    last_uploaded: BTreeMap<String, i64>,
    /// Queue of screenshots we saw but didn't match yet
    pending_screens: VecDeque<PendingShot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingShot {
    path: String,
    ts_epoch: i64,
}

// ---------- WoW layout helpers ----------

#[derive(Debug, Clone)]
struct WowPaths {
    root: PathBuf,     // e.g. C:\Program Files (x86)\World of Warcraft
    branch: String,    // _retail_ / _classic_ / _classic_era_ / _classic_ptr_
}

impl WowPaths {
    fn branch_root(&self) -> PathBuf {
        self.root.join(&self.branch)
    }
    fn addons_dir(&self) -> PathBuf {
        self.branch_root().join("Interface").join("AddOns")
    }
    fn screenshots_dir(&self) -> PathBuf {
        self.branch_root().join("Screenshots")
    }
    fn wtf_savedvariables_glob(&self) -> String {
        // WTF/Account/<ACCOUNT>[/<ServerName>/<CharName>]/SavedVariables/DeathLogger.lua
        self.branch_root()
            .join("WTF")
            .join("Account")
            .join("*")
            .join("SavedVariables")
            .join("DeathLogger.lua")
            .to_string_lossy()
            .to_string()
    }
}

// ---------- Startup registration (Windows) ----------

fn set_startup(enable: bool) -> Result<()> {
    let exe = std::env::current_exe()?.to_string_lossy().to_string();
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")?;
    if enable {
        key.set_value("DeathLoggerAgent", &exe)?;
    } else {
        let _ = key.delete_value("DeathLoggerAgent");
    }
    Ok(())
}

// ---------- Installer / updater ----------

const RAW_TOC: &str = "https://raw.githubusercontent.com/2Lynk/DeathLogger/main/Addon/DeathLogger.toc";
const RAW_LUA: &str = "https://raw.githubusercontent.com/2Lynk/DeathLogger/main/Addon/DeathLogger.lua";

async fn download_to(url: &str, dest: &Path) -> Result<()> {
    let bytes = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {}", url))?
        .error_for_status()?
        .bytes()
        .await?;
    if let Some(pdir) = dest.parent() {
        fs::create_dir_all(pdir)?;
    }
    fs::write(dest, &bytes)?;
    Ok(())
}

async fn install_or_update_addon(paths: &WowPaths) -> Result<()> {
    let addon_dir = paths.addons_dir().join("DeathLogger");
    fs::create_dir_all(&addon_dir)?;
    download_to(RAW_TOC, &addon_dir.join("DeathLogger.toc")).await?;
    download_to(RAW_LUA, &addon_dir.join("DeathLogger.lua")).await?;
    println!("[install] Updated addon in {}", addon_dir.display());
    Ok(())
}

// ---------- First-run setup ----------

fn try_detect_wow_root_candidates() -> Vec<PathBuf> {
    let mut cands = vec![];
    // Common installs
    let defaults = [
        r"C:\Program Files (x86)\World of Warcraft",
        r"C:\Program Files\World of Warcraft",
        r"D:\World of Warcraft",
        r"E:\World of Warcraft",
    ];
    for d in defaults {
        let p = PathBuf::from(d);
        if p.exists() {
            cands.push(p);
        }
    }
    // Look for folders containing Interface\AddOns under any *_retail_ or *_classic_* branch
    let drives = ['C', 'D', 'E', 'F'];
    for drive in drives {
        let pattern = format!(r"{drive}:\**\World of Warcraft");
        for entry in glob(&pattern).unwrap_or_default() {
            if let Ok(p) = entry {
                if p.exists() && p.is_dir() {
                    cands.push(p);
                }
            }
        }
    }
    cands.sort();
    cands.dedup();
    cands
}

fn choose_branch(root: &Path) -> Result<String> {
    let branches = ["_retail_", "_classic_", "_classic_era_", "_classic_ptr_"];
    let mut present: Vec<String> = branches
        .iter()
        .filter(|b| root.join(b).exists())
        .map(|s| s.to_string())
        .collect();

    if present.is_empty() {
        // Still allow manual selection
        present = branches.iter().map(|s| s.to_string()).collect();
    }

    let idx = Select::new()
        .with_prompt("Select WoW branch to monitor")
        .items(&present)
        .default(0)
        .interact()
        .unwrap_or(0);

    Ok(present[idx].clone())
}

async fn first_run_wizard() -> Result<Config> {
    println!("Welcome to DeathLogger Agent!");

    let detect = Confirm::new()
        .with_prompt("Try to detect your World of Warcraft folder automatically?")
        .default(true)
        .interact()
        .unwrap_or(true);

    let wow_root = if detect {
        let cands = try_detect_wow_root_candidates();
        if !cands.is_empty() {
            let items: Vec<String> = cands.iter().map(|p| p.display().to_string()).collect();
            let idx = Select::new()
                .with_prompt("Select your WoW root folder")
                .items(&items)
                .default(0)
                .interact()
                .unwrap_or(0);
            cands[idx].clone()
        } else {
            println!("No installs detected.");
            PathBuf::from(
                Input::<String>::new()
                    .with_prompt("Enter your WoW root folder (contains _retail_/_classic_)")
                    .interact_text()?,
            )
        }
    } else {
        PathBuf::from(
            Input::<String>::new()
                .with_prompt("Enter your WoW root folder (contains _retail_/_classic_)")
                .interact_text()?,
        )
    };

    if !wow_root.exists() {
        return Err(anyhow!(
            "Path does not exist: {}",
            wow_root.display()
        ));
    }

    let branch = choose_branch(&wow_root)?;

    let api_url: String = Input::new()
        .with_prompt("Enter your server upload URL")
        .default("https://your-server.example/upload".into())
        .interact_text()?;

    let api_token: String = Input::new()
        .with_prompt("Enter API token (optional, blank to skip)")
        .allow_empty(true)
        .interact_text()?;

    let start_with_windows = Confirm::new()
        .with_prompt("Start this agent with Windows?")
        .default(false)
        .interact()
        .unwrap_or(false);

    let cfg = Config {
        wow_root: wow_root.to_string_lossy().to_string(),
        wow_branch: branch,
        api_url,
        api_token,
        start_with_windows,
        pair_window_secs: 120,
        update_addon_on_start: true,
    };

    fs::create_dir_all(config_dir()?)?;
    fs::write(config_path()?, toml::to_string_pretty(&cfg)?)?;

    if cfg.start_with_windows {
        set_startup(true)?;
    }

    Ok(cfg)
}

// ---------- SV & screenshot watching ----------

fn load_state() -> Result<State> {
    let p = state_path()?;
    if p.exists() {
        let s = fs::read_to_string(&p)?;
        Ok(serde_json::from_str(&s)?)
    } else {
        Ok(State::default())
    }
}
fn save_state(state: &State) -> Result<()> {
    fs::create_dir_all(config_dir()?)?;
    fs::write(state_path()?, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

fn newest_mtime(path: &Path) -> Option<SystemTime> {
    path.metadata().and_then(|m| m.modified()).ok()
}

#[derive(Debug, Clone, Serialize)]
struct DeathPayload {
    at: i64,
    player: String,
    realm: String,
    class: Option<String>,
    level: Option<i64>,
    location: serde_json::Value,
    killer: serde_json::Value,
    bags: serde_json::Value,
    equipped: serde_json::Value,
    instance: serde_json::Value,
    moneyCopper: Option<i64>,
    moneyGold: Option<i64>,
    moneySilver: Option<i64>,
    moneyCopperOnly: Option<i64>,
}

fn to_key(player: &str, realm: &str) -> String {
    format!("{}@{}", player, realm)
}

// Evaluate SavedVariables file with Lua and extract the last entry
fn parse_latest_death_from_sv(sv_path: &Path) -> Result<Option<DeathPayload>> {
    let content = fs::read_to_string(sv_path)?;
    // Execute the SV Lua in a clean Lua state
    let lua = Lua::new();

    // The SV file assigns globals like: DeathLoggerDB = { ... }
    lua.load(&content).exec().context("executing SV lua")?;

    // Fetch DeathLoggerDB
    let globals = lua.globals();
    let db_val = globals.get::<_, LuaValue>("DeathLoggerDB")?;
    let db_tbl = match db_val {
        LuaValue::Table(t) => t,
        _ => return Ok(None),
    };

    // deaths is an array-like table
    let deaths_val = db_tbl.get::<_, LuaValue>("deaths")?;
    let deaths_tbl = match deaths_val {
        LuaValue::Table(t) => t,
        _ => return Ok(None),
    };

    // Walk to find max index
    let mut max_i: i64 = 0;
    let mut latest: Option<mlua::Table> = None;
    for pair in deaths_tbl.pairs::<LuaValue, LuaValue>() {
        let (k, v) = pair?;
        if let (LuaValue::Integer(i), LuaValue::Table(t)) = (k, v) {
            if i > max_i {
                max_i = i;
                latest = Some(t);
            }
        }
    }

    let latest = if let Some(t) = latest { t } else { return Ok(None) };

    // Helper to convert any Lua value to JSON
    fn lua_to_json(v: LuaValue) -> serde_json::Value {
        match v {
            LuaValue::Nil => serde_json::Value::Null,
            LuaValue::Boolean(b) => json!(b),
            LuaValue::Integer(i) => json!(i),
            LuaValue::Number(n) => json!(n),
            LuaValue::String(s) => json!(s.to_str().unwrap_or_default()),
            LuaValue::Table(t) => {
                // Decide array or object by checking for 1..n integer keys
                let mut is_array = true;
                let mut max_index = 0i64;
                let mut entries: Vec<(i64, serde_json::Value)> = vec![];
                for pair in t.clone().pairs::<LuaValue, LuaValue>() {
                    let (k, v) = pair.unwrap();
                    match k {
                        LuaValue::Integer(i) => {
                            if i > max_index {
                                max_index = i;
                            }
                            entries.push((i, lua_to_json(v)));
                        }
                        _ => {
                            is_array = false;
                        }
                    }
                }
                if is_array && !entries.is_empty() {
                    entries.sort_by_key(|(i, _)| *i);
                    let mut arr = vec![];
                    for (_, v) in entries {
                        arr.push(v);
                    }
                    serde_json::Value::Array(arr)
                } else {
                    let mut map = serde_json::Map::new();
                    for pair in t.pairs::<LuaValue, LuaValue>() {
                        let (k, v) = pair.unwrap();
                        let key = match k {
                            LuaValue::String(s) => s.to_str().unwrap_or_default().to_string(),
                            LuaValue::Integer(i) => i.to_string(),
                            _ => "key".into(),
                        };
                        map.insert(key, lua_to_json(v));
                    }
                    serde_json::Value::Object(map)
                }
            }
            _ => serde_json::Value::Null,
        }
    }

    let at = latest.get::<_, LuaValue>("at").ok().and_then(|v| match v {
        LuaValue::Integer(i) => Some(i),
        LuaValue::Number(n) => Some(n as i64),
        _ => None,
    }).unwrap_or(0);

    let player = latest.get::<_, LuaValue>("player").ok().and_then(|v| if let LuaValue::String(s)=v{Some(s.to_str().ok()?.to_string())}else{None}).unwrap_or_default();
    let realm  = latest.get::<_, LuaValue>("realm").ok().and_then(|v| if let LuaValue::String(s)=v{Some(s.to_str().ok()?.to_string())}else{None}).unwrap_or_default();
    let class  = latest.get::<_, LuaValue>("class").ok().and_then(|v| if let LuaValue::String(s)=v{Some(s.to_str().ok()?.to_string())}else{None});
    let level  = latest.get::<_, LuaValue>("level").ok().and_then(|v| match v { LuaValue::Integer(i)=>Some(i), LuaValue::Number(n)=>Some(n as i64), _=>None });

    let location = latest.get::<_, LuaValue>("location").map(lua_to_json).unwrap_or(serde_json::Value::Null);
    let killer   = latest.get::<_, LuaValue>("killer").map(lua_to_json).unwrap_or(serde_json::Value::Null);
    let bags     = latest.get::<_, LuaValue>("bags").map(lua_to_json).unwrap_or(serde_json::Value::Null);
    let equipped = latest.get::<_, LuaValue>("equipped").map(lua_to_json).unwrap_or(serde_json::Value::Null);

    let inst = {
        let mut m = serde_json::Map::new();
        for k in ["instanceID","instanceName","instanceDifficulty","mapDifficultyID"].iter() {
            if let Ok(v) = latest.get::<_, LuaValue>(*k) {
                m.insert((*k).into(), lua_to_json(v));
            }
        }
        serde_json::Value::Object(m)
    };

    let money_c  = latest.get::<_, LuaValue>("moneyCopper").ok().and_then(|v| match v { LuaValue::Integer(i)=>Some(i), LuaValue::Number(n)=>Some(n as i64), _=>None });
    let money_g  = latest.get::<_, LuaValue>("moneyGold").ok().and_then(|v| match v { LuaValue::Integer(i)=>Some(i), LuaValue::Number(n)=>Some(n as i64), _=>None });
    let money_s  = latest.get::<_, LuaValue>("moneySilver").ok().and_then(|v| match v { LuaValue::Integer(i)=>Some(i), LuaValue::Number(n)=>Some(n as i64), _=>None });
    let money_co = latest.get::<_, LuaValue>("moneyCopperOnly").ok().and_then(|v| match v { LuaValue::Integer(i)=>Some(i), LuaValue::Number(n)=>Some(n as i64), _=>None });

    Ok(Some(DeathPayload{
        at,
        player,
        realm,
        class,
        level,
        location,
        killer,
        bags,
        equipped,
        instance: inst,
        moneyCopper: money_c,
        moneyGold: money_g,
        moneySilver: money_s,
        moneyCopperOnly: money_co,
    }))
}

async fn upload(
    cfg: &Config,
    death: &DeathPayload,
    screenshot: Option<&Path>,
) -> Result<()> {
    let client = reqwest::Client::new();

    let mut form = multipart::Form::new()
        .text("death", serde_json::to_string(death)?);

    if let Some(sc) = screenshot {
        let file_name = sc.file_name().and_then(|s| s.to_str()).unwrap_or("screenshot.jpg").to_string();
        let bytes = fs::read(sc)?;
        let part = multipart::Part::bytes(bytes).file_name(file_name);
        form = form.part("screenshot", part);
    }

    let mut req = client.post(&cfg.api_url).multipart(form);
    if !cfg.api_token.is_empty() {
        req = req.bearer_auth(&cfg.api_token);
    }

    let resp = req.send().await?;
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Upload failed: {} - {}", resp.status(), text));
    }
    Ok(())
}

fn account_sv_paths(wow: &WowPaths) -> Vec<PathBuf> {
    let mut v = vec![];
    let pattern = wow.wtf_savedvariables_glob();
    for entry in glob(&pattern).unwrap_or_default() {
        if let Ok(p) = entry {
            v.push(p);
        }
    }
    v
}

fn format_epoch(ts: i64) -> String {
    let dt: DateTime<Utc> = DateTime::from_timestamp(ts, 0).unwrap_or_else(|| DateTime::from(SystemTime::now()));
    dt.to_rfc3339()
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load or create config
    let cfg_path = config_path()?;
    let mut cfg: Config = if cfg_path.exists() {
        let s = fs::read_to_string(&cfg_path)?;
        toml::from_str(&s)?
    } else {
        first_run_wizard().await?
    };

    // Offer to toggle startup
    let want_toggle = Confirm::new()
        .with_prompt(format!(
            "Start with Windows is currently {}. Change it?",
            if cfg.start_with_windows { "ENABLED" } else { "DISABLED" }
        ))
        .default(false)
        .interact()
        .unwrap_or(false);

    if want_toggle {
        let enable = Confirm::new()
            .with_prompt("Enable start with Windows?")
            .default(cfg.start_with_windows)
            .interact()
            .unwrap_or(cfg.start_with_windows);
        set_startup(enable)?;
        cfg.start_with_windows = enable;
        fs::write(cfg_path, toml::to_string_pretty(&cfg)?)?;
    }

    let wow = WowPaths {
        root: PathBuf::from(&cfg.wow_root),
        branch: cfg.wow_branch.clone(),
    };

    // Install/update addon
    if cfg.update_addon_on_start {
        if let Err(e) = install_or_update_addon(&wow).await {
            eprintln!("[warn] addon update failed: {e:#}");
        }
    } else {
        // still ensure folder exists
        fs::create_dir_all(wow.addons_dir().join("DeathLogger")).ok();
    }

    // Ensure Screenshots dir exists (watcher needs it)
    fs::create_dir_all(wow.screenshots_dir()).ok();

    // Build watcher list for SavedVariables
    let sv_files = account_sv_paths(&wow);
    if sv_files.is_empty() {
        println!("[info] No SavedVariables found yet. The file appears after running the game once with the addon loaded.");
    } else {
        println!("[watch] Monitoring {} SavedVariables file(s)", sv_files.len());
    }

    // Start file watchers
    let (tx, rx) = std::sync::mpsc::channel::<Event>();

    let mut watcher = RecommendedWatcher::new(
        move |res| {
            if let Ok(ev) = res {
                let _ = tx.send(ev);
            }
        },
        NotifyConfig::default(),
    )?;

    // Watch SV folders (directory-level)
    {
        let wtf_root = wow.branch_root().join("WTF").join("Account");
        if wtf_root.exists() {
            watcher.watch(&wtf_root, RecursiveMode::Recursive)?;
        }
    }
    // Watch Screenshots
    watcher.watch(&wow.screenshots_dir(), RecursiveMode::NonRecursive).ok();

    // Load persisted state
    let mut state = load_state().unwrap_or_default();

    println!("[run] Agent is running. Press Ctrl+C to exit.");
    println!("      WoW: {}", wow.branch_root().display());
    println!("      Upload URL: {}", cfg.api_url);

    // Main loop: also do a periodic poll to catch writes some drivers miss
    let mut last_poll = SystemTime::now();
    loop {
        // Non-blocking check for events (with small timeout)
        let ev = rx.recv_timeout(Duration::from_millis(500));
        match ev {
            Ok(event) => {
                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) => {
                        for p in event.paths {
                            if p.extension().map(|e| e == "lua").unwrap_or(false)
                                && p.file_name().map(|f| f == "DeathLogger.lua").unwrap_or(false)
                            {
                                if let Err(e) = handle_sv_change(&cfg, &wow, &mut state, &p).await {
                                    eprintln!("[error] SV handle: {e:#}");
                                }
                            } else if is_screenshot_file(&p) {
                                if let Err(e) = handle_screenshot_created(&wow, &mut state, &p) {
                                    eprintln!("[error] shot handle: {e:#}");
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Err(_timeout) => {
                // periodic poll every 10s to match lingering screenshots with new SV writes
                if last_poll.elapsed().unwrap_or(Duration::ZERO) > Duration::from_secs(10) {
                    last_poll = SystemTime::now();
                    if let Err(e) = periodic_poll(&cfg, &wow, &mut state).await {
                        eprintln!("[warn] poll failed: {e:#}");
                    }
                }
            }
        }
    }
}

// Check typical image extensions WoW uses (jpg, png)
fn is_screenshot_file(p: &Path) -> bool {
    if !p.is_file() { return false; }
    match p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" | "png" => true,
        _ => false,
    }
}

fn handle_screenshot_created(_wow: &WowPaths, state: &mut State, path: &Path) -> Result<()> {
    let ts = newest_mtime(path)
        .and_then(|st| st.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(|| Utc::now().timestamp());

    state.pending_screens.push_back(PendingShot {
        path: path.to_string_lossy().to_string(),
        ts_epoch: ts,
    });
    // Keep last 50 pending screenshots
    while state.pending_screens.len() > 50 {
        state.pending_screens.pop_front();
    }
    save_state(state).ok();
    println!("[queue] New screenshot queued: {}", path.display());
    Ok(())
}

async fn handle_sv_change(cfg: &Config, wow: &WowPaths, state: &mut State, sv_file: &Path) -> Result<()> {
    if !sv_file.exists() { return Ok(()); }
    let latest = match parse_latest_death_from_sv(sv_file) {
        Ok(Some(d)) => d,
        Ok(None) => return Ok(()),
        Err(e) => {
            // The file may be mid-write. Retry once later.
            return Err(e);
        }
    };

    let key = to_key(&latest.player, &latest.realm);
    let already = state.last_uploaded.get(&key).copied().unwrap_or(0);
    if latest.at <= already {
        // nothing new
        return Ok(());
    }

    // Find nearest screenshot within window
    let near = find_nearest_screenshot(state, latest.at, cfg.pair_window_secs);
    let near_path = near.as_ref().map(|p| Path::new(&p.path));

    println!(
        "[upload] {} new death for {} at {} (screenshot: {})",
        latest.class.clone().unwrap_or_default(),
        key,
        format_epoch(latest.at),
        if near.is_some() { "yes" } else { "no" }
    );

    if let Err(e) = upload(cfg, &latest, near_path).await {
        eprintln!("[error] upload failed: {e:#}");
        return Err(e);
    }

    // mark uploaded and remove matched screenshot from queue
    state.last_uploaded.insert(key, latest.at);
    if let Some(near) = near {
        if let Some(pos) = state.pending_screens.iter().position(|x| x.path == near.path) {
            state.pending_screens.remove(pos);
        }
    }
    save_state(state).ok();
    Ok(())
}

fn find_nearest_screenshot(state: &State, death_ts: i64, window_secs: i64) -> Option<PendingShot> {
    let mut best: Option<PendingShot> = None;
    let mut best_dt = i64::MAX;
    for p in &state.pending_screens {
        let dt = (p.ts_epoch - death_ts).abs();
        if dt < best_dt && dt <= window_secs {
            best_dt = dt;
            best = Some(p.clone());
        }
    }
    best
}

async fn periodic_poll(cfg: &Config, wow: &WowPaths, state: &mut State) -> Result<()> {
    // Re-scan SV files (new accounts may have appeared)
    for sv in account_sv_paths(wow) {
        if let Err(e) = handle_sv_change(cfg, wow, state, &sv).await {
            // Often due to partial writes; not fatal
            eprintln!("[poll] SV check error: {e}");
        }
    }
    Ok(())
}
