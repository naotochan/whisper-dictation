use tauri::{AppHandle, Emitter, Manager};

use crate::api::{claude, whisper};
use crate::audio;
use crate::clipboard;
use crate::config::{AppSettings, LanguageMode};
use crate::AppState;

/// Restore clipboard text saved at the start of a replace-selection session.
pub fn restore_clipboard_backup(app_handle: &AppHandle) {
    let backup = {
        let state = app_handle.state::<AppState>();
        let taken = match state.clipboard_backup.lock() {
            Ok(mut g) => g.take(),
            Err(_) => None,
        };
        taken
    };
    if backup.is_none() {
        return;
    }
    let handle = app_handle.clone();
    let _ = handle.run_on_main_thread(move || {
        std::thread::sleep(std::time::Duration::from_millis(60));
        if let Err(e) = clipboard::paste::restore_clipboard(backup.as_deref()) {
            log::warn!("Clipboard restore failed: {}", e);
        } else {
            log::info!("Clipboard restored after replace-selection session");
        }
    });
}

/// Hide the overlay window and reset its size.
fn hide_overlay(app_handle: &AppHandle) {
    if let Some(overlay) = app_handle.get_webview_window("overlay") {
        let _ = overlay.hide();
        // Reset to default pill size
        let _ = overlay.set_size(tauri::LogicalSize::new(320.0, 88.0));
    }
}

/// Resize overlay to show transcription result text.
fn resize_overlay_for_result(app_handle: &AppHandle) {
    if let Some(overlay) = app_handle.get_webview_window("overlay") {
        let _ = overlay.set_size(tauri::LogicalSize::new(500.0, 88.0));
        // Re-center on screen
        if let Ok(Some(monitor)) = overlay.primary_monitor() {
            let screen = monitor.size();
            let scale = monitor.scale_factor();
            let win_w = 500.0;
            let win_h = 88.0;
            let x = (screen.width as f64 / scale - win_w) / 2.0;
            let y = screen.height as f64 / scale - win_h - 80.0;
            let _ = overlay.set_position(tauri::PhysicalPosition::new(
                (x * scale) as i32,
                (y * scale) as i32,
            ));
        }
    }
}

#[derive(Clone, serde::Serialize)]
pub struct RecordingStateEvent {
    pub state: String, // "idle", "recording", "processing"
}

#[derive(Clone, serde::Serialize)]
pub struct TranscriptionResultEvent {
    pub text: String,
    pub raw_text: String,
    pub language: String,
}

#[derive(Clone, serde::Serialize)]
pub struct ErrorEvent {
    pub message: String,
}

/// RMS threshold below which audio is considered silence.
///
/// Empirically, speech captured from the MacBook Pro built-in mic lands around
/// RMS 0.005–0.015, while a genuinely silent tap is ~0.0003. The previous
/// 0.008 threshold sat right inside the speech range and silently dropped a
/// large fraction of real dictation. 0.002 clears true silence while letting
/// quiet speech through; downstream hallucination filtering handles any noise
/// that slips past.
const SILENCE_RMS_THRESHOLD: f32 = 0.002;

/// Phrases treated as hallucinations only when they make up the ENTIRE
/// utterance. Anything that could plausibly appear inside real dictation
/// belongs here rather than in the substring list below — "ご覧いただき
/// ありがとうございます" is a stock business phrase, and "資料をご覧いただき
/// ありがとうございます" must survive.
///
/// Matching ignores whitespace and trailing punctuation, so whole hallucinated
/// sentences can be pasted in as observed.
const HALLUCINATION_EXACT: &[&str] = &[
    "ありがとうございました",
    "ありがとうございます",
    "お疲れ様でした",
    "おやすみなさい",
    "Thank you.",
    "Goodbye.",
    "you",
    // Video sign-offs that are also things a person might genuinely dictate.
    "ご覧いただきありがとうございました",
    "ご覧いただきありがとうございます",
    "最後までご覧いただきありがとうございました",
    "最後までご覧いただきありがとうございます",
    "次回もお楽しみに",
    "チャンネル登録お願いします",
    "チャンネル登録をお願いします",
    "字幕作成",
    "Please subscribe.",
    // Whole hallucinated sentences that carry no reusable marker. These can
    // only be collected one at a time; new sightings go here, and they must be
    // whole-utterance so a shared opening clause can't eat real speech
    // ("子供のお話を聞いてみると面白いですね" is perfectly ordinary).
    "子供のお話を聞いてみると 子供にとっての気持ちがいいです",
    // Whisper echoes the initial prompt back when fed silence or noise. These
    // must stay in sync with the prompts in api/whisper.rs.
    "音声入力による文章の書き取りです",
    "This is a voice dictation transcription.",
];

