mod api;
mod audio;
mod clipboard;
mod config;
mod history;
mod hotkey;
mod paths;
mod pipeline;
mod tray;

use config::AppSettings;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Emitter, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tokio::process::Child;
use tokio::sync::Mutex;

/// Find a Python script by searching: resource_dir, executable dir, cwd
fn find_script(app: &tauri::AppHandle, name: &str) -> Result<PathBuf, String> {
    // 1. Tauri resource dir (bundled app)
    if let Ok(resource_dir) = app.path().resource_dir() {
        let candidate = resource_dir.join(name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    // 2. Next to the executable
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let candidate = exe_dir.join(name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    // 3. Current working directory
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join(name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!("{} not found", name))
}

/// Find Python interpreter.
/// If `configured_path` is set and valid, use it directly.
/// Otherwise: check .venv near the script, exe ancestors, cwd, then system python3.
fn find_python(configured_path: &str, script_path: &PathBuf) -> PathBuf {
    // 0. Use explicitly configured path
    if !configured_path.is_empty() {
        let p = PathBuf::from(configured_path);
        if p.exists() {
            return p;
        }
    }

    let venv_rel = ".venv/bin/python";

    // 1. Look for .venv next to the script
    if let Some(script_dir) = script_path.parent() {
        let candidate = script_dir.join(venv_rel);
        if candidate.exists() {
            return candidate;
        }
    }
    // 2. Walk up from executable to find .venv (covers bundled .app in project dir)
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        while let Some(d) = dir {
            let candidate = d.join(venv_rel);
            if candidate.exists() {
                return candidate;
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
    // 3. Try cwd/.venv
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join(venv_rel);
        if candidate.exists() {
            return candidate;
        }
    }
    // 4. Check setup_local_whisper's venv in Application Support
    {
        let candidate = paths::app_support_dir().join("venv").join("bin").join("python");
        if candidate.exists() {
            return candidate;
        }
    }
    // Fallback to system python3
    PathBuf::from("python3")
}

pub struct AppState {
    pub settings: Arc<Mutex<AppSettings>>,
    pub recorder: Arc<Mutex<audio::AudioRecorder>>,
    pub detector: Arc<std::sync::Mutex<hotkey::HotkeyDetector>>,
    pub stt_server_process: Arc<Mutex<Option<Child>>>,
    pub download_process: Arc<Mutex<Option<Child>>>,
    pub hotkeys_initialized: Arc<std::sync::atomic::AtomicBool>,
    pub hotkey_test_mode: Arc<std::sync::atomic::AtomicBool>,
    /// Previous clipboard text to restore after a replace-selection session.
    pub clipboard_backup: Arc<std::sync::Mutex<Option<String>>>,
    /// True between RecordStart commit and stop/cancel (incl. async startup).
    pub recording_session: Arc<std::sync::atomic::AtomicBool>,
    /// Set by Escape cancel; RecordStart checks this after awaits.
    pub cancel_requested: Arc<std::sync::atomic::AtomicBool>,
    /// Global-shortcut record key (None when record uses CGEventTap).
    pub record_shortcut: Arc<std::sync::Mutex<Option<Shortcut>>>,
    /// Global shortcuts that switch post-process mode (Pressed only).
    pub mode_shortcuts: Arc<std::sync::Mutex<Vec<(Shortcut, String)>>>,
    /// True after a Flow paste until undo (or overwritten by the next paste).
    pub paste_undoable: Arc<std::sync::atomic::AtomicBool>,
    /// Last known combined Accessibility + Input Monitoring state, as seen by
    /// the background poller. `None` until the first check after startup.
    pub last_hotkey_permission_ok: Arc<std::sync::Mutex<Option<bool>>>,
    /// Guards against starting `monitor_hotkey_permissions` twice (it can be
    /// kicked off from both `setup()` and the `initialize_hotkeys` command).
    pub permission_monitor_started: Arc<std::sync::atomic::AtomicBool>,
}

#[tauri::command]
async fn get_settings(state: tauri::State<'_, AppState>) -> Result<AppSettings, String> {
    let settings = state.settings.lock().await;
    Ok(settings.clone())
}

#[tauri::command]
async fn save_settings(
    settings: AppSettings,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let mut settings = settings;
    config::normalize_modes(&mut settings);

    let (was_history_enabled, needs_hotkey_reload, hotkey_key, activation_mode, double_tap_ms, mode_hotkeys) = {
        let current = state.settings.lock().await;
        let hotkey_changed = current.hotkey.key != settings.hotkey.key
            || current.activation_mode != settings.activation_mode
            || current.hotkey.double_tap_ms != settings.hotkey.double_tap_ms
            || current.mode_hotkeys != settings.mode_hotkeys;
        (
            current.history_enabled,
            hotkey_changed,
            settings.hotkey.key.clone(),
            settings.activation_mode.clone(),
            settings.hotkey.double_tap_ms,
            settings.mode_hotkeys.clone(),
        )
    };

    // Turning history off clears persisted entries (privacy).
    if was_history_enabled && !settings.history_enabled {
        if let Err(e) = history::clear_history() {
            log::warn!("Failed to clear history on disable: {}", e);
        }
        let _ = app.emit("history-updated", ());
    } else if settings.history_enabled {
        // Apply retention prune when saving privacy settings.
        if let Err(e) = history::load_history_pruned(settings.history_retention_days) {
            log::warn!("Failed to prune history on save: {}", e);
        }
    }

    config::save_settings(&settings).map_err(|e| e.to_string())?;

    {
        let mut current = state.settings.lock().await;
        *current = settings;
    }

    // Hot-reload hotkeys if relevant settings changed
    if needs_hotkey_reload
        && state.hotkeys_initialized.load(std::sync::atomic::Ordering::SeqCst)
    {
        reload_hotkeys(
            &app,
            &hotkey_key,
            activation_mode,
            double_tap_ms,
            &mode_hotkeys,
        );
    }

    if let Err(e) = tray::menu::rebuild_tray_menu(&app) {
        log::warn!("Failed to refresh tray menu after settings save: {}", e);
    }
    let _ = app.emit("settings-changed", ());

    Ok(())
}

#[tauri::command]
async fn test_microphone(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let mut recorder = state.recorder.lock().await;
    recorder.start().map_err(|e| format!("Microphone error: {}", e))?;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let samples = recorder.stop();

    if samples.is_empty() {
        return Ok("No samples captured. Check microphone permission in System Settings > Privacy & Security > Microphone.".to_string());
    }

    let max_amplitude = samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();

    log::info!(
        "Mic test: {} samples, max={:.6}, rms={:.6}",
        samples.len(),
        max_amplitude,
        rms
    );

    if max_amplitude > 0.0001 {
        Ok(format!(
            "Microphone working! ({} samples, peak={:.4}, rms={:.4})",
            samples.len(),
            max_amplitude,
            rms
        ))
    } else {
        Ok(format!(
            "No audio detected. {} samples captured but all silent (peak={:.6}). Check mic permission or input device.",
            samples.len(),
            max_amplitude
        ))
    }
}

#[derive(Clone, serde::Serialize)]
struct SttTestResult {
    raw_text: String,
    processed_text: Option<String>,
}

#[tauri::command]
async fn test_microphone_stt(state: tauri::State<'_, AppState>) -> Result<SttTestResult, String> {
    // 1. Record 3 seconds
    let (samples, sample_rate, channels) = {
        let mut recorder = state.recorder.lock().await;
        recorder
            .start()
            .map_err(|e| format!("Microphone error: {}", e))?;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let sr = recorder.sample_rate;
        let ch = recorder.channels;
        let s = recorder.stop();
        (s, sr, ch)
    };

    if samples.is_empty() {
        return Ok(SttTestResult {
            raw_text: "No samples captured. Check microphone permission.".to_string(),
            processed_text: None,
        });
    }

    let max_amplitude = samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
    if max_amplitude < 0.0001 {
        return Ok(SttTestResult {
            raw_text: "No audio detected. Speak louder or check mic settings.".to_string(),
            processed_text: None,
        });
    }

    // 2. Encode WAV
    let wav_bytes = audio::encode_wav(&samples, sample_rate, channels)
        .map_err(|e| format!("WAV encode error: {}", e))?;

    // 3. STT
    let settings = state.settings.lock().await.clone();
    let language = match settings.language.mode {
        config::LanguageMode::Japanese => Some("ja"),
        config::LanguageMode::English => Some("en"),
        config::LanguageMode::Auto => None,
    };

    let raw_text = api::whisper::transcribe(wav_bytes, &settings.stt, language)
        .await
        .map_err(|e| format!("STT error: {}", e))?;

    if raw_text.trim().is_empty() {
        return Ok(SttTestResult {
            raw_text: "Audio captured but STT returned empty text.".to_string(),
            processed_text: None,
        });
    }

    // 4. Filler removal, then LLM post-processing (active mode) — same order as
    //    the live pipeline, so the test can't take a different path than the
    //    real thing. `raw_text` stays untouched: it is shown as the STT result.
    let stripped = if settings.remove_fillers {
        config::strip_fillers(&raw_text)
    } else {
        raw_text.clone()
    };

    if stripped.trim().is_empty() {
        // Nothing but fillers — the live pipeline drops this recording, so
        // there is nothing to hand the LLM here either.
        return Ok(SttTestResult {
            raw_text,
            processed_text: Some(String::new()),
        });
    }

    let mode = config::resolve_active_mode(&settings);
    let mut processed_text = if mode.runs_llm() {
        let lang_str = language.unwrap_or(&settings.language.primary);
        let prompt =
            config::render_mode_prompt(&mode.system_prompt, lang_str, settings.remove_fillers);
        match api::claude::post_process(&stripped, &settings.llm, &prompt).await {
            Ok(processed) => Some(processed),
            Err(e) => {
                log::warn!("LLM post-processing failed in test: {}", e);
                Some(format!("(LLM error: {})", e))
            }
        }
    } else if stripped != raw_text {
        // Raw mode still has something to show once fillers are gone.
        Some(stripped.clone())
    } else {
        None
    };

    // 5. Apply replacement dictionary (same order as live pipeline).
    let after_llm = processed_text.as_deref().unwrap_or(&stripped);
    let replaced = config::apply_replacements(after_llm, &settings.replacements);
    if replaced != after_llm {
        processed_text = Some(replaced);
    }

    Ok(SttTestResult {
        raw_text,
        processed_text,
    })
}

#[tauri::command]
async fn check_stt_server(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let settings = state.settings.lock().await;
    let url = format!(
        "http://{}:{}/health",
        settings.local_stt_server.host, settings.local_stt_server.port
    );
    drop(settings);

    match crate::api::http_client()
        .get(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    {
        Ok(resp) => Ok(resp.status().is_success()),
        Err(_) => Ok(false),
    }
}

#[tauri::command]
async fn start_stt_server(state: tauri::State<'_, AppState>, app: tauri::AppHandle) -> Result<(), String> {
    ensure_stt_server(&state, &app).await
}

/// Ensure the local Whisper STT server is up (correct venv). Safe to call repeatedly.
async fn ensure_stt_server(state: &AppState, app: &tauri::AppHandle) -> Result<(), String> {
    // Already tracked and running.
    {
        let mut proc = state.stt_server_process.lock().await;
        if let Some(ref mut child) = *proc {
            match child.try_wait() {
                Ok(None) => {
                    log::info!("STT server process already tracked — skip start");
                    return Ok(());
                }
                _ => {
                    *proc = None;
                }
            }
        }
    }

    let (host, port) = {
        let settings = state.settings.lock().await;
        (
            settings.local_stt_server.host.clone(),
            settings.local_stt_server.port,
        )
    };
    let url = format!("http://{}:{}/health", host, port);
    let healthy = crate::api::http_client()
        .get(&url)
        .timeout(std::time::Duration::from_secs(1))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if healthy {
        // Port is serving a capable health check (faster_whisper import ok).
        // May be an orphan from a previous session — leave it alone.
        log::info!("STT server already healthy on {}:{} — skip start", host, port);
        return Ok(());
    }

    // Not healthy: free the port (broken orphan / half-dead process) then spawn.
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("lsof -ti:{} | xargs kill -9 2>/dev/null", port))
        .output();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let settings = state.settings.lock().await;
    let model = settings.local_stt_server.model.clone();
    let port = settings.local_stt_server.port;
    let host = settings.local_stt_server.host.clone();
    let python_path = settings.local_stt_server.python_path.clone();
    drop(settings);

    let script_path = find_script(app, "stt-server.py")?;
    let python = find_python(&python_path, &script_path);

    log::info!(
        "Starting STT server: {:?} {:?} --model {} --port {} --host {}",
        python, script_path, model, port, host
    );

    let mut child = tokio::process::Command::new(&python)
        .arg(&script_path)
        .arg("--model")
        .arg(&model)
        .arg("--port")
        .arg(port.to_string())
        .arg("--host")
        .arg(&host)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn server: {}", e))?;

    // Wait a moment and check if the process crashed immediately
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    match child.try_wait() {
        Ok(Some(status)) => {
            // Process already exited — read stderr for error details
            let stderr_msg = if let Some(stderr) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                let mut buf = String::new();
                let mut reader = tokio::io::BufReader::new(stderr);
                let _ = reader.read_to_string(&mut buf).await;
                buf
            } else {
                String::new()
            };
            return Err(format!(
                "Server exited immediately (code: {}). Python: {:?}, Script: {:?}\n{}",
                status, python, script_path, stderr_msg.trim()
            ));
        }
        Ok(None) => {
            log::info!("STT server process is running");
        }
        Err(e) => {
            return Err(format!("Failed to check server status: {}", e));
        }
    }

    let mut proc = state.stt_server_process.lock().await;
    *proc = Some(child);

    Ok(())
}

#[tauri::command]
async fn stop_stt_server(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut proc = state.stt_server_process.lock().await;
    if let Some(ref mut child) = *proc {
        child.kill().await.map_err(|e| format!("Failed to stop server: {}", e))?;
        let _ = child.wait().await;
    }
    *proc = None;

    // Also kill any remaining process on the configured port
    let settings = state.settings.lock().await;
    let port = settings.local_stt_server.port;
    drop(settings);
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("lsof -ti:{} | xargs kill -9 2>/dev/null", port))
        .output();

    Ok(())
}

#[tauri::command]
async fn check_downloaded_models() -> Result<Vec<String>, String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let cache_dir = std::path::PathBuf::from(&home).join(".cache/huggingface/hub");

    let models = ["tiny", "base", "small", "medium", "large-v3"];
    let mut downloaded = Vec::new();

    for model in &models {
        // faster-whisper models are cached as models--Systran--faster-whisper-{model}
        let dir_name = format!("models--Systran--faster-whisper-{}", model);
        let model_path = cache_dir.join(&dir_name);
        // Check if the snapshots directory exists and has content
        let snapshots = model_path.join("snapshots");
        if snapshots.exists() && std::fs::read_dir(&snapshots).map(|d| d.count() > 0).unwrap_or(false) {
            downloaded.push(model.to_string());
        }
    }

    Ok(downloaded)
}

#[derive(Clone, serde::Serialize)]
struct DownloadProgressEvent {
    status: String,  // "downloading" | "done" | "error" | "cancelled"
    progress: u32,   // 0-100
    message: String,
}

#[tauri::command]
async fn download_model(
    model: String,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    // Check if already downloading
    {
        let mut proc = state.download_process.lock().await;
        if let Some(ref mut child) = *proc {
            match child.try_wait() {
                Ok(None) => return Err("A download is already in progress".to_string()),
                _ => { *proc = None; }
            }
        }
    }

    let python_path = {
        let settings = state.settings.lock().await;
        settings.local_stt_server.python_path.clone()
    };
    let script_path = find_script(&app, "download-model.py")?;
    let python = find_python(&python_path, &script_path);

    log::info!("Downloading model: {}", model);

    let mut child = tokio::process::Command::new(&python)
        .arg(&script_path)
        .arg(&model)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to start download: {}", e))?;

    let stdout = child.stdout.take().ok_or("No stdout")?;

    // Store the process handle for cancellation
    {
        let mut proc = state.download_process.lock().await;
        *proc = Some(child);
    }

    // Read progress lines from stdout in background
    let download_proc = state.download_process.clone();
    tauri::async_runtime::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();

        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                let evt = DownloadProgressEvent {
                    status: parsed["status"].as_str().unwrap_or("downloading").to_string(),
                    progress: parsed["progress"].as_u64().unwrap_or(0) as u32,
                    message: parsed["message"].as_str().unwrap_or("").to_string(),
                };
                let _ = app.emit("download-progress", evt);
            }
        }

        // Wait for process to finish
        let mut proc = download_proc.lock().await;
        if let Some(ref mut child) = *proc {
            let _ = child.wait().await;
        }
        *proc = None;
    });

    Ok(())
}

