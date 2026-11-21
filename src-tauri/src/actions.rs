#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::apple_intelligence;
use crate::audio_feedback::{play_feedback_sound, play_feedback_sound_blocking, SoundType};
use crate::audio_toolkit::VadPolicy;
use crate::clipboard::PasteOptions;
use crate::context::{self, AppInfo, TransformContext};
use crate::managers::audio::{recording_notice_code, AudioRecordingManager};
use crate::managers::history::HistoryManager;
use crate::managers::model::ModelManager;
use crate::managers::transcription::policy::{live_decode, LiveDecode, ModelTraits};
use crate::managers::transcription::StreamWorkKind;
use crate::managers::transcription::{Transcription, TranscriptionManager};
use crate::notices::{self, Notice, NoticeCode, NoticeSource};
use crate::settings::{
    get_settings, AppSettings, OverlayStyle, TransformAppRule, TransformMode,
    TransformSelectionBehavior, APPLE_INTELLIGENCE_PROVIDER_ID,
};
use crate::shortcut;
use crate::tray::{set_tray_state, TrayIconState};
use crate::utils::{
    self, show_processing_overlay, show_recording_overlay, show_transcribing_overlay,
};
use crate::TranscriptionCoordinator;
use log::{debug, error, warn};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tauri_specta::Event;

/// How a dictation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptionOutcome {
    /// The text was typed.
    Typed,
    /// Nothing was heard, so nothing was typed.
    Empty,
    /// The microphone sent only digital silence (e.g. muted).
    Silent,
    /// Transcribing or pasting failed.
    Failed,
}

/// Emitted when a dictation finishes (not on cancel); onboarding uses it to measure speed.
#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct TranscriptionCompleteEvent {
    pub outcome: TranscriptionOutcome,
    pub audio_seconds: f64,
    pub wait_seconds: f64,
}

fn emit_transcription_complete(
    app: &AppHandle,
    outcome: TranscriptionOutcome,
    sample_count: usize,
    stop_time: Instant,
) {
    let event = TranscriptionCompleteEvent {
        outcome,
        audio_seconds: sample_count as f64
            / crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE as f64,
        wait_seconds: stop_time.elapsed().as_secs_f64(),
    };
    if let Err(e) = event.emit(app) {
        warn!("Failed to emit transcription complete: {e}");
    }
}

/// Nothing to type: `Silent` if the microphone sent only zeros, else `Empty`.
fn empty_outcome(input_silent: bool) -> TranscriptionOutcome {
    if input_silent {
        TranscriptionOutcome::Silent
    } else {
        TranscriptionOutcome::Empty
    }
}

/// Ends a dictation that has nothing to type. A microphone that sent only
/// digital silence (muted, or the wrong default device) gets a notice, so it
/// doesn't look like the model missed the speech (#1899).
fn finish_without_text(
    app: &AppHandle,
    input_silent: bool,
    sample_count: usize,
    stop_time: Instant,
) {
    if input_silent {
        warn!("The microphone sent only digital silence; nothing was transcribed");
        notices::raise(
            app,
            Notice::new(NoticeCode::MicrophoneSilent, NoticeSource::Dictation),
        );
    } else {
        utils::hide_recording_overlay(app);
    }
    set_tray_state(app, TrayIconState::Idle);
    emit_transcription_complete(app, empty_outcome(input_silent), sample_count, stop_time);
}

const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Deadline for the whole post-processing step, retries included. The HTTP
/// client only bounds connecting, so an endpoint that accepts the request and
/// never answers would otherwise hold the dictation until a proxy gives up.
const POST_PROCESS_TIMEOUT: Duration = Duration::from_secs(60);

/// Bumped per recording session, so a late paste from an earlier session
/// doesn't reset the tray or hide the overlay of a newer one.
static RECORDING_SESSION: AtomicU64 = AtomicU64::new(0);

/// Tracks whether an instant selection transform (no microphone recording) is currently running.
static INSTANT_TRANSFORM_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Monotonically incremented whenever an instant selection transform starts so the coordinator
/// can distinguish a fast instant-transform start from a failed start even if it completes quickly.
static INSTANT_TRANSFORM_SEQ: AtomicU64 = AtomicU64::new(0);

pub(crate) fn is_instant_transform_active() -> bool {
    INSTANT_TRANSFORM_ACTIVE.load(Ordering::SeqCst)
}

pub(crate) fn instant_transform_seq() -> u64 {
    INSTANT_TRANSFORM_SEQ.load(Ordering::SeqCst)
}

struct InstantTransformGuard(AppHandle);
impl Drop for InstantTransformGuard {
    fn drop(&mut self) {
        INSTANT_TRANSFORM_ACTIVE.store(false, Ordering::SeqCst);
        shortcut::unregister_cancel_shortcut(&self.0);
        if let Some(c) = self.0.try_state::<TranscriptionCoordinator>() {
            c.notify_processing_finished();
        }
    }
}

/// Drop guard that finishes the transcription pipeline, including immediate
/// model unloading on early exits.
struct FinishGuard(AppHandle, Arc<TranscriptionManager>);
impl Drop for FinishGuard {
    fn drop(&mut self) {
        self.1.maybe_unload_immediately("transcription session");
        if let Some(c) = self.0.try_state::<TranscriptionCoordinator>() {
            c.notify_processing_finished();
        }
        // The pipeline just freed its large transient buffers (captured PCM,
        // WAV copy, engine scratch); hand the cached pages back to the OS so
        // they don't sit in malloc arenas until they get swapped out (#1792).
        crate::memory::trim_freed_memory();
    }
}

// Shortcut Action Trait
pub trait ShortcutAction: Send + Sync {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActionKind {
    Transcribe,
    PostProcess,
    Transform,
}

#[derive(Clone, Debug)]
struct CapturedTransformSession {
    session_id: u64,
    mode: TransformMode,
    context: TransformContext,
    prompt_override_id: Option<String>,
}

static ACTIVE_TRANSFORM_SESSION: Lazy<Mutex<Option<CapturedTransformSession>>> =
    Lazy::new(|| Mutex::new(None));

// Transcribe Action
struct TranscribeAction {
    kind: ActionKind,
}

/// Field name for structured output JSON schema
const TRANSCRIPTION_FIELD: &str = "transcription";

/// Strip invisible Unicode characters that some LLMs may insert
fn strip_invisible_chars(s: &str) -> String {
    s.replace(['\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}'], "")
}

/// Strip a leading `<think>...</think>` block. Some endpoints can't disable
/// reasoning, and some local servers put the reasoning text into `content`
/// instead of a separate field — without this the user would get the model's
/// chain of thought pasted along with the cleaned transcription.
fn strip_think_block(s: &str) -> &str {
    if let Some(rest) = s.trim_start().strip_prefix("<think>") {
        if let Some(end) = rest.find("</think>") {
            return rest[end + "</think>".len()..].trim_start();
        }
    }
    s
}

/// Build a system prompt from the user's prompt template.
/// Removes `${output}` placeholder since the transcription is sent as the user message.
fn build_system_prompt(prompt_template: &str) -> String {
    prompt_template.replace("${output}", "").trim().to_string()
}

/// Returns `true` when a transcription has no meaningful content to
/// post-process (empty or whitespace-only). Used to skip the post-processing
/// LLM call when nothing was actually transcribed, which would otherwise make
/// the model reply with an error message such as "you need to provide the
/// transcription".
fn is_blank_transcription(transcription: &str) -> bool {
    transcription.trim().is_empty()
}

fn non_blank_post_processed_text(text: String) -> Option<String> {
    if is_blank_transcription(&text) {
        warn!(
            "Post-processing returned an empty response. Falling back to original transcription."
        );
        None
    } else {
        Some(text)
    }
}

async fn complete_unless_cancelled<F, C>(operation: F, is_cancelled: C) -> Option<F::Output>
where
    F: Future,
    C: Fn() -> bool,
{
    tokio::pin!(operation);

    loop {
        if is_cancelled() {
            return None;
        }

        if let Ok(result) =
            tokio::time::timeout(CANCELLATION_POLL_INTERVAL, operation.as_mut()).await
        {
            return Some(result);
        }
    }
}

fn should_use_streaming_overlay(style: OverlayStyle, is_streaming: bool) -> bool {
    style == OverlayStyle::Live && is_streaming
}

/// What became of a transcription's post-processing.
#[derive(Debug, PartialEq, Eq)]
enum PostProcessOutcome {
    /// There was no speech to process.
    Skipped,
    Done(String),
    /// The user asked for it, but it couldn't run or produced nothing, so the
    /// transcript goes out as it is.
    Failed,
}

async fn run_llm_completion(
    settings: &AppSettings,
    system_prompt: String,
    user_content: String,
    legacy_prompt: String,
) -> PostProcessOutcome {
    let provider = match settings.active_post_process_provider().cloned() {
        Some(provider) => provider,
        None => {
            debug!("Post-processing enabled but no provider is selected");
            return PostProcessOutcome::Failed;
        }
    };

    let model = settings
        .post_process_models
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    if model.trim().is_empty() {
        debug!(
            "Post-processing skipped because provider '{}' has no model configured",
            provider.id
        );
        return PostProcessOutcome::Failed;
    }

    debug!(
        "Starting LLM request with provider '{}' (model: {})",
        provider.id, model
    );

    let api_key = settings
        .post_process_api_keys
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    // Ask these providers to skip reasoning/thinking — post-processing rarely
    // benefits from it and it adds seconds of latency. llm_client picks the
    // field the endpoint understands and retries without it if rejected.
    let disable_reasoning = matches!(provider.id.as_str(), "custom" | "openrouter");

    if provider.supports_structured_output {
        debug!("Using structured outputs for provider '{}'", provider.id);

        // Handle Apple Intelligence separately since it uses native Swift APIs
        if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                if !apple_intelligence::check_apple_intelligence_availability() {
                    debug!(
                        "Apple Intelligence selected but not currently available on this device"
                    );
                    return PostProcessOutcome::Failed;
                }

                let token_limit = model.trim().parse::<i32>().unwrap_or(0);
                return match apple_intelligence::process_text_with_system_prompt(
                    &system_prompt,
                    &user_content,
                    token_limit,
                ) {
                    Ok(result) => {
                        if result.trim().is_empty() {
                            debug!("Apple Intelligence returned an empty response");
                            PostProcessOutcome::Failed
                        } else {
                            let result = strip_invisible_chars(&result);
                            debug!(
                                "Apple Intelligence post-processing succeeded. Output length: {} chars",
                                result.len()
                            );
                            PostProcessOutcome::Done(result)
                        }
                    }
                    Err(err) => {
                        error!("Apple Intelligence post-processing failed: {}", err);
                        PostProcessOutcome::Failed
                    }
                };
            }

            #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
            {
                debug!("Apple Intelligence provider selected on unsupported platform");
                return PostProcessOutcome::Failed;
            }
        }