/// Marker phrases that are virtually never real dictation. Whisper emits these
/// (often with extra words prepended/appended) when fed silence or noise, e.g.
/// "字幕をご覧いただきまして、ご視聴ありがとうございました。". We filter the
/// result if it CONTAINS any of these anywhere, to catch such variants.
///
/// Prefer the shortest fragment that is still unambiguous. Whisper conjugates
/// these endlessly — 視聴ありがとうございました / 視聴してくださって
/// ありがとうございました / 視聴いただきありがとうございます — so matching on a
/// full sentence only catches the one variant we happened to write down.
/// Every entry must fail this test: "could this appear in the middle of a
/// sentence someone actually dictates?" If yes, it belongs in the exact list.
/// "チャンネル登録" reads as a hallucination but "このチャンネル登録しておいて"
/// is ordinary speech, which is why the bare noun is not here.
const HALLUCINATION_CONTAINS: &[&str] = &[
    // Video sign-offs, by far the most common source.
    "視聴ありがとう",
    "清聴ありがとう",
    "視聴してくださって",
    "視聴してくださり",
    "最後までご視聴",
    "チャンネル登録と高評価",
    "高評価とチャンネル",
    // Subtitle credits.
    "字幕をご覧",
    "字幕作成者",
    "字幕提供",
    "thank you for watching",
    "thanks for watching",
    "subscribe to my channel",
    "amara.org",
];

/// Lowercase, strip all whitespace, drop trailing sentence punctuation.
///
/// Whitespace has to go: Whisper's spacing inside a hallucination is arbitrary
/// ("最後まで視聴してくださって ありがとうございました"), so a marker written
/// without the space would otherwise miss.
fn normalize_for_match(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .trim_end_matches(&['.', '!', '?', '。', '！', '？'][..])
        .to_lowercase()
}

/// Non-speech annotations Whisper writes for music, applause and the like:
/// "(音楽)", "［拍手］", "【BGM】", "♪～♪".
fn is_non_speech_annotation(normalized: &str) -> bool {
    // No angle brackets: "<div>を使ってレイアウトして</div>" is real dictation
    // in code mode, and no annotation uses them.
    const BRACKETS: &[(char, char)] = &[
        ('(', ')'),
        ('（', '）'),
        ('[', ']'),
        ('［', '］'),
        ('【', '】'),
        ('〔', '〕'),
    ];
    let (Some(first), Some(last)) = (normalized.chars().next(), normalized.chars().last()) else {
        return false;
    };
    let len = normalized.chars().count();
    // Short and with nothing bracketed inside, or this also swallows
    // "(笑)そうですね、それでいきましょう(かっこ)" and parenthetical asides.
    if (2..=12).contains(&len)
        && BRACKETS
            .iter()
            .any(|(open, close)| first == *open && last == *close)
    {
        let inner_has_bracket = normalized
            .chars()
            .skip(1)
            .take(len - 2)
            .any(|c| BRACKETS.iter().any(|(o, c2)| c == *o || c == *c2));
        if !inner_has_bracket {
            return true;
        }
    }
    // Music/filler symbols with nothing else in the utterance.
    !normalized.is_empty()
        && normalized
            .chars()
            .all(|c| matches!(c, '♪' | '♬' | '♫' | '〜' | '～' | '~' | '-' | '−' | 'ー' | '.' | '。'))
}

/// Wall-clock length of the capture, or `None` if the device config makes it
/// unknowable. `audio_samples` is interleaved, so one second of audio is
/// `sample_rate * channels` entries.
///
/// `None` rather than `0`: a bogus sample rate would otherwise read as
/// "0ms long" and silently discard every single recording.
fn recording_duration_ms(sample_count: usize, sample_rate: u32, channels: u16) -> Option<u64> {
    let samples_per_second = sample_rate as u64 * channels.max(1) as u64;
    if samples_per_second == 0 {
        return None;
    }
    Some(sample_count as u64 * 1000 / samples_per_second)
}