#[tauri::command]
async fn cancel_download(state: tauri::State<'_, AppState>, app: tauri::AppHandle) -> Result<(), String> {
    let mut proc = state.download_process.lock().await;
    if let Some(ref mut child) = *proc {
        child.kill().await.map_err(|e| format!("Failed to cancel: {}", e))?;
        let _ = child.wait().await;
    }
    *proc = None;

    let _ = app.emit("download-progress", DownloadProgressEvent {
        status: "cancelled".to_string(),
        progress: 0,
        message: "Download cancelled".to_string(),
    });

    Ok(())
}

#[derive(Clone, serde::Serialize)]
struct SetupProgressEvent {
    step: String,
    message: String,
}

#[tauri::command]
fn get_build_number() -> String {
    option_env!("WD_BUILD_NUMBER")
        .unwrap_or("0")
        .to_string()
}

#[tauri::command]
async fn get_history(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<history::HistoryEntry>, String> {
    let (enabled, retention) = {
        let settings = state.settings.lock().await;
        (settings.history_enabled, settings.history_retention_days)
    };
    if !enabled {
        return Ok(Vec::new());
    }
    history::load_history_pruned(retention).map_err(|e| e.to_string())
}

#[tauri::command]
fn clear_history(app: tauri::AppHandle) -> Result<(), String> {
    history::clear_history().map_err(|e| e.to_string())?;
    let _ = app.emit("history-updated", ());
    if let Err(e) = tray::menu::rebuild_tray_menu(&app) {
        log::warn!("Failed to refresh tray history menu: {}", e);
    }
    Ok(())
}

#[tauri::command]
fn delete_history_entry(app: tauri::AppHandle, id: String) -> Result<(), String> {
    history::delete_entry(&id).map_err(|e| e.to_string())?;
    let _ = app.emit("history-updated", ());
    if let Err(e) = tray::menu::rebuild_tray_menu(&app) {
        log::warn!("Failed to refresh tray history menu: {}", e);
    }
    Ok(())
}

#[tauri::command]
fn copy_history_text(text: String) -> Result<(), String> {
    clipboard::paste::copy_text(&text).map_err(|e| e.to_string())
}

/// Hide settings, then paste into the previously focused app.
#[tauri::command]
fn paste_history_text(app: tauri::AppHandle, text: String) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.hide();
    }
    // Give focus time to return to the previous app.
    std::thread::sleep(std::time::Duration::from_millis(200));
    clipboard::paste::copy_and_paste(&text).map_err(|e| e.to_string())?;
    tray::menu::mark_paste_undoable(&app, true);
    Ok(())
}