        // Define JSON schema for transcription output
        let json_schema = serde_json::json!({
            "type": "object",
            "properties": {
                (TRANSCRIPTION_FIELD): {
                    "type": "string",
                    "description": "The cleaned and processed transcription text"
                }
            },
            "required": [TRANSCRIPTION_FIELD],
            "additionalProperties": false
        });

        match crate::llm_client::send_chat_completion_with_schema(
            &provider,
            api_key.clone(),
            &model,
            user_content,
            Some(system_prompt),
            Some(json_schema),
            disable_reasoning,
        )
        .await
        {
            Ok(Some(content)) => {
                // Parse the JSON response to extract the transcription field
                let content = strip_think_block(&content);
                match serde_json::from_str::<serde_json::Value>(content) {
                    Ok(json) => {
                        if let Some(transcription_value) =
                            json.get(TRANSCRIPTION_FIELD).and_then(|t| t.as_str())
                        {
                            let result = strip_invisible_chars(transcription_value);
                            debug!(
                                "Structured output post-processing succeeded for provider '{}'. Output length: {} chars",
                                provider.id,
                                result.len()
                            );
                            return PostProcessOutcome::Done(result);
                        } else {
                            error!("Structured output response missing 'transcription' field");
                            return PostProcessOutcome::Done(strip_invisible_chars(content));
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to parse structured output JSON: {}. Returning raw content.",
                            e
                        );
                        return PostProcessOutcome::Done(strip_invisible_chars(content));
                    }
                }
            }
            Ok(None) => {
                error!("LLM API response has no content");
                return PostProcessOutcome::Failed;
            }
            Err(e) => {
                warn!(
                    "Structured output failed for provider '{}': {}. Falling back to legacy mode.",
                    provider.id, e
                );
                // Fall through to legacy mode below
            }
        }
    }

    debug!("Processed prompt length: {} chars", legacy_prompt.len());

    match crate::llm_client::send_chat_completion(
        &provider,
        api_key,
        &model,
        legacy_prompt,
        disable_reasoning,
    )
    .await
    {
        Ok(Some(content)) => {
            let content = strip_invisible_chars(strip_think_block(&content));
            debug!(
                "LLM post-processing succeeded for provider '{}'. Output length: {} chars",
                provider.id,
                content.len()
            );
            PostProcessOutcome::Done(content)
        }
        Ok(None) => {
            error!("LLM API response has no content");
            PostProcessOutcome::Failed
        }
        Err(e) => {
            error!(
                "LLM post-processing failed for provider '{}': {}. Falling back to original transcription.",
                provider.id,
                e
            );
            PostProcessOutcome::Failed
        }
    }
}

async fn post_process_transcription(
    settings: &AppSettings,
    transcription: &str,
) -> PostProcessOutcome {
    if is_blank_transcription(transcription) {
        debug!("Post-processing skipped because the transcription is empty");
        return PostProcessOutcome::Skipped;
    }

    let selected_prompt_id = match &settings.post_process_selected_prompt_id {
        Some(id) => id.clone(),
        None => {
            debug!("Post-processing skipped because no prompt is selected");
            return PostProcessOutcome::Failed;
        }
    };

    let prompt = match settings
        .post_process_prompts
        .iter()
        .find(|prompt| prompt.id == selected_prompt_id)
    {
        Some(prompt) => prompt.prompt.clone(),
        None => {
            debug!(
                "Post-processing skipped because prompt '{}' was not found",
                selected_prompt_id
            );
            return PostProcessOutcome::Failed;
        }
    };

    if prompt.trim().is_empty() {
        debug!("Post-processing skipped because the selected prompt is empty");
        return PostProcessOutcome::Failed;
    }

    let system_prompt = build_system_prompt(&prompt);
    let user_content = transcription.to_string();
    let legacy_prompt = prompt.replace("${output}", transcription);

    run_llm_completion(settings, system_prompt, user_content, legacy_prompt).await
}

/// Matches the frontmost application against enabled [`TransformAppRule`] entries.
/// Exact case-insensitive matches on bundle ID or app name win over substring matches.
fn match_app_rule<'a>(
    rules: &'a [TransformAppRule],
    app: Option<&AppInfo>,
) -> Option<&'a TransformAppRule> {
    let app = app?;
    let bundle_lower = app.bundle_id.trim().to_lowercase();
    let name_lower = app.name.trim().to_lowercase();
    if bundle_lower.is_empty() && name_lower.is_empty() {
        return None;
    }

    // Pass 1: exact case-insensitive match on bundle_id.
    if !bundle_lower.is_empty() {
        if let Some(rule) = rules.iter().filter(|r| r.enabled).find(|rule| {
            let id_lower = rule.app_identifier.trim().to_lowercase();
            !id_lower.is_empty() && id_lower == bundle_lower
        }) {
            return Some(rule);
        }
    }

    // Pass 2: exact case-insensitive match against app name.
    if !name_lower.is_empty() {
        if let Some(rule) = rules.iter().filter(|r| r.enabled).find(|rule| {
            let id_lower = rule.app_identifier.trim().to_lowercase();
            let rule_name_lower = rule.app_name.trim().to_lowercase();
            (!id_lower.is_empty() && id_lower == name_lower)
                || (!rule_name_lower.is_empty() && rule_name_lower == name_lower)
        }) {
            return Some(rule);
        }
    }

    // Pass 3: case-insensitive substring match of rule.app_identifier against bundle_id or name.
    rules.iter().filter(|r| r.enabled).find(|rule| {
        let id_lower = rule.app_identifier.trim().to_lowercase();
        !id_lower.is_empty()
            && ((!bundle_lower.is_empty() && bundle_lower.contains(&id_lower))
                || (!name_lower.is_empty() && name_lower.contains(&id_lower)))
    })
}