fn is_hallucination(text: &str) -> bool {
    let normalized = normalize_for_match(text);
    if normalized.is_empty() {
        return true;
    }

    // 1. Whole-utterance exact match for ambiguous short phrases.
    if HALLUCINATION_EXACT
        .iter()
        .any(|phrase| normalized == normalize_for_match(phrase))
    {
        return true;
    }

    // 2. Substring match for unambiguous "viewing/subtitle" markers, which
    //    catches prefixed/suffixed hallucination variants.
    if HALLUCINATION_CONTAINS
        .iter()
        .any(|marker| normalized.contains(&normalize_for_match(marker)))
    {
        return true;
    }

    // 3. Bracketed sound annotations, which are never dictated text.
    is_non_speech_annotation(&normalized)
}

pub async fn handle_recording_complete(
    audio_samples: Vec<f32>,
    sample_rate: u32,
    channels: u16,
    settings: &AppSettings,
    app_handle: &AppHandle,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Same teardown as every other early exit below — without hiding the
    // overlay the recording pill stays on screen forever, and a hotkey brush
    // that captures zero samples is exactly how you get here.
    if audio_samples.is_empty() {
        restore_clipboard_backup(app_handle);
        hide_overlay(app_handle);
        let _ = app_handle.emit(
            "recording-state",
            RecordingStateEvent {
                state: "idle".to_string(),
            },
        );
        return Ok(());
    }

    // 0a. Too-short recording — an accidental hotkey brush captures a fraction
    // of a second of room noise, which is exactly what Whisper hallucinates on.
    let duration_ms = recording_duration_ms(audio_samples.len(), sample_rate, channels);
    if let (Some(duration_ms), true) = (duration_ms, settings.min_recording_ms > 0) {
        if duration_ms < settings.min_recording_ms as u64 {
            log::info!(
                "Recording too short ({}ms < {}ms), skipping STT",
                duration_ms,
                settings.min_recording_ms
            );
            restore_clipboard_backup(app_handle);
            hide_overlay(app_handle);
            let _ = app_handle.emit(
                "recording-state",
                RecordingStateEvent {
                    state: "idle".to_string(),
                },
            );
            return Ok(());
        }
    }

    // 0b. Silence detection — skip STT if audio is too quiet
    let rms = (audio_samples.iter().map(|s| s * s).sum::<f32>() / audio_samples.len() as f32).sqrt();
    if rms < SILENCE_RMS_THRESHOLD {
        log::info!("Audio too quiet (RMS={:.5}), skipping STT", rms);
        restore_clipboard_backup(app_handle);
        hide_overlay(app_handle);
        let _ = app_handle.emit(
            "recording-state",
            RecordingStateEvent {
                state: "idle".to_string(),
            },
        );
        return Ok(());
    }
    log::info!("Audio RMS: {:.5}", rms);

    // 1. Emit processing state
    let _ = app_handle.emit(
        "recording-state",
        RecordingStateEvent {
            state: "processing".to_string(),
        },
    );

    // 2. Encode to WAV
    let wav_bytes = audio::encode_wav(&audio_samples, sample_rate, channels)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?;
    log::info!("WAV encoded: {} bytes", wav_bytes.len());

    // 3. Determine language
    let language = match settings.language.mode {
        LanguageMode::Japanese => Some("ja"),
        LanguageMode::English => Some("en"),
        LanguageMode::Auto => None,
    };

    // 4. Transcribe with STT
    let raw_text = whisper::transcribe(wav_bytes, &settings.stt, language).await?;
    log::info!("STT result: {}", raw_text);

    // Check for hallucination or empty result
    if is_hallucination(&raw_text) {
        log::info!("Filtered hallucination: '{}'", raw_text.trim());
        restore_clipboard_backup(app_handle);
        hide_overlay(app_handle);
        let _ = app_handle.emit(
            "recording-state",
            RecordingStateEvent {
                state: "idle".to_string(),
            },
        );
        return Ok(());
    }

    // Strip fillers before the emptiness check below, which then doubles as the
    // guard for an utterance that was nothing but "えーと".
    //
    // Kept beside `raw_text` rather than replacing it: this edits the user's own
    // words, and the history entry is the only place left to see what went.
    let stripped = if settings.remove_fillers {
        let stripped = crate::config::strip_fillers(&raw_text);
        if stripped != raw_text {
            log::info!("Fillers removed: '{}'", stripped);
        }
        stripped
    } else {
        raw_text.clone()
    };

    if stripped.trim().is_empty() {
        restore_clipboard_backup(app_handle);
        hide_overlay(app_handle);
        let _ = app_handle.emit(
            "recording-state",
            RecordingStateEvent {
                state: "idle".to_string(),
            },
        );
        return Ok(());
    }

    // 5. Post-process with LLM mode (raw skips LLM)
    let mode = crate::config::resolve_active_mode(&settings);
    let mut final_text = if mode.runs_llm() {
        let lang_str = language.unwrap_or(&settings.language.primary);
        let prompt = crate::config::render_mode_prompt(
            &mode.system_prompt,
            lang_str,
            settings.remove_fillers,
        );
        log::info!("LLM mode: {}", mode.id);
        match claude::post_process(&stripped, &settings.llm, &prompt).await {
            Ok(processed) => processed,
            Err(e) => {
                log::warn!("LLM post-processing failed: {}, using raw text", e);
                stripped.clone()
            }
        }
    } else {
        stripped.clone()
    };

    // 5b. Apply replacement dictionary / snippets (after LLM so paste/history match UI).
    let replaced = crate::config::apply_replacements(&final_text, &settings.replacements);
    if replaced != final_text {
        log::info!(
            "Replacements applied ({} → {} chars)",
            final_text.len(),
            replaced.len()
        );
        final_text = replaced;
    }

    log::info!("Final text: {}", final_text);

    // 6. Copy and paste FIRST, before touching the overlay window.
    //
    // Resizing/repositioning the overlay (step 7) can activate our app on
    // macOS and steal key focus from the target app, so a Cmd+V issued
    // afterwards would land in the overlay instead of where the user is
    // typing. Pasting first avoids that race entirely.
    // (Must run on the main thread for macOS enigo/HIToolbox.)
    //
    // replace_selection implies paste even if auto_paste is off (otherwise
    // the captured selection would be left on the clipboard unused).
    let should_paste = settings.auto_paste || settings.replace_selection;
    if should_paste {
        let text_for_paste = final_text.clone();
        let handle = app_handle.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

        handle
            .run_on_main_thread(move || {
                let result = clipboard::paste::copy_and_paste(&text_for_paste)
                    .map_err(|e| e.to_string());
                let _ = tx.send(result);
            })
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                e.to_string().into()
            })?;

        rx.await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                e.to_string().into()
            })?
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

        crate::tray::menu::mark_paste_undoable(app_handle, true);
    }

    // Restore the user's prior clipboard (best-effort) after paste settles.
    restore_clipboard_backup(app_handle);

    // 7. Resize overlay and emit transcription result for display.
    resize_overlay_for_result(app_handle);
    let language_label = language.unwrap_or("auto").to_string();
    let _ = app_handle.emit(
        "transcription-result",
        TranscriptionResultEvent {
            text: final_text.clone(),
            raw_text: raw_text.clone(),
            language: language_label.clone(),
        },
    );

    // Persist to recognition history (best-effort), unless privacy mode disables it.
    if settings.history_enabled {
        match crate::history::push_entry(
            &final_text,
            &raw_text,
            &language_label,
            settings.history_retention_days,
        ) {
            Ok(entry) => {
                let _ = app_handle.emit("history-updated", entry);
                if let Err(e) = crate::tray::menu::rebuild_tray_menu(app_handle) {
                    log::warn!("Failed to refresh tray history menu: {}", e);
                }
            }
            Err(e) => log::warn!("Failed to save history: {}", e),
        }
    }

    // 8. Return to idle
    let _ = app_handle.emit(
        "recording-state",
        RecordingStateEvent {
            state: "idle".to_string(),
        },
    );

    // 9. Hide overlay after a brief delay so user can see the result
    let handle_for_hide = app_handle.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        hide_overlay(&handle_for_hide);
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_hallucination, recording_duration_ms};

    #[test]
    fn duration_accounts_for_interleaved_channels() {
        // 48kHz stereo: 96000 samples is one second of audio, not two.
        assert_eq!(recording_duration_ms(96_000, 48_000, 2), Some(1000));
        assert_eq!(recording_duration_ms(48_000, 48_000, 1), Some(1000));
        assert_eq!(recording_duration_ms(12_000, 48_000, 2), Some(125));
        // A malformed config yields "unknown", not "0ms", so the minimum-length
        // guard skips instead of discarding every recording.
        assert_eq!(recording_duration_ms(1_000, 0, 2), None);
    }

    #[test]
    fn filters_video_signoff_variants() {
        // Every one of these was missed by matching on full sentences.
        for text in [
            "ご視聴ありがとうございました。",
            "最後まで視聴してくださって ありがとうございました。",
            "最後までご視聴いただきありがとうございます",
            "チャンネル登録と高評価をお願いします",
            "字幕をご覧いただきまして、ご視聴ありがとうございました。",
            "Thanks for watching!",
            "Subtitles by the Amara.org community",
            "子供のお話を聞いてみると 子供にとっての気持ちがいいです",
        ] {
            assert!(is_hallucination(text), "should be filtered: {text}");
        }
    }

    /// Prefixed sign-offs we deliberately let through. "本日もご覧いただき
    /// ありがとうございました" is a hallucination, but it is structurally
    /// identical to "資料をご覧いただきありがとうございます", which is ordinary
    /// business dictation. There is no way to keep one without eating the
    /// other, and losing real speech is the worse failure — VAD, the minimum
    /// recording length and the RMS gate are the defences that do not have to
    /// guess at meaning.
    #[test]
    fn ambiguous_prefixed_signoffs_are_knowingly_kept() {
        assert!(!is_hallucination("本日もご覧いただきありがとうございました！"));
        // The bare phrase, with no prefix, is still filtered.
        assert!(is_hallucination("ご覧いただきありがとうございました"));
    }

    #[test]
    fn filters_non_speech_annotations() {
        for text in ["(音楽)", "［拍手］", "【BGM】", "♪～♪", "...", "  "] {
            assert!(is_hallucination(text), "should be filtered: {text}");
        }
    }

    #[test]
    fn filters_echoed_prompt_and_bare_pleasantries() {
        for text in [
            "音声入力による文章の書き取りです。",
            "ありがとうございました",
            "Thank you.",
        ] {
            assert!(is_hallucination(text), "should be filtered: {text}");
        }
    }

    #[test]
    fn keeps_real_dictation() {
        // The filter eating real speech is worse than letting one through, so
        // these guard the substring markers against over-reaching.
        for text in [
            "今日はいい天気ですね。少し散歩に行こうと思います。",
            "明日の会議の資料をまとめておいてもらえますか。",
            "先ほどの件、ありがとうございました。助かりました。",
            "字幕の位置を少し下げたいので調整をお願いします",
            "この動画の音量を上げてください",
            "Please review the pull request when you get a chance.",
            "ご覧のとおり、テストはすべて通っています",
            // Every one of these was destroyed by the substring markers, three
            // of them before this change. They are the reason the ambiguous
            // markers moved to whole-utterance matching.
            "資料をご覧いただきありがとうございます。",
            "デモ動画を視聴いただき、ご意見をお聞かせください。",
            "添付を最後までご覧ください。",
            "動画を最後まで視聴した人の割合を教えて",
            "アプリストアで高評価をお願いします",
            "子供のお話を聞いてみると面白いですね",
            "このチャンネル登録しておいて",
            "字幕作成ツールを探しています",
            "The subtitles by default are turned off.",
            "Please subscribe to the newsletter before Friday.",
            // Parenthetical asides and markup are not sound annotations.
            "(笑)そうですね、それでいきましょう(かっこ)",
            "<div>を使ってレイアウトしてください</div>",
            "(これは重要な補足事項です)",
        ] {
            assert!(!is_hallucination(text), "should be kept: {text}");
        }
    }
}