/// Hayate-style window / sidebar label: `Flow (1.0.0 · build 300)`.
fn app_title_with_build(version: &str) -> String {
    let build = option_env!("WD_BUILD_NUMBER").unwrap_or("0");
    format!("Flow ({version} · build {build})")
}

#[tauri::command]
async fn open_system_preferences(pane: String) -> Result<(), String> {
    // pane: "accessibility", "input_monitoring", "microphone"
    let url = match pane.as_str() {
        "accessibility" => "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
        "input_monitoring" => "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent",
        "microphone" => "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone",
        _ => return Err(format!("Unknown pane: {}", pane)),
    };
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .map_err(|e| format!("Failed to open system preferences: {}", e))?;
    Ok(())
}

#[derive(serde::Serialize)]
struct PermissionStatus {
    accessibility: bool,
    microphone: bool,
    input_monitoring: bool,
}

/// Check Input Monitoring permission using CGPreflightListenEventAccess (macOS 10.15.4+).
/// Returns true if granted, false otherwise. Does not trigger a permission dialog.
fn check_input_monitoring() -> bool {
    extern "C" {
        fn CGPreflightListenEventAccess() -> bool;
    }
    unsafe { CGPreflightListenEventAccess() }
}

/// Check Accessibility trust (AXIsProcessTrusted). Does not trigger a permission dialog.
fn check_accessibility_trusted() -> bool {
    unsafe {
        extern "C" {
            fn AXIsProcessTrusted() -> bool;
        }
        AXIsProcessTrusted()
    }
}