fn find_prompt_by_id(settings: &AppSettings, id: &str) -> Option<String> {
    settings
        .transform_prompts
        .iter()
        .chain(settings.post_process_prompts.iter())
        .find(|p| p.id == id && !p.prompt.trim().is_empty())
        .map(|p| p.prompt.clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransformPlan {
    is_command_replace: bool,
    prompt_template: String,
    system_prompt: String,
    user_content: String,
    legacy_prompt: String,
}

fn resolve_transform_plan(
    settings: &AppSettings,
    transcription: &str,
    mode: TransformMode,
    context: &TransformContext,
    prompt_override_id: Option<&str>,
) -> Option<TransformPlan> {
    let is_command_replace = match mode {
        TransformMode::Command => true,
        TransformMode::AutoStyle => false,
        TransformMode::Smart => context.selected_text.is_some(),
    };

    let app_label = context
        .app
        .as_ref()
        .map(|a| a.name.trim())
        .filter(|n| !n.is_empty())
        .unwrap_or("");

    let override_prompt = prompt_override_id.and_then(|id| find_prompt_by_id(settings, id));

    let prompt_template = if let Some(template) = override_prompt {
        template
    } else if is_command_replace {
        settings
            .transform_selected_prompt_id
            .as_deref()
            .and_then(|id| find_prompt_by_id(settings, id))
            .or_else(|| {
                settings
                    .transform_prompts
                    .iter()
                    .find(|p| !p.prompt.trim().is_empty())
                    .map(|p| p.prompt.clone())
            })?
    } else {
        let matched_rule_prompt =
            match_app_rule(&settings.transform_app_rules, context.app.as_ref())
                .and_then(|rule| find_prompt_by_id(settings, &rule.prompt_id));

        matched_rule_prompt
            .or_else(|| {
                settings
                    .transform_default_dictation_prompt_id
                    .as_deref()
                    .and_then(|id| find_prompt_by_id(settings, id))
            })
            .or_else(|| {
                settings
                    .post_process_selected_prompt_id
                    .as_deref()
                    .and_then(|id| find_prompt_by_id(settings, id))
            })
            .or_else(|| {
                settings
                    .post_process_prompts
                    .iter()
                    .find(|p| !p.prompt.trim().is_empty())
                    .map(|p| p.prompt.clone())
            })
            .or_else(|| {
                settings
                    .transform_prompts
                    .iter()
                    .find(|p| !p.prompt.trim().is_empty())
                    .map(|p| p.prompt.clone())
            })?
    };

    if is_command_replace {
        let selected_text = context.selected_text.as_deref()?;
        let has_placeholders = prompt_template.contains("${text}")
            || prompt_template.contains("${instruction}")
            || prompt_template.contains("${output}");

        let with_app = prompt_template.replace("${app}", app_label);
        let user_content = format!(
            "Selected text:\n<selected_text>\n{selected_text}\n</selected_text>\n\nInstruction: {transcription}"
        );

        let (system_prompt, legacy_prompt) = if has_placeholders {
            let sys = with_app
                .replace("${text}", "(provided below)")
                .replace("${instruction}", "(provided below)")
                .replace("${output}", "(provided below)")
                .trim()
                .to_string();
            let leg = with_app
                .replace("${text}", selected_text)
                .replace("${instruction}", transcription)
                .replace("${output}", transcription);
            (sys, leg)
        } else {
            let sys = format!(
                "{}\n\nTransform the text inside <selected_text> strictly according to the spoken instruction. Return only the transformed text.",
                with_app.trim()
            );
            let leg = format!("{sys}\n\n{user_content}");
            (sys, leg)
        };

        Some(TransformPlan {
            is_command_replace: true,
            prompt_template,
            system_prompt,
            user_content,
            legacy_prompt,
        })
    } else {
        let with_app = prompt_template.replace("${app}", app_label);
        let has_placeholders = with_app.contains("${output}")
            || with_app.contains("${text}")
            || with_app.contains("${instruction}");

        let user_content = transcription.to_string();
        let (system_prompt, legacy_prompt) = if has_placeholders {
            let sys = with_app
                .replace("${output}", "")
                .replace("${text}", "")
                .replace("${instruction}", "")
                .trim()
                .to_string();
            let leg = with_app
                .replace("${output}", transcription)
                .replace("${text}", transcription)
                .replace("${instruction}", transcription);
            (sys, leg)
        } else {
            let sys = with_app.trim().to_string();
            let leg = format!("{sys}\n\n<transcript>\n{transcription}\n</transcript>");
            (sys, leg)
        };

        Some(TransformPlan {
            is_command_replace: false,
            prompt_template,
            system_prompt,
            user_content,
            legacy_prompt,
        })
    }
}

fn clean_instant_system_prompt(prompt_template: &str, app_label: &str) -> String {
    let with_app = prompt_template.replace("${app}", app_label);
    let has_placeholders = with_app.contains("${text}")
        || with_app.contains("${instruction}")
        || with_app.contains("${output}");

    let base = if has_placeholders {
        let mut cleaned_lines: Vec<String> = Vec::new();
        for line in with_app.lines() {
            if line.contains("${instruction}") {
                let without = line.replace("${instruction}", "").trim().to_string();
                if without.is_empty()
                    || without.eq_ignore_ascii_case("instruction:")
                    || without.eq_ignore_ascii_case("user instruction:")
                {
                    continue;
                }
                cleaned_lines.push(without);
                continue;
            }
            let without = line
                .replace("${text}", "")
                .replace("${output}", "")
                .trim()
                .to_string();
            if without.is_empty()
                || without.eq_ignore_ascii_case("selected text:")
                || without.eq_ignore_ascii_case("text:")
                || without.eq_ignore_ascii_case("output:")
            {
                continue;
            }
            cleaned_lines.push(without);
        }
        cleaned_lines.join("\n").trim().to_string()
    } else {
        with_app.trim().to_string()
    };

    if base.is_empty() {
        "Transform the provided text inside <selected_text> and return only the transformed text with no explanations.".to_string()
    } else {
        format!(
            "{base}\n\nApply the prompt instructions above directly to the provided text (there is no additional spoken instruction). Return only the transformed text with no explanations."
        )
    }
}

fn resolve_instant_selection_plan(
    settings: &AppSettings,
    context: &TransformContext,
    prompt_id: &str,
) -> Option<TransformPlan> {
    let prompt_template = find_prompt_by_id(settings, prompt_id)?;
    let selected_text = context.selected_text.as_deref()?;
    let app_label = context
        .app
        .as_ref()
        .map(|a| a.name.trim())
        .filter(|n| !n.is_empty())
        .unwrap_or("");
    let system_prompt = clean_instant_system_prompt(&prompt_template, app_label);
    let user_content =
        format!("Selected text:\n<selected_text>\n{selected_text}\n</selected_text>");
    let legacy_prompt = format!("{system_prompt}\n\n{user_content}");

    Some(TransformPlan {
        is_command_replace: true,
        prompt_template,
        system_prompt,
        user_content,
        legacy_prompt,
    })
}

async fn run_transform_llm(settings: &AppSettings, plan: TransformPlan) -> PostProcessOutcome {
    match tokio::time::timeout(
        POST_PROCESS_TIMEOUT,
        run_llm_completion(
            settings,
            plan.system_prompt,
            plan.user_content,
            plan.legacy_prompt,
        ),
    )
    .await
    {
        Ok(PostProcessOutcome::Done(text)) => non_blank_post_processed_text(text)
            .map_or(PostProcessOutcome::Failed, PostProcessOutcome::Done),
        Ok(outcome) => outcome,
        Err(_) => {
            warn!("Transform timed out after {:?}", POST_PROCESS_TIMEOUT);
            PostProcessOutcome::Failed
        }
    }
}

pub(crate) struct ProcessedTranscription {
    pub final_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
    /// `final_text` is the bare transcript because post-processing failed.
    pub post_process_failed: bool,
    /// True when the action transformed selected text using a voice command.
    pub is_command_replace: bool,
    /// True when a command-replace transform failed (raw command must not be pasted).
    pub transform_failed: bool,
}

pub(crate) async fn process_transcription_output(
    app: &AppHandle,
    transcription: &str,
    post_process: bool,
) -> ProcessedTranscription {
    let settings = get_settings(app);
    let mut final_text = transcription.to_string();
    let mut post_processed_text: Option<String> = None;
    let mut post_process_prompt: Option<String> = None;
    let mut post_process_failed = false;

    if post_process {
        // A reply that's blank once cleaned is no better than none.
        let outcome = match tokio::time::timeout(
            POST_PROCESS_TIMEOUT,
            post_process_transcription(&settings, &final_text),
        )
        .await
        {
            Ok(PostProcessOutcome::Done(text)) => non_blank_post_processed_text(text)
                .map_or(PostProcessOutcome::Failed, PostProcessOutcome::Done),
            Ok(outcome) => outcome,
            Err(_) => {
                warn!(
                    "Post-processing timed out after {:?}; pasting the original transcription",
                    POST_PROCESS_TIMEOUT
                );
                PostProcessOutcome::Failed
            }
        };
        match outcome {
            PostProcessOutcome::Skipped => {}
            PostProcessOutcome::Failed => post_process_failed = true,
            PostProcessOutcome::Done(processed_text) => {
                post_processed_text = Some(processed_text.clone());
                final_text = processed_text;

                if let Some(prompt_id) = &settings.post_process_selected_prompt_id {
                    if let Some(prompt) = settings
                        .post_process_prompts
                        .iter()
                        .find(|prompt| &prompt.id == prompt_id)
                    {
                        post_process_prompt = Some(prompt.prompt.clone());
                    }
                }
            }
        }
    }

    ProcessedTranscription {
        final_text,
        post_processed_text,
        post_process_prompt,
        post_process_failed,
        is_command_replace: false,
        transform_failed: false,
    }
}

async fn process_transform_output(
    app: &AppHandle,
    transcription: &str,
    session: Option<&CapturedTransformSession>,
) -> ProcessedTranscription {
    let settings = get_settings(app);
    let fallback_context = TransformContext::default();
    let (mode, context) = match session {
        Some(s) => (s.mode, &s.context),
        None => (settings.transform_mode, &fallback_context),
    };

    if is_blank_transcription(transcription) {
        debug!("Transform skipped because the transcription is empty");
        return ProcessedTranscription {
            final_text: String::new(),
            post_processed_text: None,
            post_process_prompt: None,
            post_process_failed: false,
            is_command_replace: mode == TransformMode::Command
                || (mode == TransformMode::Smart && context.selected_text.is_some()),
            transform_failed: false,
        };
    }

    let prompt_override_id = session.and_then(|s| s.prompt_override_id.as_deref());
    let Some(plan) =
        resolve_transform_plan(&settings, transcription, mode, context, prompt_override_id)
    else {
        let is_command_replace = mode == TransformMode::Command
            || (mode == TransformMode::Smart && context.selected_text.is_some());
        warn!("Could not resolve transform plan (missing prompt or selected text)");
        return ProcessedTranscription {
            final_text: transcription.to_string(),
            post_processed_text: None,
            post_process_prompt: None,
            post_process_failed: !is_command_replace,
            is_command_replace,
            transform_failed: is_command_replace,
        };
    };

    let is_command_replace = plan.is_command_replace;
    let prompt_template = plan.prompt_template.clone();

    let outcome = run_transform_llm(&settings, plan).await;

    match outcome {
        PostProcessOutcome::Skipped => ProcessedTranscription {
            final_text: String::new(),
            post_processed_text: None,
            post_process_prompt: None,
            post_process_failed: false,
            is_command_replace,
            transform_failed: false,
        },
        PostProcessOutcome::Failed => ProcessedTranscription {
            final_text: transcription.to_string(),
            post_processed_text: None,
            post_process_prompt: Some(prompt_template),
            post_process_failed: !is_command_replace,
            is_command_replace,
            transform_failed: is_command_replace,
        },
        PostProcessOutcome::Done(processed_text) => ProcessedTranscription {
            final_text: processed_text.clone(),
            post_processed_text: Some(processed_text),
            post_process_prompt: Some(prompt_template),
            post_process_failed: false,
            is_command_replace,
            transform_failed: false,
        },
    }
}

struct PasteJob {
    ah: AppHandle,
    final_text: String,
    post_process_failed: bool,
    is_command_replace: bool,
    rm: Arc<AudioRecordingManager>,
    cancel_generation: u64,
    session: u64,
    sample_count: usize,
    stop_time: Instant,
    instant_guard: Option<InstantTransformGuard>,
}

fn spawn_paste_and_finish(job: PasteJob) {
    let PasteJob {
        ah,
        final_text,
        post_process_failed,
        is_command_replace,
        rm,
        cancel_generation,
        session,
        sample_count,
        stop_time,
        instant_guard,
    } = job;
    let ah_clone = ah.clone();
    let paste_time = Instant::now();
    // Off the main thread: paste sleeps between steps, and a
    // paste into Handy's own windows needs the main thread free.
    std::thread::Builder::new()
        .name("paste".into())
        .spawn(move || {
            let _instant_guard = instant_guard;
            if rm.was_cancelled_since(cancel_generation) {
                debug!("Transcription operation cancelled before paste");
                utils::hide_recording_overlay(&ah_clone);
                set_tray_state(&ah_clone, TrayIconState::Idle);
                return;
            }

            // A new session may have taken over the UI while this paste ran.
            let owns_ui = || RECORDING_SESSION.load(Ordering::SeqCst) == session;
            let paste_res = if is_command_replace {
                utils::paste_with_options(
                    final_text,
                    ah_clone.clone(),
                    PasteOptions {
                        allow_trailing_space: false,
                        allow_auto_submit: false,
                    },
                )
            } else {
                utils::paste(final_text, ah_clone.clone())
            };
            let (outcome, notice) = match paste_res {
                Ok(utils::PasteOutcome::Pasted) => {
                    debug!("Text pasted successfully in {:?}", paste_time.elapsed());
                    // Leave a newer session's notice alone.
                    if owns_ui() {
                        notices::clear(&ah_clone, NoticeSource::Dictation);
                    }
                    let notice = post_process_failed.then(|| {
                        Notice::new(NoticeCode::PostProcessFailed, NoticeSource::Dictation)
                    });
                    (TranscriptionOutcome::Typed, notice)
                }
                Ok(utils::PasteOutcome::Blocked) => (
                    TranscriptionOutcome::Failed,
                    Some(Notice::new(
                        NoticeCode::PasteBlockedElevated,
                        NoticeSource::Dictation,
                    )),
                ),
                Err(e) => {
                    error!("Failed to paste transcription: {}", e);
                    (
                        TranscriptionOutcome::Failed,
                        Some(Notice::new(
                            NoticeCode::PasteFailed,
                            NoticeSource::Dictation,
                        )),
                    )
                }
            };
            match (owns_ui(), notice) {
                (true, Some(notice)) => notices::raise(&ah_clone, notice),
                (true, None) => utils::hide_recording_overlay(&ah_clone),
                (false, Some(notice)) => notices::raise_without_overlay(&ah_clone, notice),
                (false, None) => {}
            }
            if owns_ui() {
                set_tray_state(&ah_clone, TrayIconState::Idle);
            }
            emit_transcription_complete(&ah_clone, outcome, sample_count, stop_time);
        })
        .map(|_| ())
        .unwrap_or_else(|e| {
            error!("Failed to start the paste thread: {:?}", e);
            utils::hide_recording_overlay(&ah);
            set_tray_state(&ah, TrayIconState::Idle);
            emit_transcription_complete(&ah, TranscriptionOutcome::Failed, sample_count, stop_time);
        });
}

fn start_instant_selection_transform(
    app: &AppHandle,
    settings: &AppSettings,
    rm: &Arc<AudioRecordingManager>,
    context: &TransformContext,
    prompt_id: &str,
    start_time: Instant,
) {
    let Some(plan) = resolve_instant_selection_plan(settings, context, prompt_id) else {
        warn!(
            "Could not resolve instant selection transform plan for '{}'",
            prompt_id
        );
        notices::raise(
            app,
            Notice::new(NoticeCode::TransformFailed, NoticeSource::Dictation),
        );
        return;
    };

    let session_id = RECORDING_SESSION.fetch_add(1, Ordering::SeqCst) + 1;
    let cancel_generation = rm.cancel_generation();
    INSTANT_TRANSFORM_SEQ.fetch_add(1, Ordering::SeqCst);
    INSTANT_TRANSFORM_ACTIVE.store(true, Ordering::SeqCst);

    set_tray_state(app, TrayIconState::Transcribing);
    show_processing_overlay(app);
    shortcut::register_cancel_shortcut(app);

    let ah = app.clone();
    let rm = Arc::clone(rm);
    let settings_snapshot = settings.clone();
    tauri::async_runtime::spawn(async move {
        let guard = InstantTransformGuard(ah.clone());
        let outcome_opt =
            complete_unless_cancelled(run_transform_llm(&settings_snapshot, plan), || {
                rm.was_cancelled_since(cancel_generation)
            })
            .await;

        let Some(outcome) = outcome_opt else {
            debug!("Instant selection transform cancelled during LLM completion");
            utils::hide_recording_overlay(&ah);
            set_tray_state(&ah, TrayIconState::Idle);
            return;
        };

        match outcome {
            PostProcessOutcome::Skipped => {
                utils::hide_recording_overlay(&ah);
                set_tray_state(&ah, TrayIconState::Idle);
            }
            PostProcessOutcome::Failed => {
                notices::raise(
                    &ah,
                    Notice::new(NoticeCode::TransformFailed, NoticeSource::Dictation),
                );
                set_tray_state(&ah, TrayIconState::Idle);
                emit_transcription_complete(&ah, TranscriptionOutcome::Failed, 0, start_time);
            }
            PostProcessOutcome::Done(processed_text) => {
                let Some(final_text) = complete_unless_cancelled(
                    crate::transcription_hook::apply(&ah, processed_text),
                    || rm.was_cancelled_since(cancel_generation),
                )
                .await
                else {
                    debug!("Instant selection transform cancelled during hook");
                    utils::hide_recording_overlay(&ah);
                    set_tray_state(&ah, TrayIconState::Idle);
                    return;
                };

                if final_text.is_empty() {
                    finish_without_text(&ah, false, 0, start_time);
                    return;
                }

                spawn_paste_and_finish(PasteJob {
                    ah,
                    final_text,
                    post_process_failed: false,
                    is_command_replace: true,
                    rm,
                    cancel_generation,
                    session: session_id,
                    sample_count: 0,
                    stop_time: start_time,
                    instant_guard: Some(guard),
                });
            }
        }
    });
}

impl ShortcutAction for TranscribeAction {
    fn start(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str) {
        let start_time = Instant::now();
        debug!("TranscribeAction::start called for binding: {}", binding_id);

        let settings = get_settings(app);
        let tm = app.state::<Arc<TranscriptionManager>>();
        let rm = app.state::<Arc<AudioRecordingManager>>();

        let captured_transform = if self.kind == ActionKind::Transform {
            let prompt_override_id =
                crate::settings::transform_prompt_id_from_binding(binding_id).map(str::to_string);
            let selection_behavior = prompt_override_id
                .as_deref()
                .map(|id| settings.transform_selection_behavior_for_prompt(id));
            let capture_selection = matches!(
                settings.transform_mode,
                TransformMode::Command | TransformMode::Smart
            ) || selection_behavior
                == Some(TransformSelectionBehavior::Instant);
            let context = context::capture_transform_context(
                app,
                capture_selection,
                settings.transform_clipboard_fallback,
            );
            if context.selected_text.is_some()
                && selection_behavior == Some(TransformSelectionBehavior::Instant)
            {
                if let Some(prompt_id) = prompt_override_id.as_deref() {
                    debug!(
                        "Starting instant selection transform for prompt '{}' (binding '{}')",
                        prompt_id, binding_id
                    );
                    start_instant_selection_transform(
                        app, &settings, &rm, &context, prompt_id, start_time,
                    );
                    return;
                }
            }
            if settings.transform_mode == TransformMode::Command && context.selected_text.is_none()
            {
                warn!("Transform Command mode requested with no selected text");
                notices::raise(
                    app,
                    Notice::new(NoticeCode::NoSelectedText, NoticeSource::Dictation),
                );
                return;
            }
            Some((settings.transform_mode, context, prompt_override_id))
        } else {
            None
        };

        // Load ASR model and VAD model in parallel
        let kickoff_started = Instant::now();
        // An empty selection has nothing to load; the check below says so.
        let selected_model = &settings.selected_model;
        let load_started = !selected_model.is_empty() && tm.initiate_model_load(true);
        let rm_clone = Arc::clone(&rm);
        std::thread::spawn(move || {
            if let Err(e) = rm_clone.preload_vad() {
                debug!("VAD pre-load failed: {}", e);
            }
        });
        let kickoff_elapsed = kickoff_started.elapsed();

        // Don't open the mic if nothing can transcribe the recording. A load
        // started above reports its own failure; one already running doesn't.
        if !tm.is_model_loaded() {
            if selected_model.is_empty() {
                warn!("Not starting recording: no model is selected");
                tm.raise_no_model_selected();
                return;
            }
            if let Err(e) = app
                .state::<Arc<ModelManager>>()
                .get_model_path(selected_model)
            {
                warn!("Not starting recording: no model can transcribe it ({})", e);
                if !load_started {
                    tm.raise_model_load_failed(selected_model, &e);
                }
                return;
            }
        }

        let binding_id = binding_id.to_string();
        let session_id = RECORDING_SESSION.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some((mode, context, prompt_override_id)) = captured_transform {
            if let Ok(mut slot) = ACTIVE_TRANSFORM_SESSION.lock() {
                *slot = Some(CapturedTransformSession {
                    session_id,
                    mode,
                    context,
                    prompt_override_id,
                });
            }
        }
        let tray_started = Instant::now();
        set_tray_state(app, TrayIconState::Recording);
        let tray_elapsed = tray_started.elapsed();

        // Get the microphone mode to determine audio feedback timing
        let plan_started = Instant::now();
        let is_always_on = settings.always_on_microphone;

        let selected_model_info = app
            .state::<Arc<ModelManager>>()
            .get_model_info(&settings.selected_model);

        // Use the app-facing model capability as the single pre-recording source
        // for live streaming decisions. Unknown support is represented as false
        // until the model registry is updated by discovery or runtime load.
        let model_supports_streaming = selected_model_info
            .as_ref()
            .map(|m| m.supports_streaming)
            .unwrap_or(false);
        let vad_policy = if !settings.vad_enabled {
            VadPolicy::Disabled
        } else if model_supports_streaming {
            VadPolicy::Streaming
        } else {
            VadPolicy::Offline
        };
        // Before the model loads, whether it keeps its accuracy on long audio
        // is unknown, so assume not: the worker decides again with the loaded
        // model, and a wrong guess costs only an idle worker.
        let expected_model = ModelTraits {
            streaming: model_supports_streaming,
            long_form: false,
        };
        if live_decode(expected_model, settings.long_form_chunking) != LiveDecode::None {
            tm.start_stream();
        }
        let plan_elapsed = plan_started.elapsed();

        // Sizing the overlay follows the same advertised capability. A model that
        // doesn't stream (or whose capability is not known yet) gets the compact
        // pill instead of an oversized transparent live window.
        let overlay_started = Instant::now();
        match settings.overlay_style {
            OverlayStyle::Live if model_supports_streaming => utils::show_streaming_overlay(app),
            OverlayStyle::Live | OverlayStyle::Minimal => show_recording_overlay(app),
            OverlayStyle::None => {} // show_overlay_state no-ops on None anyway
        }
        // Everything above runs before capture can begin, so each span here is
        // added keypress->capture latency.
        debug!(
            "start-path pre-recording steps: model_kickoff={:?} tray={:?} settings+stream_plan={:?} overlay={:?}",
            kickoff_elapsed,
            tray_elapsed,
            plan_elapsed,
            overlay_started.elapsed()
        );
        debug!("Microphone mode - always_on: {}", is_always_on);

        let mut recording_error: Option<anyhow::Error> = None;
        let recording_start_time = Instant::now();
        match rm.try_start_recording(&binding_id, vad_policy) {
            Ok(readiness) => {
                debug!(
                    "Recording request accepted in {:?}; waiting for first microphone samples",
                    recording_start_time.elapsed()
                );
                let generation = readiness.generation();
                let app_clone = app.clone();
                let rm_clone = Arc::clone(&rm);
                std::thread::spawn(move || {
                    if !readiness.wait() {
                        debug!("Microphone readiness wait ended without receiving samples");
                        return;
                    }

                    // Development-only preview hook for evaluating the brief
                    // arming animation on hardware that normally starts too fast
                    // to make it visible.
                    #[cfg(debug_assertions)]
                    if let Ok(delay_ms) = std::env::var("HANDY_DEBUG_MIC_READY_DELAY_MS")
                        .unwrap_or_default()
                        .parse::<u64>()
                    {
                        let delay_ms = delay_ms.min(10_000);
                        if delay_ms > 0 {
                            debug!("Delaying microphone-ready cue by {delay_ms}ms for UI preview");
                            std::thread::sleep(Duration::from_millis(delay_ms));
                        }
                    }

                    if !rm_clone.is_recording_readiness_current(generation) {
                        debug!("Microphone became ready for an inactive recording");
                        return;
                    }

                    debug!("Microphone is receiving samples; recording is ready");
                    utils::emit_recording_ready(&app_clone);

                    // The start chime is a readiness cue, so it must follow the
                    // first real input callback rather than Stream::play() or a
                    // fixed delay. The helper returns immediately when feedback
                    // is disabled; mute still follows the same readiness point.
                    if rm_clone.is_recording_readiness_current(generation) {
                        play_feedback_sound_blocking(&app_clone, SoundType::Start);
                    }
                    if rm_clone.is_recording_readiness_current(generation) {
                        rm_clone.apply_mute();
                    }
                });
            }
            Err(e) => {
                debug!("Failed to start recording: {}", e);
                recording_error = Some(e);
            }
        }

        if recording_error.is_none() {
            // Dynamically register the cancel shortcut in a separate task to avoid deadlock
            shortcut::register_cancel_shortcut(app);
        } else {
            // Starting failed (for example due to blocked microphone permissions).
            // Revert UI state so we don't stay stuck in the recording overlay.
            if self.kind == ActionKind::Transform {
                if let Ok(mut slot) = ACTIVE_TRANSFORM_SESSION.lock() {
                    if slot.as_ref().is_some_and(|s| s.session_id == session_id) {
                        *slot = None;
                    }
                }
            }
            tm.cancel_stream();
            set_tray_state(app, TrayIconState::Idle);
            if let Some(err) = recording_error {
                notices::raise(
                    app,
                    Notice::new(recording_notice_code(&err), NoticeSource::Dictation)
                        .with_detail(format!("{err:#}")),
                );
            }
        }

        debug!(
            "TranscribeAction::start completed in {:?}",
            start_time.elapsed()
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str) {
        // Prevent a slow microphone from emitting a ready event or start chime
        // after the user has already requested stop.
        app.state::<Arc<AudioRecordingManager>>()
            .invalidate_recording_readiness();

        // Unregister the cancel shortcut when transcription stops
        shortcut::unregister_cancel_shortcut(app);

        let stop_time = Instant::now();
        debug!("TranscribeAction::stop called for binding: {}", binding_id);

        let ah = app.clone();
        let rm = Arc::clone(&app.state::<Arc<AudioRecordingManager>>());
        let tm = Arc::clone(&app.state::<Arc<TranscriptionManager>>());
        let hm = Arc::clone(&app.state::<Arc<HistoryManager>>());

        set_tray_state(app, TrayIconState::Transcribing);
        // Stop should give immediate visual feedback. Live streaming can keep
        // the larger panel, but it still switches from listening to a working
        // spinner while the stream finalizes. Non-streaming paths use the
        // compact transcribing pill (None no-ops in show_*).
        let style = get_settings(app).overlay_style;
        // Capture this before finalizing the stream so every later working state
        // targets the same overlay that was shown for this transcription.
        let use_streaming_overlay = should_use_streaming_overlay(style, tm.is_streaming());
        if use_streaming_overlay {
            tm.emit_stream_working(StreamWorkKind::Transcribing);
        } else {
            show_transcribing_overlay(app);
        }

        // Unmute before playing audio feedback so the stop sound is audible
        rm.remove_mute();

        // Play audio feedback for recording stop
        play_feedback_sound(app, SoundType::Stop);

        let binding_id = binding_id.to_string(); // Clone binding_id for the async task
        let kind = self.kind;
        let transform_session = if kind == ActionKind::Transform {
            ACTIVE_TRANSFORM_SESSION
                .lock()
                .ok()
                .and_then(|mut slot| slot.take())
        } else {
            None
        };
        let cancel_generation = rm.cancel_generation();

        tauri::async_runtime::spawn(async move {
            let _guard = FinishGuard(ah.clone(), Arc::clone(&tm));
            debug!(
                "Starting async transcription task for binding: {}",
                binding_id
            );

            let stop_recording_time = Instant::now();
            if let Some(samples) = rm.stop_recording(&binding_id, cancel_generation) {
                // Read now: by the time transcribing ends, another take may
                // have replaced it.
                let input_silent = rm.last_input_silent();
                debug!(
                    "Recording stopped and samples retrieved in {:?}, sample count: {}",
                    stop_recording_time.elapsed(),
                    samples.len()
                );

                if rm.was_cancelled_since(cancel_generation) {
                    debug!("Transcription operation cancelled after recording stop");
                    tm.cancel_stream();
                    utils::hide_recording_overlay(&ah);
                    set_tray_state(&ah, TrayIconState::Idle);
                    return;
                }

                if samples.is_empty() {
                    debug!("Recording produced no audio samples; skipping persistence");
                    // Tear down any streaming worker so its channel doesn't leak
                    // and block the next start_stream.
                    tm.cancel_stream();
                    finish_without_text(&ah, input_silent, 0, stop_time);
                } else {
                    // Save WAV concurrently with transcription
                    let sample_count = samples.len();
                    let file_name = format!("handy-{}.wav", chrono::Utc::now().timestamp());
                    let wav_path = hm.recordings_dir().join(&file_name);
                    let wav_path_for_verify = wav_path.clone();
                    let samples_for_wav = samples.clone();
                    let wav_handle = tauri::async_runtime::spawn_blocking(move || {
                        crate::audio_toolkit::save_wav_file(&wav_path, &samples_for_wav)
                    });

                    // Transcribe concurrently with WAV save: the live decode's
                    // result when it has one (all audio was already fed to it),
                    // otherwise a batch run of the samples. A cancel is surfaced
                    // as an error, so it never starts a batch run.
                    let transcription_time = Instant::now();
                    let transcription_result = tm.transcribe_recording(samples);

                    // Await WAV save and verify
                    let wav_saved = match wav_handle.await {
                        Ok(Ok(())) => {
                            match crate::audio_toolkit::verify_wav_file(
                                &wav_path_for_verify,
                                sample_count,
                            ) {
                                Ok(()) => true,
                                Err(e) => {
                                    error!("WAV verification failed: {}", e);
                                    false
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            error!("Failed to save WAV file: {}", e);
                            false
                        }
                        Err(e) => {
                            error!("WAV save task panicked: {}", e);
                            false
                        }
                    };

                    if rm.was_cancelled_since(cancel_generation) {
                        debug!("Transcription operation cancelled before output handling");
                        utils::hide_recording_overlay(&ah);
                        set_tray_state(&ah, TrayIconState::Idle);
                        return;
                    }

                    match transcription_result {
                        Ok(Transcription {
                            text: transcription,
                            language,
                        }) => {
                            debug!(
                                "Transcription completed in {:?}: '{}'",
                                transcription_time.elapsed(),
                                utils::redact_text(&transcription)
                            );

                            let uses_llm = kind != ActionKind::Transcribe;
                            if uses_llm {
                                if use_streaming_overlay {
                                    tm.emit_stream_working(StreamWorkKind::Polishing);
                                } else {
                                    show_processing_overlay(&ah);
                                }
                            }
                            let processed_opt = match kind {
                                ActionKind::Transcribe => {
                                    complete_unless_cancelled(
                                        process_transcription_output(&ah, &transcription, false),
                                        || rm.was_cancelled_since(cancel_generation),
                                    )
                                    .await
                                }
                                ActionKind::PostProcess => {
                                    complete_unless_cancelled(
                                        process_transcription_output(&ah, &transcription, true),
                                        || rm.was_cancelled_since(cancel_generation),
                                    )
                                    .await
                                }
                                ActionKind::Transform => {
                                    complete_unless_cancelled(
                                        process_transform_output(
                                            &ah,
                                            &transcription,
                                            transform_session.as_ref(),
                                        ),
                                        || rm.was_cancelled_since(cancel_generation),
                                    )
                                    .await
                                }
                            };
                            let Some(processed) = processed_opt else {
                                debug!("Transcription operation cancelled during output handling");
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                return;
                            };

                            if rm.was_cancelled_since(cancel_generation) {
                                debug!("Transcription operation cancelled before paste");
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                return;
                            }

                            // Save to history if WAV was saved
                            if wav_saved {
                                if let Err(err) = hm.save_entry(
                                    file_name,
                                    transcription,
                                    uses_llm,
                                    processed.post_processed_text.clone(),
                                    processed.post_process_prompt.clone(),
                                    language,
                                ) {
                                    error!("Failed to save history entry: {}", err);
                                }
                            }

                            // In Command replace mode, never overwrite the user's selected
                            // text with the raw spoken instruction when transformation fails.
                            if processed.transform_failed {
                                notices::raise(
                                    &ah,
                                    Notice::new(
                                        NoticeCode::TransformFailed,
                                        NoticeSource::Dictation,
                                    ),
                                );
                                set_tray_state(&ah, TrayIconState::Idle);
                                emit_transcription_complete(
                                    &ah,
                                    TranscriptionOutcome::Failed,
                                    sample_count,
                                    stop_time,
                                );
                                return;
                            }

                            // The user's hook only changes what gets pasted;
                            // history above keeps the text as produced.
                            let Some(final_text) = complete_unless_cancelled(
                                crate::transcription_hook::apply(&ah, processed.final_text),
                                || rm.was_cancelled_since(cancel_generation),
                            )
                            .await
                            else {
                                debug!("Transcription operation cancelled during hook");
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                return;
                            };

                            if final_text.is_empty() {
                                finish_without_text(&ah, input_silent, sample_count, stop_time);
                            } else {
                                // Taken before the paste thread exists, and so
                                // before the next session can start.
                                let session = RECORDING_SESSION.load(Ordering::SeqCst);
                                spawn_paste_and_finish(PasteJob {
                                    ah,
                                    final_text,
                                    post_process_failed: processed.post_process_failed,
                                    is_command_replace: processed.is_command_replace,
                                    rm,
                                    cancel_generation,
                                    session,
                                    sample_count,
                                    stop_time,
                                    instant_guard: None,
                                });
                            }
                        }
                        Err(err) => {
                            if rm.was_cancelled_since(cancel_generation) {
                                debug!(
                                    "Transcription operation cancelled after transcription error"
                                );
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                return;
                            }

                            error!("Transcription failed: {}", err);
                            notices::raise(
                                &ah,
                                Notice::new(
                                    NoticeCode::TranscriptionFailed,
                                    NoticeSource::Dictation,
                                )
                                .with_detail(err.to_string()),
                            );
                            // Save entry with empty text so user can retry
                            if wav_saved {
                                if let Err(save_err) = hm.save_entry(
                                    file_name,
                                    String::new(),
                                    kind != ActionKind::Transcribe,
                                    None,
                                    None,
                                    None,
                                ) {
                                    error!("Failed to save failed history entry: {}", save_err);
                                }
                            }
                            set_tray_state(&ah, TrayIconState::Idle);
                            emit_transcription_complete(
                                &ah,
                                TranscriptionOutcome::Failed,
                                sample_count,
                                stop_time,
                            );
                        }
                    }
                }
            } else {
                debug!("No samples retrieved from recording stop");
                // Tear down any streaming worker so its channel doesn't leak.
                tm.cancel_stream();
                utils::hide_recording_overlay(&ah);
                set_tray_state(&ah, TrayIconState::Idle);
                if !rm.was_cancelled_since(cancel_generation) {
                    emit_transcription_complete(&ah, TranscriptionOutcome::Empty, 0, stop_time);
                }
            }
        });

        debug!(
            "TranscribeAction::stop completed in {:?}",
            stop_time.elapsed()
        );
    }
}

// Cancel Action
struct CancelAction;

impl ShortcutAction for CancelAction {
    fn start(&self, app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        utils::cancel_current_operation(app);
    }

    fn stop(&self, _app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        // Nothing to do on stop for cancel
    }
}

// Test Action
struct TestAction;

impl ShortcutAction for TestAction {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Started - {} (App: {})", // Changed "Pressed" to "Started" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Stopped - {} (App: {})", // Changed "Released" to "Stopped" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }
}

// Static Action Map
pub static ACTION_MAP: Lazy<HashMap<String, Arc<dyn ShortcutAction>>> = Lazy::new(|| {
    let mut map = HashMap::new();
    map.insert(
        "transcribe".to_string(),
        Arc::new(TranscribeAction {
            kind: ActionKind::Transcribe,
        }) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "transcribe_with_post_process".to_string(),
        Arc::new(TranscribeAction {
            kind: ActionKind::PostProcess,
        }) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "transform_text".to_string(),
        Arc::new(TranscribeAction {
            kind: ActionKind::Transform,
        }) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "cancel".to_string(),
        Arc::new(CancelAction) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "test".to_string(),
        Arc::new(TestAction) as Arc<dyn ShortcutAction>,
    );
    map
});

/// Look up the action handler for a static binding ID or a dynamic per-prompt
/// transform binding (`transform_prompt:<prompt_id>`).
pub fn get_action(binding_id: &str) -> Option<&'static Arc<dyn ShortcutAction>> {
    ACTION_MAP.get(binding_id).or_else(|| {
        crate::settings::transform_prompt_id_from_binding(binding_id)
            .and_then(|_| ACTION_MAP.get("transform_text"))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        complete_unless_cancelled, get_action, is_blank_transcription, match_app_rule,
        non_blank_post_processed_text, post_process_transcription, resolve_transform_plan,
        run_llm_completion, should_use_streaming_overlay, strip_invisible_chars, strip_think_block,
        PostProcessOutcome,
    };
    use crate::context::{AppInfo, TransformContext};
    use crate::settings::{
        get_default_settings, AppSettings, LLMPrompt, OverlayStyle, TransformAppRule, TransformMode,
    };
    use std::future;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    /// Settings that run unstructured post-processing against `base_url`.
    fn post_process_settings(base_url: &str) -> AppSettings {
        let mut settings = get_default_settings();
        let provider = settings
            .post_process_providers
            .iter_mut()
            .find(|provider| provider.id == settings.post_process_provider_id)
            .expect("the default provider exists");
        provider.base_url = base_url.to_string();
        provider.supports_structured_output = false;
        let provider_id = provider.id.clone();
        settings
            .post_process_models
            .insert(provider_id, "model".to_string());
        settings.post_process_selected_prompt_id =
            settings.post_process_prompts.first().map(|p| p.id.clone());
        settings
    }

    /// Answers one chat completion request with `body`, and returns its URL.
    fn serve_once(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 8192];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        format!("http://{address}")
    }

    fn run_post_process(settings: &AppSettings, transcription: &str) -> PostProcessOutcome {
        tauri::async_runtime::block_on(post_process_transcription(settings, transcription))
    }

    #[test]
    fn blank_speech_is_skipped_but_missing_setup_failed() {
        let settings = post_process_settings("http://127.0.0.1:1");
        assert_eq!(
            run_post_process(&settings, "  "),
            PostProcessOutcome::Skipped
        );

        let mut settings = post_process_settings("http://127.0.0.1:1");
        settings.post_process_models.clear();
        assert_eq!(
            run_post_process(&settings, "hello"),
            PostProcessOutcome::Failed
        );
    }

    #[test]
    fn a_post_processing_reply_is_done() {
        let url = serve_once(r#"{"choices":[{"message":{"content":"polished"}}]}"#);
        assert_eq!(
            run_post_process(&post_process_settings(&url), "hello"),
            PostProcessOutcome::Done("polished".to_string())
        );
    }

    #[test]
    fn a_post_processing_reply_with_no_content_failed() {
        let url = serve_once(r#"{"choices":[]}"#);
        assert_eq!(
            run_post_process(&post_process_settings(&url), "hello"),
            PostProcessOutcome::Failed
        );
    }

    #[test]
    fn blank_transcription_is_detected() {
        assert!(is_blank_transcription(""));
        assert!(is_blank_transcription("   "));
        assert!(is_blank_transcription("\t\n  \r\n"));
    }

    #[test]
    fn non_blank_transcription_is_kept() {
        assert!(!is_blank_transcription("hello"));
        assert!(!is_blank_transcription("  hello  "));
    }

    #[test]
    fn blank_post_processed_text_is_rejected() {
        for text in ["", "   ", "\t\n\r\n", "\u{00A0}\u{2003}"] {
            assert_eq!(non_blank_post_processed_text(text.to_string()), None);
        }
    }

    #[test]
    fn cleaned_empty_post_processed_text_is_rejected() {
        for text in [
            "\u{200B}\u{200C}\u{200D}\u{FEFF}",
            "<think>reasoning only</think>",
            "<think>reasoning only</think>\n\u{200B} ",
        ] {
            let text = strip_invisible_chars(strip_think_block(text));
            assert_eq!(non_blank_post_processed_text(text), None);
        }
    }

    #[test]
    fn non_blank_post_processed_text_is_preserved() {
        for text in ["Cleaned text.", "  정리된 문장입니다.\n", "0"] {
            assert_eq!(
                non_blank_post_processed_text(text.to_string()),
                Some(text.to_string())
            );
        }
    }

    #[test]
    fn completed_operation_returns_its_output() {
        let result = tauri::async_runtime::block_on(complete_unless_cancelled(
            future::ready("done"),
            || false,
        ));

        assert_eq!(result, Some("done"));
    }

    #[test]
    fn pending_operation_stops_after_cancellation() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_thread = Arc::clone(&cancelled);
        let cancel_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            cancelled_for_thread.store(true, Ordering::Release);
        });

        let result = tauri::async_runtime::block_on(complete_unless_cancelled(
            future::pending::<()>(),
            || cancelled.load(Ordering::Acquire),
        ));

        cancel_thread.join().unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn leading_think_block_is_stripped() {
        assert_eq!(
            strip_think_block("<think>pondering...</think>Cleaned text."),
            "Cleaned text."
        );
        assert_eq!(
            strip_think_block("  \n<think>multi\nline</think>\n  Cleaned text."),
            "Cleaned text."
        );
    }

    #[test]
    fn content_without_think_block_is_unchanged() {
        assert_eq!(strip_think_block("Cleaned text."), "Cleaned text.");
        assert_eq!(
            strip_think_block("Mentions <think> mid-sentence."),
            "Mentions <think> mid-sentence."
        );
        // Unclosed block: leave untouched rather than guess
        assert_eq!(
            strip_think_block("<think>never closed"),
            "<think>never closed"
        );
    }

    #[test]
    fn live_overlay_uses_streaming_states_only_for_streaming_models() {
        assert!(should_use_streaming_overlay(OverlayStyle::Live, true));
        assert!(!should_use_streaming_overlay(OverlayStyle::Live, false));
        assert!(!should_use_streaming_overlay(OverlayStyle::Minimal, true));
        assert!(!should_use_streaming_overlay(OverlayStyle::None, true));
    }

    #[test]
    fn match_app_rule_prefers_exact_over_substring_and_skips_disabled() {
        let rules = vec![
            TransformAppRule {
                id: "disabled".to_string(),
                app_name: "Slack".to_string(),
                app_identifier: "com.tinyspeck.slackmacgap".to_string(),
                prompt_id: "p_disabled".to_string(),
                enabled: false,
            },
            TransformAppRule {
                id: "substr".to_string(),
                app_name: "Slack Sub".to_string(),
                app_identifier: "tinyspeck".to_string(),
                prompt_id: "p_substr".to_string(),
                enabled: true,
            },
            TransformAppRule {
                id: "exact".to_string(),
                app_name: "Slack".to_string(),
                app_identifier: "com.tinyspeck.slackmacgap".to_string(),
                prompt_id: "p_exact".to_string(),
                enabled: true,
            },
        ];

        let slack = AppInfo {
            name: "Slack".to_string(),
            bundle_id: "com.tinyspeck.slackmacgap".to_string(),
        };
        assert_eq!(
            match_app_rule(&rules, Some(&slack)).map(|r| r.id.as_str()),
            Some("exact")
        );

        let partial_app = AppInfo {
            name: "My Client".to_string(),
            bundle_id: "com.tinyspeck.beta".to_string(),
        };
        assert_eq!(
            match_app_rule(&rules, Some(&partial_app)).map(|r| r.id.as_str()),
            Some("substr")
        );

        let other = AppInfo {
            name: "Finder".to_string(),
            bundle_id: "com.apple.finder".to_string(),
        };
        assert_eq!(match_app_rule(&rules, Some(&other)), None);
        assert_eq!(match_app_rule(&rules, None), None);
    }

    #[test]
    fn resolve_transform_plan_handles_all_three_modes_and_placeholders() {
        let mut settings = get_default_settings();
        settings.post_process_selected_prompt_id =
            Some("default_improve_transcriptions".to_string());

        let mail_ctx = TransformContext {
            app: Some(AppInfo {
                name: "Mail".to_string(),
                bundle_id: "com.apple.mail".to_string(),
            }),
            selected_text: Some("hey send me the deck".to_string()),
            pid: Some(123),
        };

        // Smart mode with selection -> command replace
        let smart_with_sel = resolve_transform_plan(
            &settings,
            "make it formal",
            TransformMode::Smart,
            &mail_ctx,
            None,
        )
        .unwrap();
        assert!(smart_with_sel.is_command_replace);
        assert!(smart_with_sel
            .legacy_prompt
            .contains("hey send me the deck"));
        assert!(smart_with_sel.legacy_prompt.contains("make it formal"));

        // Smart mode without selection -> auto-style dictation using Mail rule
        let mail_no_sel = TransformContext {
            selected_text: None,
            ..mail_ctx.clone()
        };
        let smart_no_sel = resolve_transform_plan(
            &settings,
            "hi bob",
            TransformMode::Smart,
            &mail_no_sel,
            None,
        )
        .unwrap();
        assert!(!smart_no_sel.is_command_replace);
        assert!(smart_no_sel
            .prompt_template
            .contains("professional message or email"));
        assert_eq!(smart_no_sel.user_content, "hi bob");

        // AutoStyle mode ignores selected text and uses app rule
        let auto_style = resolve_transform_plan(
            &settings,
            "hi bob",
            TransformMode::AutoStyle,
            &mail_ctx,
            None,
        )
        .unwrap();
        assert!(!auto_style.is_command_replace);
        assert!(auto_style
            .prompt_template
            .contains("professional message or email"));

        // Command mode without selection returns None
        assert_eq!(
            resolve_transform_plan(
                &settings,
                "make formal",
                TransformMode::Command,
                &mail_no_sel,
                None
            ),
            None
        );

        // Unmatched app falls back to transform_default_dictation_prompt_id, then post-process prompt
        let unknown_ctx = TransformContext {
            app: Some(AppInfo {
                name: "Terminal".to_string(),
                bundle_id: "com.apple.Terminal".to_string(),
            }),
            selected_text: None,
            pid: None,
        };
        let fallback_pp = resolve_transform_plan(
            &settings,
            "ls minus la",
            TransformMode::AutoStyle,
            &unknown_ctx,
            None,
        )
        .unwrap();
        assert!(!fallback_pp.is_command_replace);
        assert!(fallback_pp.prompt_template.contains("speech-to-text model"));

        settings.transform_default_dictation_prompt_id = Some("default_style_notes".to_string());
        let fallback_transform = resolve_transform_plan(
            &settings,
            "note this",
            TransformMode::AutoStyle,
            &unknown_ctx,
            None,
        )
        .unwrap();
        assert!(
            fallback_transform.prompt_template.contains("Structured")
                || fallback_transform.prompt_template.contains("bullet points")
        );

        // Plain instruction prompt without placeholders works for both command & dictation
        settings.transform_prompts.push(LLMPrompt {
            id: "plain_cmd".to_string(),
            name: "Plain".to_string(),
            prompt: "Rewrite concisely for ${app}.".to_string(),
        });
        settings.transform_selected_prompt_id = Some("plain_cmd".to_string());
        let plain_cmd = resolve_transform_plan(
            &settings,
            "shorten",
            TransformMode::Command,
            &mail_ctx,
            None,
        )
        .unwrap();
        assert!(plain_cmd
            .system_prompt
            .contains("Rewrite concisely for Mail."));
        assert!(plain_cmd.legacy_prompt.contains("hey send me the deck"));
        assert!(plain_cmd.legacy_prompt.contains("shorten"));
    }

    #[test]
    fn resolve_transform_plan_honors_prompt_override_and_dynamic_action_lookup() {
        assert!(get_action("transform_prompt:default_style_casual").is_some());
        assert!(get_action("transform_prompt:").is_none());

        let mut settings = get_default_settings();
        settings.transform_prompts.push(LLMPrompt {
            id: "custom_translate".to_string(),
            name: "Translate Thai".to_string(),
            prompt: "Translate to Thai for ${app}.".to_string(),
        });

        let mail_with_sel = TransformContext {
            app: Some(AppInfo {
                name: "Mail".to_string(),
                bundle_id: "com.apple.mail".to_string(),
            }),
            selected_text: Some("Good morning team".to_string()),
            pid: Some(123),
        };
        let mail_no_sel = TransformContext {
            selected_text: None,
            ..mail_with_sel.clone()
        };

        // Override wins over selected command prompt when text is selected.
        let cmd_override = resolve_transform_plan(
            &settings,
            "translate",
            TransformMode::Smart,
            &mail_with_sel,
            Some("custom_translate"),
        )
        .unwrap();
        assert!(cmd_override.is_command_replace);
        assert!(cmd_override
            .system_prompt
            .contains("Translate to Thai for Mail."));

        // Override wins over Mail App Style Rule and fallback dictation prompt when no selection.
        let dict_override = resolve_transform_plan(
            &settings,
            "good morning",
            TransformMode::Smart,
            &mail_no_sel,
            Some("custom_translate"),
        )
        .unwrap();
        assert!(!dict_override.is_command_replace);
        assert_eq!(dict_override.system_prompt, "Translate to Thai for Mail.");

        // Unknown override gracefully falls back to normal Mail rule resolution.
        let unknown_override = resolve_transform_plan(
            &settings,
            "hi bob",
            TransformMode::Smart,
            &mail_no_sel,
            Some("missing_prompt"),
        )
        .unwrap();
        assert!(unknown_override
            .prompt_template
            .contains("professional message or email"));
    }

    #[test]
    fn command_transform_llm_mock_succeeds_and_fails_safely() {
        let ok_url = serve_once(r#"{"choices":[{"message":{"content":"Polished text."}}]}"#);
        let settings = post_process_settings(&ok_url);
        let ctx = TransformContext {
            app: Some(AppInfo {
                name: "Notes".to_string(),
                bundle_id: "com.apple.Notes".to_string(),
            }),
            selected_text: Some("raw note".to_string()),
            pid: None,
        };
        let plan =
            resolve_transform_plan(&settings, "polish it", TransformMode::Command, &ctx, None)
                .unwrap();
        assert!(plan.is_command_replace);
        let outcome = tauri::async_runtime::block_on(run_llm_completion(
            &settings,
            plan.system_prompt,
            plan.user_content,
            plan.legacy_prompt,
        ));
        assert_eq!(
            outcome,
            PostProcessOutcome::Done("Polished text.".to_string())
        );

        // Failure path returns Failed so caller never overwrites selection with raw instruction
        let fail_url = serve_once(r#"{"choices":[]}"#);
        let fail_settings = post_process_settings(&fail_url);
        let fail_plan = resolve_transform_plan(
            &fail_settings,
            "polish it",
            TransformMode::Command,
            &ctx,
            None,
        )
        .unwrap();
        let fail_outcome = tauri::async_runtime::block_on(run_llm_completion(
            &fail_settings,
            fail_plan.system_prompt,
            fail_plan.user_content,
            fail_plan.legacy_prompt,
        ));
        assert_eq!(fail_outcome, PostProcessOutcome::Failed);
    }

    #[test]
    fn resolve_instant_selection_plan_cleans_placeholders_and_requires_selection() {
        let mut settings = get_default_settings();
        settings.transform_prompts.push(crate::settings::LLMPrompt {
            id: "prompt_translate".to_string(),
            name: "Transform to English".to_string(),
            prompt: "Translate to English for ${app}.\n\nSelected text:\n${text}\n\nInstruction: ${instruction}".to_string(),
        });
        settings.transform_prompts.push(crate::settings::LLMPrompt {
            id: "prompt_plain".to_string(),
            name: "Formal".to_string(),
            prompt: "Rewrite formally.".to_string(),
        });

        let empty_ctx = TransformContext::default();
        assert!(
            super::resolve_instant_selection_plan(&settings, &empty_ctx, "prompt_translate")
                .is_none()
        );

        let ctx = TransformContext {
            app: Some(AppInfo {
                name: "Notes".to_string(),
                bundle_id: "com.apple.Notes".to_string(),
            }),
            selected_text: Some("สวัสดี".to_string()),
            pid: None,
        };
        let plan =
            super::resolve_instant_selection_plan(&settings, &ctx, "prompt_translate").unwrap();
        assert!(plan.is_command_replace);
        assert!(plan
            .system_prompt
            .contains("Translate to English for Notes."));
        assert!(!plan.system_prompt.contains("${text}"));
        assert!(!plan.system_prompt.contains("${instruction}"));
        assert!(!plan.system_prompt.contains("Selected text:"));
        assert!(!plan.system_prompt.contains("Instruction:"));
        assert!(plan
            .system_prompt
            .contains("no additional spoken instruction"));
        assert_eq!(
            plan.user_content,
            "Selected text:\n<selected_text>\nสวัสดี\n</selected_text>"
        );

        let plain_plan =
            super::resolve_instant_selection_plan(&settings, &ctx, "prompt_plain").unwrap();
        assert!(plain_plan.system_prompt.starts_with("Rewrite formally."));
        assert!(super::resolve_instant_selection_plan(&settings, &ctx, "missing_id").is_none());
    }
}