/// The two permissions the hotkey CGEventTap depends on. Both are required
/// for hotkeys to actually fire — Accessibility for the tap itself, Input
/// Monitoring for receiving HID keyboard events (macOS 10.15+).
fn read_hotkey_permission_flags() -> (bool, bool) {
    (check_accessibility_trusted(), check_input_monitoring())
}

#[derive(Clone, serde::Serialize)]
struct HotkeyPermissionEvent {
    accessibility: bool,
    input_monitoring: bool,
    ok: bool,
}

/// Post a native macOS notification (no extra plugin dependency needed).
fn notify_permission_change(recovered: bool, ui_language: &str) {
    let (title, body) = if recovered {
        if ui_language == "en" {
            ("Flow", "Permissions restored — hotkey re-registered.")
        } else {
            ("Flow", "権限が復旧しました。ホットキーを再登録しました。")
        }
    } else if ui_language == "en" {
        (
            "Flow",
            "Accessibility / Input Monitoring permission is missing. The hotkey won't work until it's granted in System Settings.",
        )
    } else {
        (
            "Flow",
            "アクセシビリティ／入力監視の権限がありません。システム設定で許可するまでホットキーが反応しません。",
        )
    };
    let script = format!(
        r#"display notification "{}" with title "{}""#,
        body.replace('\\', "\\\\").replace('"', "\\\""),
        title.replace('\\', "\\\\").replace('"', "\\\"")
    );
    match std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .spawn()
    {
        // Reap on a plain OS thread (not the async runtime) so the process
        // doesn't linger as a zombie; this fires rarely (permission state
        // transitions only), so a blocking `wait()` here is fine.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => log::warn!("Failed to spawn osascript notification: {}", e),
    }
}

/// Periodically re-check Accessibility/Input Monitoring permission (the two
/// the hotkey CGEventTap depends on). If a hotkey was configured while the
/// permission was missing (or it got revoked mid-session — e.g. a macOS
/// update or TCC re-evaluation), `register_hotkeys` fails silently and the
/// user is left with a hotkey that "looks" configured but never fires, with
/// no feedback and no recovery short of an app restart. This closes that gap:
/// detect the transition and auto re-register, and notify the user either way.
async fn monitor_hotkey_permissions(app: tauri::AppHandle) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(20));
    loop {
        ticker.tick().await;

        let (accessibility, input_monitoring) = read_hotkey_permission_flags();
        let ok = accessibility && input_monitoring;

        let Some(state) = app.try_state::<AppState>() else {
            continue;
        };

        let was_ok = {
            let mut last = state.last_hotkey_permission_ok.lock().unwrap();
            let prev = *last;
            *last = Some(ok);
            prev
        };

        let _ = app.emit(
            "hotkey-permission-status",
            HotkeyPermissionEvent {
                accessibility,
                input_monitoring,
                ok,
            },
        );

        let ui_language = {
            let settings = state.settings.lock().await;
            settings.ui_language.clone()
        };

        match was_ok {
            Some(false) if ok => {
                log::info!("Hotkey permissions recovered — reloading hotkeys");
                let (hotkey_key, activation_mode, double_tap_ms, mode_hotkeys) = {
                    let settings = state.settings.lock().await;
                    (
                        settings.hotkey.key.clone(),
                        settings.activation_mode.clone(),
                        settings.hotkey.double_tap_ms,
                        settings.mode_hotkeys.clone(),
                    )
                };
                // hotkey::rshift's CGEventTap install/uninstall touches
                // unsynchronized `static mut` state and adds run-loop sources
                // via Core Foundation APIs that assume the main thread (it's
                // normally only ever called from the main-thread `setup()`
                // closure or a tauri command handler dispatched to it).
                // This poller runs on a background tokio worker, so the
                // reload must be marshaled onto the main thread — calling
                // reload_hotkeys directly here would race with any other
                // hotkey reload path (e.g. a settings save) touching the
                // same static state concurrently.
                let handle = app.clone();
                let hotkey_key_mt = hotkey_key.clone();
                let mode_hotkeys_mt = mode_hotkeys.clone();
                let activation_mode_mt = activation_mode.clone();
                let _ = app.run_on_main_thread(move || {
                    reload_hotkeys(
                        &handle,
                        &hotkey_key_mt,
                        activation_mode_mt,
                        double_tap_ms,
                        &mode_hotkeys_mt,
                    );
                });
                notify_permission_change(true, &ui_language);
            }
            Some(true) | None if !ok => {
                log::warn!(
                    "Hotkey permission missing (accessibility={}, input_monitoring={})",
                    accessibility,
                    input_monitoring
                );
                notify_permission_change(false, &ui_language);
            }
            _ => {}
        }
    }
}

/// Start `monitor_hotkey_permissions` at most once per app lifetime.
///
/// Hotkeys are registered from two places: `setup()` (onboarding already
/// completed at launch) and the `initialize_hotkeys` command (onboarding
/// finishes mid-session). The poller needs to run in both cases, so both
/// call this — the flag makes the second call a no-op instead of running
/// two pollers that would double up notifications and reload attempts.
fn start_permission_monitor_once(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    if state
        .permission_monitor_started
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    let handle = app.clone();
    tauri::async_runtime::spawn(monitor_hotkey_permissions(handle));
}

/// Unregister all hotkeys (CGEventTap + global shortcuts).
fn unregister_hotkeys(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();

    // Only unregister if currently initialized
    if !state.hotkeys_initialized.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return;
    }

    // Tear down CGEventTap (no-op if none installed)
    hotkey::rshift::uninstall_key_listener();
    hotkey::rshift::uninstall_cancel_listener();

    // Unregister all global shortcuts
    let _ = app.global_shortcut().unregister_all();

    log::info!("All hotkeys unregistered");
}

/// Register hotkeys (CGEventTap or global-shortcut).
/// Takes hotkey values directly to avoid locking the settings Mutex
/// (which would require block_on and panic inside async contexts).
fn register_hotkeys(
    app: &tauri::AppHandle,
    hotkey_str: &str,
    mode_hotkeys: &std::collections::HashMap<String, String>,
) {
    let state = app.state::<AppState>();

    // Prevent double-initialization
    if state.hotkeys_initialized.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }

    // Clear previous shortcut routing tables
    {
        let mut rec = state.record_shortcut.lock().unwrap();
        *rec = None;
        let mut modes = state.mode_shortcuts.lock().unwrap();
        modes.clear();
    }

    let use_eventtap = hotkey::rshift::is_eventtap_key(hotkey_str);

    if use_eventtap {
        let handle = app.clone();
        let key_for_log = hotkey_str.to_string();
        hotkey::rshift::install_key_listener(hotkey_str, move |pressed| {
            let state = handle.state::<AppState>();
            let mut detector = state.detector.lock().unwrap();
            let evt = if pressed {
                detector.on_key_down()
            } else {
                detector.on_key_up()
            };
            if let Some(evt) = evt {
                handle_hotkey_event(&handle, evt);
            }
        });
        log::info!("'{}' hotkey installed via CGEventTap", key_for_log);
    } else {
        let shortcut_str = parse_shortcut(hotkey_str);
        let shortcut: Shortcut = shortcut_str.parse().unwrap_or_else(|_| {
            log::warn!("Failed to parse shortcut '{}', using F5", shortcut_str);
            "F5".parse().unwrap()
        });

        app.global_shortcut()
            .register(shortcut.clone())
            .unwrap_or_else(|e| {
                log::error!("Failed to register shortcut: {}", e);
            });

        {
            let mut rec = state.record_shortcut.lock().unwrap();
            *rec = Some(shortcut);
        }

        log::info!("Global shortcut registered: {}", shortcut_str);
    }

    // Mode switch shortcuts (global-shortcut chords / function keys only).
    for (mode_id, key) in mode_hotkeys {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        if hotkey::rshift::is_eventtap_key(key) {
            log::warn!(
                "Mode hotkey for '{}' uses EventTap-only key '{}'; skipped",
                mode_id,
                key
            );
            continue;
        }
        if key == hotkey_str {
            log::warn!(
                "Mode hotkey for '{}' collides with record hotkey; skipped",
                mode_id
            );
            continue;
        }
        let shortcut_str = parse_shortcut(key);
        let Ok(shortcut) = shortcut_str.parse::<Shortcut>() else {
            log::warn!(
                "Failed to parse mode hotkey '{}' for '{}'",
                key,
                mode_id
            );
            continue;
        };
        // Skip duplicate of already-registered record shortcut
        {
            let rec = state.record_shortcut.lock().unwrap();
            if rec.as_ref() == Some(&shortcut) {
                log::warn!(
                    "Mode hotkey for '{}' collides with record shortcut; skipped",
                    mode_id
                );
                continue;
            }
        }
        if let Err(e) = app.global_shortcut().register(shortcut.clone()) {
            log::error!(
                "Failed to register mode hotkey '{}' → {}: {}",
                mode_id,
                shortcut_str,
                e
            );
            continue;
        }
        {
            let mut modes = state.mode_shortcuts.lock().unwrap();
            modes.push((shortcut, mode_id.clone()));
        }
        log::info!(
            "Mode hotkey registered: {} → {}",
            mode_id,
            shortcut_str
        );
    }

    // Escape cancels an in-progress recording (listen-only; does not swallow Esc).
    let cancel_handle = app.clone();
    hotkey::rshift::install_cancel_listener(move || {
        handle_hotkey_event(&cancel_handle, hotkey::HotkeyEvent::RecordCancel);
    });
}

/// Tear down old hotkeys, update detector mode, and re-register.
/// Takes settings values as parameters to avoid block_on inside async contexts.
fn reload_hotkeys(
    app: &tauri::AppHandle,
    hotkey_key: &str,
    activation_mode: config::ActivationMode,
    double_tap_ms: u64,
    mode_hotkeys: &std::collections::HashMap<String, String>,
) {
    unregister_hotkeys(app);

    let state = app.state::<AppState>();
    {
        let mut detector = state.detector.lock().unwrap();
        detector.update_mode(activation_mode, double_tap_ms);
    }

    register_hotkeys(app, hotkey_key, mode_hotkeys);
    log::info!("Hotkeys reloaded");
}

#[tauri::command]
async fn save_onboarding_step(
    step: u32,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut settings = state.settings.lock().await;
    settings.onboarding_step = step;
    config::save_settings(&settings).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn set_hotkey_test_mode(
    enabled: bool,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    state
        .hotkey_test_mode
        .store(enabled, std::sync::atomic::Ordering::SeqCst);
    log::info!("Hotkey test mode: {}", enabled);
    Ok(())
}

#[tauri::command]
async fn initialize_hotkeys(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().await;
    let hotkey_key = settings.hotkey.key.clone();
    let activation_mode = settings.activation_mode.clone();
    let double_tap_ms = settings.hotkey.double_tap_ms;
    let mode_hotkeys = settings.mode_hotkeys.clone();
    drop(settings);

    if state.hotkeys_initialized.load(std::sync::atomic::Ordering::SeqCst) {
        reload_hotkeys(
            &app,
            &hotkey_key,
            activation_mode,
            double_tap_ms,
            &mode_hotkeys,
        );
    } else {
        register_hotkeys(&app, &hotkey_key, &mode_hotkeys);
    }

    // Onboarding may complete (and hotkeys first get initialized) well after
    // setup() ran, in which case that startup branch never started the
    // permission poller — start it here too (no-op if already running).
    start_permission_monitor_once(&app);

    Ok(())
}

#[tauri::command]
async fn check_permissions() -> Result<PermissionStatus, String> {
    // Check Accessibility (AXIsProcessTrusted)
    let accessibility = check_accessibility_trusted();

    // Check Microphone via AVFoundation's authorizationStatus
    let microphone = {
        use std::process::Command;
        let output = Command::new("osascript")
            .arg("-e")
            .arg(r#"use framework "AVFoundation"
set status to current application's AVCaptureDevice's authorizationStatusForMediaType:(current application's AVMediaTypeAudio)
-- 0=notDetermined, 1=restricted, 2=denied, 3=authorized
if status = 3 then
    return "authorized"
else
    return "not_authorized"
end if"#)
            .output();
        match output {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim() == "authorized",
            Err(_) => false,
        }
    };

    // Check Input Monitoring via CGEventTap probe
    let input_monitoring = check_input_monitoring();

    Ok(PermissionStatus {
        accessibility,
        microphone,
        input_monitoring,
    })
}

#[tauri::command]
async fn check_venv_exists() -> bool {
    let python = paths::app_support_dir()
        .join("venv")
        .join("bin")
        .join("python");
    python.exists()
}

#[tauri::command]
async fn setup_local_whisper(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let base = paths::app_support_dir();
    let venv_path = base.join("venv");
    let python_path = venv_path.join("bin").join("python");

    // Step 1: Create venv
    let _ = app.emit("setup-progress", SetupProgressEvent {
        step: "venv".to_string(),
        message: "Creating Python virtual environment...".to_string(),
    });

    let venv_output = tokio::process::Command::new("python3")
        .args(["-m", "venv", &venv_path.to_string_lossy()])
        .output()
        .await
        .map_err(|e| format!("Failed to run python3: {}", e))?;

    if !venv_output.status.success() {
        let stderr = String::from_utf8_lossy(&venv_output.stderr);
        let _ = app.emit("setup-progress", SetupProgressEvent {
            step: "error".to_string(),
            message: format!("venv creation failed: {}", stderr),
        });
        return Err(format!("venv creation failed: {}", stderr));
    }

    // Step 2: pip install
    let _ = app.emit("setup-progress", SetupProgressEvent {
        step: "pip".to_string(),
        message: "Installing dependencies (this may take a few minutes)...".to_string(),
    });

    let pip_path = venv_path.join("bin").join("pip");
    let pip_output = tokio::process::Command::new(&pip_path)
        // faster-whisper is pinned to 1.x: stt-server.py depends on 1.x VAD
        // option names and on the decoder applying no_speech_threshold itself.
        .args(["install", "faster-whisper>=1.1,<2", "uvicorn", "fastapi", "python-multipart", "huggingface_hub"])
        .output()
        .await
        .map_err(|e| format!("Failed to run pip: {}", e))?;

    if !pip_output.status.success() {
        let stderr = String::from_utf8_lossy(&pip_output.stderr);
        let _ = app.emit("setup-progress", SetupProgressEvent {
            step: "error".to_string(),
            message: format!("pip install failed: {}", stderr),
        });
        return Err(format!("pip install failed: {}", stderr));
    }

    // Step 3: Update python_path in settings
    {
        let mut settings = state.settings.lock().await;
        settings.local_stt_server.python_path = python_path.to_string_lossy().to_string();
        settings.local_stt_server.model = "base".to_string();
        if let Err(e) = config::save_settings(&settings) {
            log::error!("Failed to save settings after venv setup: {}", e);
        }
    }

    let _ = app.emit("setup-progress", SetupProgressEvent {
        step: "done".to_string(),
        message: "Environment setup complete!".to_string(),
    });

    Ok(())
}

#[derive(Clone, serde::Serialize)]
struct AudioLevelEvent {
    level: f32,
}

/// Show or hide the overlay window and position it at the bottom center of the screen.
fn set_overlay_visible(app_handle: &tauri::AppHandle, visible: bool) {
    if let Some(overlay) = app_handle.get_webview_window("overlay") {
        if visible {
            // Position at bottom center of the primary monitor
            if let Ok(Some(monitor)) = overlay.primary_monitor() {
                let screen = monitor.size();
                let scale = monitor.scale_factor();
                let win_w = 340.0;
                let win_h = 112.0;
                let x = (screen.width as f64 / scale - win_w) / 2.0;
                let y = screen.height as f64 / scale - win_h - 80.0;
                let _ = overlay.set_position(tauri::PhysicalPosition::new(
                    (x * scale) as i32,
                    (y * scale) as i32,
                ));
            }
            // The overlay must never become key/main or focusable (never call
            // set_focus() on it; keep "focusable": false in tauri.conf.json) —
            // that would activate Flow and steal focus from the user's target app.
            let _ = overlay.show();
        } else {
            let _ = overlay.hide();
        }
    }
}

fn handle_hotkey_event(app_handle: &tauri::AppHandle, event: hotkey::HotkeyEvent) {
    // In test mode, emit detection event instead of actually recording
    let state = app_handle.state::<AppState>();
    if state
        .hotkey_test_mode
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        match event {
            hotkey::HotkeyEvent::RecordStart => {
                let _ = app_handle.emit("hotkey-detected", "pressed");
            }
            hotkey::HotkeyEvent::RecordStop => {
                let _ = app_handle.emit("hotkey-detected", "released");
            }
            hotkey::HotkeyEvent::RecordCancel => {}
        }
        return;
    }

    match event {
        hotkey::HotkeyEvent::RecordStart => {
            log::info!("Hotkey: RecordStart");

            let state = app_handle.state::<AppState>();
            // Ignore if a session is already active (e.g. double-fire)
            if state
                .recording_session
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                log::info!("RecordStart ignored — session already active");
                return;
            }
            state
                .cancel_requested
                .store(false, std::sync::atomic::Ordering::SeqCst);

            let recorder = state.recorder.clone();
            let settings_arc = state.settings.clone();
            let clipboard_backup = state.clipboard_backup.clone();
            let cancel_requested = state.cancel_requested.clone();
            let recording_session = state.recording_session.clone();
            let handle = app_handle.clone();

            tauri::async_runtime::spawn(async move {
                let abort_if_cancelled = || {
                    cancel_requested.load(std::sync::atomic::Ordering::SeqCst)
                };

                // If using local model, check server availability before recording
                let use_local_server = {
                    let s = settings_arc.lock().await;
                    s.stt.preset == "local_whisper"
                };

                if use_local_server {
                    let (host, port) = {
                        let s = settings_arc.lock().await;
                        (s.local_stt_server.host.clone(), s.local_stt_server.port)
                    };
                    let url = format!("http://{}:{}/health", host, port);
                    let client = crate::api::http_client();
                    let mut server_ok = client
                        .get(&url)
                        .timeout(std::time::Duration::from_secs(1))
                        .send()
                        .await
                        .map(|r| r.status().is_success())
                        .unwrap_or(false);

                    if !server_ok {
                        log::warn!("Local STT server not healthy — attempting auto-start");
                        let state = handle.state::<AppState>();
                        if let Err(e) = ensure_stt_server(&state, &handle).await {
                            log::warn!("STT auto-start failed: {}", e);
                        } else {
                            // Model import / bind can take a moment
                            for _ in 0..20 {
                                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                                server_ok = client
                                    .get(&url)
                                    .timeout(std::time::Duration::from_secs(1))
                                    .send()
                                    .await
                                    .map(|r| r.status().is_success())
                                    .unwrap_or(false);
                                if server_ok {
                                    break;
                                }
                            }
                        }
                    }

                    if abort_if_cancelled() {
                        recording_session.store(false, std::sync::atomic::Ordering::SeqCst);
                        return;
                    }

                    if !server_ok {
                        log::warn!("Local STT server not running, aborting recording");
                        recording_session.store(false, std::sync::atomic::Ordering::SeqCst);
                        let _ = handle.emit(
                            "error",
                            pipeline::ErrorEvent {
                                message: "ローカルモデルのサーバーが起動していません\nLocal model server is not running".to_string(),
                            },
                        );
                        let _ = handle.emit(
                            "recording-state",
                            pipeline::RecordingStateEvent {
                                state: "error".to_string(),
                            },
                        );
                        set_overlay_visible(&handle, true);
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        set_overlay_visible(&handle, false);
                        let _ = handle.emit(
                            "recording-state",
                            pipeline::RecordingStateEvent {
                                state: "idle".to_string(),
                            },
                        );
                        return;
                    }
                }

                if abort_if_cancelled() {
                    recording_session.store(false, std::sync::atomic::Ordering::SeqCst);
                    return;
                }

                // Optional: capture selection (Cmd+C) BEFORE showing overlay,
                // so focus stays on the target app.
                let replace_selection = {
                    let s = settings_arc.lock().await;
                    s.replace_selection
                };
                if replace_selection {
                    let handle_for_copy = handle.clone();
                    let backup_slot = clipboard_backup.clone();
                    let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
                    let _ = handle_for_copy.run_on_main_thread(move || {
                        if let Ok(mut g) = backup_slot.lock() {
                            *g = None;
                        }
                        let result = match clipboard::paste::capture_selection() {
                            Ok((previous, _selected)) => {
                                if let Ok(mut g) = backup_slot.lock() {
                                    *g = previous;
                                }
                                Ok(())
                            }
                            Err(e) => Err(e.to_string()),
                        };
                        let _ = tx.send(result);
                    });
                    match rx.await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => log::warn!("Selection capture failed: {}", e),
                        Err(e) => log::warn!("Selection capture channel error: {}", e),
                    }
                }

                if abort_if_cancelled() {
                    pipeline::restore_clipboard_backup(&handle);
                    recording_session.store(false, std::sync::atomic::Ordering::SeqCst);
                    return;
                }

                // Server OK (or not using local model) — show overlay and start recording
                let _ = handle.emit(
                    "recording-state",
                    pipeline::RecordingStateEvent {
                        state: "recording".to_string(),
                    },
                );
                set_overlay_visible(&handle, true);

                {
                    let mut rec = recorder.lock().await;
                    if let Err(e) = rec.start() {
                        log::error!("Failed to start recording: {}", e);
                        recording_session.store(false, std::sync::atomic::Ordering::SeqCst);
                        pipeline::restore_clipboard_backup(&handle);
                        let _ = handle.emit(
                            "error",
                            pipeline::ErrorEvent {
                                message: format!("Failed to start recording: {}", e),
                            },
                        );
                        set_overlay_visible(&handle, false);
                        let _ = handle.emit(
                            "recording-state",
                            pipeline::RecordingStateEvent {
                                state: "idle".to_string(),
                            },
                        );
                        return;
                    }
                }

                if abort_if_cancelled() {
                    {
                        let mut rec = recorder.lock().await;
                        rec.cancel();
                    }
                    pipeline::restore_clipboard_backup(&handle);
                    set_overlay_visible(&handle, false);
                    let _ = handle.emit(
                        "recording-state",
                        pipeline::RecordingStateEvent {
                            state: "idle".to_string(),
                        },
                    );
                    recording_session.store(false, std::sync::atomic::Ordering::SeqCst);
                    return;
                }

                // Emit audio level events periodically while recording
                let recorder2 = recorder.clone();
                let handle2 = handle.clone();
                tauri::async_runtime::spawn(async move {
                    // Wait for recording to actually start, then grab atomic handles
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    let (is_recording_flag, peak_level_atomic) = {
                        let rec = recorder2.lock().await;
                        rec.atomic_handles()
                    };
                    // Poll lock-free using the atomics directly
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        if !is_recording_flag.load(std::sync::atomic::Ordering::SeqCst) {
                            break;
                        }
                        let level = f32::from_bits(
                            peak_level_atomic.swap(0, std::sync::atomic::Ordering::Relaxed),
                        )
                        .clamp(0.0, 1.0);
                        let _ = handle2.emit("audio-level", AudioLevelEvent { level });
                    }
                });
            });
        }
        hotkey::HotkeyEvent::RecordStop => {
            log::info!("Hotkey: RecordStop");

            let state = app_handle.state::<AppState>();
            // Cancelled session — discard stop so we don't run STT on empty/partial audio
            if state
                .cancel_requested
                .load(std::sync::atomic::Ordering::SeqCst)
                || !state
                    .recording_session
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                log::info!("RecordStop ignored — no active session (cancelled or idle)");
                return;
            }
            state
                .recording_session
                .store(false, std::sync::atomic::Ordering::SeqCst);

            let recorder = state.recorder.clone();
            let settings = state.settings.clone();
            let handle = app_handle.clone();

            tauri::async_runtime::spawn(async move {
                let (samples, sample_rate, channels) = {
                    let mut rec = recorder.lock().await;
                    let sr = rec.sample_rate;
                    let ch = rec.channels;
                    let s = rec.stop();
                    (s, sr, ch)
                };

                let settings = settings.lock().await.clone();

                if let Err(e) = pipeline::handle_recording_complete(
                    samples,
                    sample_rate,
                    channels,
                    &settings,
                    &handle,
                )
                .await
                {
                    log::error!("Pipeline error: {}", e);
                    pipeline::restore_clipboard_backup(&handle);
                    let _ = handle.emit(
                        "error",
                        pipeline::ErrorEvent {
                            message: e.to_string(),
                        },
                    );
                    let _ = handle.emit(
                        "recording-state",
                        pipeline::RecordingStateEvent {
                            state: "idle".to_string(),
                        },
                    );
                    // Hide overlay on error (success case handled in pipeline.rs)
                    set_overlay_visible(&handle, false);
                }
            });
        }
        hotkey::HotkeyEvent::RecordCancel => {
            let state = app_handle.state::<AppState>();
            let detector_recording = state
                .detector
                .lock()
                .map(|d| d.is_recording())
                .unwrap_or(false);
            let session_active = state
                .recording_session
                .load(std::sync::atomic::Ordering::SeqCst);

            if !session_active && !detector_recording {
                // Not in a recording session — leave Escape alone for other apps
                return;
            }

            log::info!("Hotkey: RecordCancel");
            state
                .cancel_requested
                .store(true, std::sync::atomic::Ordering::SeqCst);
            state
                .recording_session
                .store(false, std::sync::atomic::Ordering::SeqCst);
            if let Ok(mut detector) = state.detector.lock() {
                detector.cancel_recording();
            }

            let recorder = state.recorder.clone();
            let handle = app_handle.clone();

            tauri::async_runtime::spawn(async move {
                {
                    let mut rec = recorder.lock().await;
                    if rec.is_recording() {
                        rec.cancel();
                    }
                }
                pipeline::restore_clipboard_backup(&handle);
                set_overlay_visible(&handle, false);
                let _ = handle.emit(
                    "recording-state",
                    pipeline::RecordingStateEvent {
                        state: "idle".to_string(),
                    },
                );
                log::info!("Recording cancelled by user");
            });
        }
    }
}

/// Convert our hotkey config string to a Tauri Shortcut
fn parse_shortcut(key: &str) -> String {
    // Split "ctrl+shift+a" into parts, convert each to Tauri shortcut format
    let parts: Vec<&str> = key.split('+').collect();
    let mut result = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;
        let converted = match part.to_lowercase().as_str() {
            "ctrl" | "meta" | "cmd" => "CmdOrCtrl".to_string(),
            "shift" => "Shift".to_string(),
            "alt" => "Alt".to_string(),
            _ if is_last => {
                // Key name: capitalize first letter for Tauri format
                let p = part.to_lowercase();
                if p.starts_with('f') && p[1..].parse::<u32>().is_ok() {
                    p.to_uppercase() // f5 → F5
                } else if p == "space" {
                    "Space".to_string()
                } else if p == "enter" {
                    "Enter".to_string()
                } else if p == "backspace" {
                    "Backspace".to_string()
                } else if p == "tab" {
                    "Tab".to_string()
                } else if p.len() == 1 {
                    p.to_uppercase() // a → A
                } else {
                    // e.g. arrowup, arrowdown — capitalize
                    let mut c = p.chars();
                    match c.next() {
                        Some(first) => first.to_uppercase().to_string() + c.as_str(),
                        None => p,
                    }
                }
            }
            _ => part.to_string(), // unknown modifier position, pass through
        };
        result.push(converted);
    }
    result.join("+")
}

/// Initialize logging to a file under ~/Library/Logs/Flow/.
///
/// The previous `env_logger::init()` wrote only to stderr and only when
/// `RUST_LOG` was set — meaning a bundled .app launched from Finder produced
/// no logs at all, making paste/post-processing failures impossible to
/// diagnose. We now default to `info` and always write to a file, while still
/// honoring `RUST_LOG` for developers running from a terminal.
fn init_logging() {
    let log_dir = dirs::home_dir()
        .map(|h| h.join("Library/Logs").join(paths::LOG_DIR_NAME))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join(paths::LOG_FILE_NAME);

    let mut builder = env_logger::Builder::new();
    builder.filter_level(log::LevelFilter::Info);
    // Let RUST_LOG override the default filter only when it is actually set,
    // so the bundled app (RUST_LOG unset) keeps the Info default rather than
    // being silenced.
    if std::env::var("RUST_LOG").is_ok() {
        builder.parse_env("RUST_LOG");
    }

    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }

    let _ = builder.try_init();
    log::info!("Logging initialized → {}", log_path.display());
}

pub fn run() {
    init_logging();

    let settings = config::load_settings().unwrap_or_default();
    let recorder = audio::AudioRecorder::new().expect("Failed to initialize audio recorder");

    let activation_mode = settings.activation_mode.clone();
    let double_tap_ms = settings.hotkey.double_tap_ms;

    // Create detector with a dummy sender — we'll dispatch events directly
    let (event_tx, _event_rx) = std::sync::mpsc::channel();
    let detector = hotkey::HotkeyDetector::new(activation_mode, double_tap_ms, event_tx);

    let onboarding_completed = settings.onboarding_completed;
    let startup_hotkey = settings.hotkey.key.clone();
    let startup_mode_hotkeys = settings.mode_hotkeys.clone();

    let app_state = AppState {
        settings: Arc::new(Mutex::new(settings)),
        recorder: Arc::new(Mutex::new(recorder)),
        detector: Arc::new(std::sync::Mutex::new(detector)),
        stt_server_process: Arc::new(Mutex::new(None)),
        download_process: Arc::new(Mutex::new(None)),
        hotkeys_initialized: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        hotkey_test_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        clipboard_backup: Arc::new(std::sync::Mutex::new(None)),
        recording_session: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        record_shortcut: Arc::new(std::sync::Mutex::new(None)),
        mode_shortcuts: Arc::new(std::sync::Mutex::new(Vec::new())),
        paste_undoable: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        last_hotkey_permission_ok: Arc::new(std::sync::Mutex::new(None)),
        permission_monitor_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {
            // Second instance attempted to launch — just ignore it
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, shortcut, event| {
                    let state = app.state::<AppState>();

                    // Mode hotkeys: switch active mode on Pressed only.
                    {
                        let modes = state.mode_shortcuts.lock().unwrap();
                        if let Some((_, mode_id)) =
                            modes.iter().find(|(s, _)| s == shortcut)
                        {
                            if event.state == ShortcutState::Pressed {
                                let mode_id = mode_id.clone();
                                drop(modes);
                                tray::menu::set_active_mode(app, &mode_id);
                            }
                            return;
                        }
                    }

                    // Record hotkey via global-shortcut (EventTap path is separate).
                    let is_record = {
                        let rec = state.record_shortcut.lock().unwrap();
                        rec.as_ref() == Some(shortcut)
                    };
                    if !is_record {
                        return;
                    }

                    let mut detector = state.detector.lock().unwrap();
                    match event.state {
                        ShortcutState::Pressed => {
                            if let Some(evt) = detector.on_key_down() {
                                handle_hotkey_event(app, evt);
                            }
                        }
                        ShortcutState::Released => {
                            if let Some(evt) = detector.on_key_up() {
                                handle_hotkey_event(app, evt);
                            }
                        }
                    }
                })
                .build(),
        )
        .manage(app_state)
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(move |app| {
            let version = app.package_info().version.to_string();
            let titled = app_title_with_build(&version);

            // Show settings window on first launch (onboarding), hide otherwise
            if let Some(window) = app.get_webview_window("settings") {
                let _ = window.set_title(&titled);
                let show_onboarding = {
                    let state = app.state::<AppState>();
                    let s = tauri::async_runtime::block_on(state.settings.lock());
                    !s.onboarding_completed
                };
                if show_onboarding {
                    let _ = window.show();
                    let _ = window.set_focus();
                } else {
                    let _ = window.hide();
                }
            }

            // Configure overlay window: transparent background, click-through, hidden initially
            if let Some(overlay) = app.get_webview_window("overlay") {
                use tauri::window::Color;
                let _ = overlay.set_background_color(Some(Color(0, 0, 0, 0)));
                let _ = overlay.set_ignore_cursor_events(true);
                let _ = overlay.hide();
            }

            // Setup system tray
            tray::menu::setup_tray(app.handle())
                .expect("Failed to setup system tray");

            // Register hotkeys immediately if onboarding is already done.
            // Otherwise, defer until the user passes the permissions page
            // (triggered via the initialize_hotkeys IPC command).
            if onboarding_completed {
                register_hotkeys(app.handle(), &startup_hotkey, &startup_mode_hotkeys);

                // Watch for Accessibility/Input Monitoring permission being
                // revoked or (re-)granted after startup, and recover without
                // requiring a manual app restart. See monitor_hotkey_permissions.
                start_permission_monitor_once(app.handle());
            } else {
                log::info!("Onboarding not completed — deferring hotkey registration");
            }

            // Auto-start local Whisper STT so dictation works without opening Settings.
            {
                let preset = {
                    let state = app.state::<AppState>();
                    let s = tauri::async_runtime::block_on(state.settings.lock());
                    s.stt.preset.clone()
                };
                if preset == "local_whisper" {
                    let handle = app.handle().clone();
                    tauri::async_runtime::spawn(async move {
                        let state = handle.state::<AppState>();
                        match ensure_stt_server(&state, &handle).await {
                            Ok(()) => log::info!("Local STT server ensured on launch"),
                            Err(e) => log::warn!("Local STT auto-start on launch failed: {}", e),
                        }
                    });
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            test_microphone,
            test_microphone_stt,
            check_stt_server,
            start_stt_server,
            stop_stt_server,
            check_downloaded_models,
            download_model,
            cancel_download,
            setup_local_whisper,
            check_venv_exists,
            open_system_preferences,
            check_permissions,
            initialize_hotkeys,
            save_onboarding_step,
            set_hotkey_test_mode,
            get_build_number,
            get_history,
            clear_history,
            delete_history_entry,
            copy_history_text,
            paste_history_text,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
