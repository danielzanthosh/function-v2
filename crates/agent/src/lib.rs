//! Core AI Agent loop and state orchestration.
//!
//! Executes the Observe-Think-Act cycle. Converts natural language instructions
//! into structured tool calls, observes real system feedback, and reports state
//! changes cleanly to the UI layer without coupling to visual views.

use function_memory::MemoryStore;
use function_providers::{
    ChatMessage, CompletionRequest, LlmProvider, RetryingLlmProvider, ToolDefinition,
};
use function_tools::{ToolContext, ToolRegistry, ToolResult};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{broadcast, watch};

pub mod context;

/// `tracing` target for request lifecycle events. Filter with
/// `RUST_LOG=function_agent::lifecycle=info` to diagnose stuck requests.
pub const LIFECYCLE_LOG_TARGET: &str = "function_agent::lifecycle";

const STATE_CHANNEL_CAPACITY: usize = 256;
const MEMORY_RECALL_TIMEOUT: Duration = Duration::from_secs(5);
const TOOL_EXECUTION_TIMEOUT: Duration = Duration::from_secs(180);
const SPEECH_SUMMARY_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClarificationOptions {
    pub display_text: String,
    pub question: String,
    pub options: Vec<String>,
}

/// Complete a pending `request_user_choice` tool call with the user's answer.
pub fn choice_result_message(tool_call_id: impl Into<String>, answer: &str) -> ChatMessage {
    ChatMessage::tool(
        tool_call_id,
        serde_json::json!({"user_response": answer}).to_string(),
    )
}

/// Recognize a final assistant response that explicitly presents numbered choices.
pub fn parse_clarification_options(text: &str) -> Option<ClarificationOptions> {
    let lines: Vec<&str> = text.lines().collect();
    let heading = lines.iter().position(|line| {
        line.trim_matches(|character| matches!(character, '*' | '#' | ' ' | '\t'))
            .trim_end_matches(':')
            .trim()
            .eq_ignore_ascii_case("options")
    })?;

    let mut options = Vec::new();
    let mut follow_up = Vec::new();
    let mut reading_follow_up = false;
    for line in &lines[heading + 1..] {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !reading_follow_up {
            if let Some(option) = numbered_option(line) {
                options.push(option.to_string());
                continue;
            }
            if options.is_empty() {
                return None;
            }
            reading_follow_up = true;
        }
        follow_up.push(line);
    }
    if !(2..=6).contains(&options.len()) || follow_up.is_empty() {
        return None;
    }

    let question = follow_up.join(" ");
    if !question.contains('?') {
        return None;
    }
    let preceding = lines[..heading].join("\n").trim().to_string();
    Some(ClarificationOptions {
        display_text: preceding,
        question,
        options,
    })
}

fn numbered_option(line: &str) -> Option<&str> {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let (number, remainder) = line.split_at(digits);
    if number.parse::<usize>().ok()? == 0 {
        return None;
    }
    let remainder = remainder.strip_prefix(['.', ')'])?.trim();
    (!remainder.is_empty()).then_some(remainder)
}

#[derive(Error, Debug)]
pub enum AgentError {
    #[error("Provider error: {0}")]
    Provider(String),
    #[error("Tool error: {0}")]
    Tool(String),
    #[error("Task canceled by user")]
    Canceled,
    #[error("Execution step limit exceeded")]
    StepLimitExceeded,
    #[error("Repeated tool call loop detected: {0}")]
    RepeatedToolCalls(String),
}

/// Agent lifecycle states observed by the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", content = "data")]
pub enum AgentState {
    Idle,
    Listening,
    Processing {
        thought_summary: Option<String>,
    },
    Streaming {
        chunk: String,
        accumulated: String,
    },
    Acting {
        action_description: String,
    },
    Paused,
    NeedsUserInput {
        question: String,
        options: Vec<String>,
        tool_call_id: String,
        selected_window_id: Option<u32>,
        new_history: Vec<ChatMessage>,
    },
    ComputerUsePreview {
        target_window_id: u32,
        target_title: String,
        frame: Vec<u8>,
        cursor_x: Option<i32>,
        cursor_y: Option<i32>,
    },
    WaitingForConfirmation {
        action: String,
        details: String,
    },
    Completed {
        summary: String,
        new_history: Vec<ChatMessage>,
    },
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_history: Option<Vec<ChatMessage>>,
    },
}

impl PartialEq for AgentState {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (AgentState::Idle, AgentState::Idle) => true,
            (AgentState::Listening, AgentState::Listening) => true,
            (AgentState::Paused, AgentState::Paused) => true,
            (
                AgentState::NeedsUserInput { question: a, options: b, tool_call_id: c, selected_window_id: g, .. },
                AgentState::NeedsUserInput { question: d, options: e, tool_call_id: f, selected_window_id: h, .. },
            ) => a == d && b == e && c == f && g == h,
            (
                AgentState::Processing { thought_summary: a },
                AgentState::Processing { thought_summary: b },
            ) => a == b,
            (
                AgentState::Streaming { accumulated: a, .. },
                AgentState::Streaming { accumulated: b, .. },
            ) => a == b,
            (
                AgentState::Acting {
                    action_description: a,
                },
                AgentState::Acting {
                    action_description: b,
                },
            ) => a == b,
            (
                AgentState::ComputerUsePreview { target_window_id: a, target_title: b, cursor_x: c, cursor_y: d, .. },
                AgentState::ComputerUsePreview { target_window_id: e, target_title: f, cursor_x: g, cursor_y: h, .. },
            ) => a == e && b == f && c == g && d == h,
            (
                AgentState::WaitingForConfirmation {
                    action: a,
                    details: c,
                },
                AgentState::WaitingForConfirmation {
                    action: b,
                    details: d,
                },
            ) => a == b && c == d,
            // Compare only the summary; history is not used for equality checks
            (
                AgentState::Completed { summary: a, .. },
                AgentState::Completed { summary: b, .. },
            ) => a == b,
            (AgentState::Error { message: a, .. }, AgentState::Error { message: b, .. }) => a == b,
            _ => false,
        }
    }
}

impl Eq for AgentState {}

/// Detailed action event emitted during agent operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentActionEvent {
    pub step: usize,
    pub description: String,
    pub success: bool,
}

/// Core agent orchestrator.
pub struct Agent {
    provider: RwLock<Arc<dyn LlmProvider>>,
    tools: ToolRegistry,
    memory: Arc<dyn MemoryStore>,
    state_tx: broadcast::Sender<AgentState>,
    /// Monotonic cancellation generation. Every `cancel()` and every new run
    /// bumps it; a run is canceled once the generation differs from the one it
    /// started with. Unlike a resettable flag, a new run can never "un-cancel"
    /// a previous run that is still in flight.
    cancel_tx: watch::Sender<u64>,
    pause_tx: watch::Sender<bool>,
    current_run_cancel_base: AtomicU64,
    run_counter: AtomicU64,
    active_run: Arc<AtomicU64>,
    max_steps: usize,
    request_delay_ms: AtomicU64,
    input_token_limit: RwLock<function_config::InputTokenLimit>,
    context_optimization_enabled: RwLock<bool>,
}

/// Per-run handle used to emit state updates and observe cancellation.
#[derive(Clone)]
struct RunHandle {
    run_id: u64,
    cancel_base: u64,
    cancel_rx: watch::Receiver<u64>,
    pause_rx: watch::Receiver<bool>,
    state_tx: broadcast::Sender<AgentState>,
    active_run: Arc<AtomicU64>,
}

impl RunHandle {
    fn is_canceled(&self) -> bool {
        *self.cancel_rx.borrow() != self.cancel_base
    }

    fn is_current(&self) -> bool {
        self.active_run.load(Ordering::SeqCst) == self.run_id
    }

    /// Emit an intermediate state; dropped once the run is canceled so a
    /// stale run cannot overwrite the UI state of a newer request.
    fn emit(&self, state: AgentState) {
        if !self.is_canceled() {
            let _ = self.state_tx.send(state);
        }
    }

    /// Emit a terminal state unless a newer run has superseded this one.
    fn emit_final(&self, state: AgentState) {
        if self.is_current() {
            let _ = self.state_tx.send(state);
        }
    }

    /// Resolves once this run has been canceled or superseded.
    async fn canceled(&self) {
        let mut rx = self.cancel_rx.clone();
        loop {
            if *rx.borrow_and_update() != self.cancel_base {
                return;
            }
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    async fn wait_until_resumed(&self) -> bool {
        let mut rx = self.pause_rx.clone();
        loop {
            if !*rx.borrow_and_update() {
                return !self.is_canceled();
            }
            tokio::select! {
                _ = self.canceled() => return false,
                changed = rx.changed() => if changed.is_err() { return false; },
            }
        }
    }
}

/// Guarantees a terminal state is broadcast even if the run future panics or
/// is dropped before reaching one, so the UI never stays in a busy state.
struct RunGuard {
    run: RunHandle,
    started: Instant,
    armed: bool,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        if self.run.is_canceled() {
            tracing::info!(
                target: LIFECYCLE_LOG_TARGET,
                run_id = self.run.run_id,
                elapsed_ms,
                "request dropped after cancellation"
            );
            self.run.emit_final(AgentState::Idle);
        } else {
            tracing::error!(
                target: LIFECYCLE_LOG_TARGET,
                run_id = self.run.run_id,
                elapsed_ms,
                panicking = std::thread::panicking(),
                "request ended without reaching a terminal state"
            );
            self.run.emit_final(AgentState::Error {
                message: "The request ended unexpectedly. Please try again.".to_string(),
                new_history: None,
            });
        }
    }
}

struct RunFailure {
    error: AgentError,
    history: Vec<ChatMessage>,
}

struct RunOutput {
    reply: String,
    history: Vec<ChatMessage>,
    pending_choice: Option<PendingChoice>,
    selected_window_id: Option<u32>,
}

struct PendingChoice {
    question: String,
    options: Vec<String>,
    tool_call_id: String,
}

fn parse_choice_request(arguments: &serde_json::Value) -> Result<(String, Vec<String>), AgentError> {
    let question = arguments.get("question").and_then(serde_json::Value::as_str)
        .map(str::trim).filter(|value| !value.is_empty())
        .ok_or_else(|| AgentError::Tool("request_user_choice requires a non-empty question".into()))?;
    let options = arguments.get("options").and_then(serde_json::Value::as_array)
        .filter(|values| (2..=6).contains(&values.len()))
        .ok_or_else(|| AgentError::Tool("request_user_choice requires 2 to 6 options".into()))?;
    let options = options.iter().map(|value| {
        value.as_str().map(str::trim).filter(|text| !text.is_empty())
            .map(str::to_string)
            .ok_or_else(|| AgentError::Tool("request_user_choice options must be non-empty strings".into()))
    }).collect::<Result<Vec<_>, _>>()?;
    Ok((question.to_string(), options))
}

fn enforce_background_input_scope(
    call: &mut function_providers::ToolCall,
    selected_window: &mut Option<u32>,
) -> Result<(), AgentError> {
    let action = call.arguments.get("action").and_then(serde_json::Value::as_str).unwrap_or("");
    let parse_id = |key: &str| -> Result<Option<u32>, AgentError> {
        let Some(value) = call.arguments.get(key) else { return Ok(None) };
        let id = value.as_u64().and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| AgentError::Tool(format!("{key} must be a valid target window ID")))?;
        Ok(Some(id))
    };
    let target_param = parse_id("target_window_id")?;
    let screen_target = if call.name == "computer_screen"
        && matches!(action, "target_screenshot" | "target_accessibility" | "target_accessibility_action")
    {
        parse_id("window_id")?
    } else { None };
    let explicit_target = target_param.or(screen_target);

    if let Some(window_id) = explicit_target {
        *selected_window = Some(window_id);
        return Ok(());
    }

    if matches!(call.name.as_str(), "computer_mouse" | "computer_keyboard") {
        if let Some(window_id) = *selected_window {
            let Some(arguments) = call.arguments.as_object_mut() else {
                return Err(AgentError::Tool("Background input arguments must be an object".into()));
            };
            arguments.insert("target_window_id".into(), serde_json::json!(window_id));
        }
        return Ok(());
    }

    if selected_window.is_some() && matches!(call.name.as_str(),
        "open_app" | "close_app" | "click" | "double_click" | "type_text" | "press_key" | "scroll"
    ) {
        return Err(AgentError::Tool(format!(
            "Global desktop action '{}' blocked while a background window is selected; use target-window computer tools instead",
            call.name
        )));
    }

    if selected_window.is_some() && call.name == "computer_apps"
        && matches!(action, "launch" | "focus_window")
    {
        return Err(AgentError::Tool(
            "Global app activation blocked while a background window is selected; select the next target with computer_screen target_windows and target_screenshot".into(),
        ));
    }
    Ok(())
}

impl RunFailure {
    fn new(error: AgentError, history: &[ChatMessage]) -> Self {
        Self {
            error,
            history: history.to_vec(),
        }
    }
}

fn describe_tool_call(call: &function_providers::ToolCall) -> String {
    match call.name.as_str() {
        // High-level OS-aware tools
        "open_app" => {
            let app_name = call
                .arguments
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("application");
            format!("Opening {}...", app_name)
        }
        "close_app" => {
            let app_name = call
                .arguments
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("application");
            format!("Closing {}...", app_name)
        }
        "take_screenshot" => "Capturing screen observation...".to_string(),
        "click" => {
            if let (Some(x), Some(y)) = (
                call.arguments.get("x").and_then(|v| v.as_f64()),
                call.arguments.get("y").and_then(|v| v.as_f64()),
            ) {
                format!("Clicking at ({}, {})...", x as i32, y as i32)
            } else {
                "Clicking target...".to_string()
            }
        }
        "double_click" => "Double-clicking target...".to_string(),
        "type_text" => "Typing text...".to_string(),
        "press_key" => {
            let key = call
                .arguments
                .get("key")
                .and_then(|k| k.as_str())
                .unwrap_or("key");
            format!("Pressing {}...", key)
        }
        "scroll" => "Scrolling view...".to_string(),
        "execute_command" => {
            let cmd = call
                .arguments
                .get("command")
                .and_then(|c| c.as_str())
                .unwrap_or("command");
            format!("Executing `{}`...", cmd)
        }
        // Low-level / legacy tools
        "computer_screen" => {
            let action = call
                .arguments
                .get("action")
                .and_then(|a| a.as_str())
                .unwrap_or("");
            match action {
                "screenshot" => "Capturing screen context...".to_string(),
                "dimensions" => "Checking screen resolution...".to_string(),
                "cursor" => "Locating mouse cursor...".to_string(),
                _ => "Reading screen info...".to_string(),
            }
        }
        "computer_mouse" => {
            let action = call
                .arguments
                .get("action")
                .and_then(|a| a.as_str())
                .unwrap_or("");
            match action {
                "move" => "Moving mouse...".to_string(),
                "click" => "Clicking mouse...".to_string(),
                "double_click" => "Double-clicking...".to_string(),
                "right_click" => "Right-clicking...".to_string(),
                "middle_click" => "Middle-clicking...".to_string(),
                "drag" => "Dragging on screen...".to_string(),
                "scroll" => "Scrolling...".to_string(),
                _ => "Operating mouse...".to_string(),
            }
        }
        "computer_keyboard" => {
            let action = call
                .arguments
                .get("action")
                .and_then(|a| a.as_str())
                .unwrap_or("");
            match action {
                "type" => "Typing text...".to_string(),
                "key" => "Pressing key...".to_string(),
                "shortcut" => "Triggering shortcut...".to_string(),
                _ => "Sending keystrokes...".to_string(),
            }
        }
        "computer_apps" => {
            let action = call
                .arguments
                .get("action")
                .and_then(|a| a.as_str())
                .unwrap_or("");
            match action {
                "open" => {
                    let app_name = call
                        .arguments
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("application");
                    format!("Opening {}...", app_name)
                }
                "list" => "Checking running applications...".to_string(),
                "focus" => "Switching window focus...".to_string(),
                _ => "Managing applications...".to_string(),
            }
        }
        "fs" => "Accessing filesystem...".to_string(),
        "terminal" => "Executing terminal command...".to_string(),
        _ => format!("Running {}", call.name),
    }
}

impl Agent {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: ToolRegistry,
        memory: Arc<dyn MemoryStore>,
    ) -> Self {
        let (state_tx, _) = broadcast::channel(STATE_CHANNEL_CAPACITY);
        let (cancel_tx, _) = watch::channel(0);
        let (pause_tx, _) = watch::channel(false);
        Self {
            provider: RwLock::new(provider),
            tools,
            memory,
            state_tx,
            cancel_tx,
            pause_tx,
            current_run_cancel_base: AtomicU64::new(0),
            run_counter: AtomicU64::new(0),
            active_run: Arc::new(AtomicU64::new(0)),
            max_steps: 30,
            request_delay_ms: AtomicU64::new(0),
            input_token_limit: RwLock::new(function_config::InputTokenLimit::Auto),
            context_optimization_enabled: RwLock::new(true),
        }
    }

    /// Clear all persistent memory without touching provider or app configuration.
    pub async fn clear_all_memory(&self) -> Result<(), function_memory::MemoryError> {
        self.memory.clear_all().await
    }

    /// Set a request delay in milliseconds applied before sending new requests to the provider.
    pub fn set_request_delay(&self, delay_ms: u64) {
        self.request_delay_ms.store(delay_ms, Ordering::Relaxed);
    }

    /// Set the input token budget limit setting.
    pub fn set_input_token_limit(&self, limit: function_config::InputTokenLimit) {
        if let Ok(mut lock) = self.input_token_limit.write() {
            *lock = limit;
        }
    }

    /// Enable or disable automatic context compaction before provider requests.
    pub fn set_context_optimization_enabled(&self, enabled: bool) {
        if let Ok(mut lock) = self.context_optimization_enabled.write() {
            *lock = enabled;
        }
    }

    /// Request cancellation of the currently executing task. In-flight
    /// provider requests, request delays, and tool calls are abandoned
    /// immediately rather than at the next step boundary.
    pub fn cancel(&self) {
        self.cancel_tx
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        tracing::info!(
            target: LIFECYCLE_LOG_TARGET,
            run_id = self.active_run.load(Ordering::SeqCst),
            "cancellation requested"
        );
    }

    /// Pause the current run at its next model/tool boundary.
    pub fn pause(&self) {
        self.pause_tx.send_replace(true);
        if self.active_run.load(Ordering::SeqCst) != 0 {
            let _ = self.state_tx.send(AgentState::Paused);
        }
    }

    /// Resume a paused run.
    pub fn resume(&self) {
        self.pause_tx.send_replace(false);
        let _ = self.state_tx.send(AgentState::Processing {
            thought_summary: None,
        });
    }

    pub fn is_paused(&self) -> bool {
        *self.pause_tx.borrow()
    }

    /// Check if cancellation was requested for the most recent run.
    pub fn is_canceled(&self) -> bool {
        *self.cancel_tx.borrow() != self.current_run_cancel_base.load(Ordering::SeqCst)
    }

    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }

    pub fn set_max_steps(&mut self, max_steps: usize) {
        self.max_steps = max_steps;
    }

    pub fn max_steps(&self) -> usize {
        self.max_steps
    }

    /// Replace the active provider without rebuilding the agent or losing its
    /// tools, memory, state subscription, or cancellation state.
    pub fn set_provider(&self, provider: Arc<dyn LlmProvider>) {
        if let Ok(mut active_provider) = self.provider.write() {
            *active_provider = provider;
        }
    }

    /// Subscribe to state updates (used by GPUI views).
    pub fn subscribe_state(&self) -> broadcast::Receiver<AgentState> {
        self.state_tx.subscribe()
    }

    /// Generate a short, spoken-only version of a completed response.
    pub async fn generate_speech_text(&self, response: &str) -> Result<String, AgentError> {
        let provider = self
            .provider
            .read()
            .map_err(|error| AgentError::Provider(format!("Provider lock poisoned: {error}")))?
            .clone();
        let source: String = response.chars().take(8_000).collect();
        let request = CompletionRequest {
            model: "default".into(),
            messages: vec![
                ChatMessage::system(
                    "Write a concise spoken version of the assistant response for text-to-speech. Output only the words to speak, in one short phrase or at most two sentences. Preserve the key answer and avoid markdown, headings, lists, and stage directions.",
                ),
                ChatMessage::user(source),
            ],
            tools: Vec::new(),
            temperature: Some(0.3),
        };
        let response = tokio::time::timeout(SPEECH_SUMMARY_TIMEOUT, provider.complete(request))
            .await
            .map_err(|_| AgentError::Provider("Speech summary timed out".into()))?
            .map_err(|error| AgentError::Provider(error.to_string()))?;
        let text = limit_spoken_sentences(&response.message.content);
        if text.is_empty() {
            return Err(AgentError::Provider("Speech summary was empty".into()));
        }
        Ok(text)
    }

    fn begin_run(&self) -> RunHandle {
        // A new run supersedes (and cancels) any run still in flight.
        let mut cancel_base = 0;
        self.cancel_tx.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
            cancel_base = *generation;
        });
        self.current_run_cancel_base
            .store(cancel_base, Ordering::SeqCst);
        let run_id = self.run_counter.fetch_add(1, Ordering::SeqCst) + 1;
        self.active_run.fetch_max(run_id, Ordering::SeqCst);
        self.pause_tx.send_replace(false);
        RunHandle {
            run_id,
            cancel_base,
            cancel_rx: self.cancel_tx.subscribe(),
            pause_rx: self.pause_tx.subscribe(),
            state_tx: self.state_tx.clone(),
            active_run: self.active_run.clone(),
        }
    }

    /// Execute a task given a user prompt.
    pub async fn execute_task(&self, user_prompt: &str) -> Result<String, AgentError> {
        let (reply, _) = self.execute_with_history(user_prompt, vec![]).await?;
        Ok(reply)
    }

    /// Execute with persistent chat history.
    ///
    /// `prior_history` contains only the User/Assistant turn pairs from previous exchanges
    /// (no system message — this function prepends one). Returns the assistant reply text
    /// and the updated history to store for the next turn.
    ///
    /// Every exit path broadcasts exactly one terminal state (`Completed`,
    /// `NeedsUserInput`, `Error`, or `Idle` on cancellation) unless superseded.
    pub async fn execute_with_history(
        &self,
        user_prompt: &str,
        prior_history: Vec<ChatMessage>,
    ) -> Result<(String, Vec<ChatMessage>), AgentError> {
        self.execute_with_history_in_window(user_prompt, prior_history, None).await
    }

    /// Execute a follow-up while preserving a previously selected background window.
    pub async fn execute_with_history_in_window(
        &self,
        user_prompt: &str,
        prior_history: Vec<ChatMessage>,
        selected_window_id: Option<u32>,
    ) -> Result<(String, Vec<ChatMessage>), AgentError> {
        let run = self.begin_run();
        let started = Instant::now();
        tracing::info!(
            target: LIFECYCLE_LOG_TARGET,
            run_id = run.run_id,
            prompt_chars = user_prompt.chars().count(),
            history_messages = prior_history.len(),
            "request started"
        );
        let mut guard = RunGuard {
            run: run.clone(),
            started,
            armed: true,
        };
        run.emit(AgentState::Processing {
            thought_summary: None,
        });

        let outcome = self.run_loop(&run, user_prompt, prior_history, selected_window_id).await;
        guard.armed = false;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let canceled = run.is_canceled()
            || matches!(
                outcome,
                Err(RunFailure {
                    error: AgentError::Canceled,
                    ..
                })
            );
        if canceled {
            tracing::info!(
                target: LIFECYCLE_LOG_TARGET,
                run_id = run.run_id,
                elapsed_ms,
                superseded = !run.is_current(),
                "request canceled"
            );
            run.emit_final(AgentState::Idle);
            return Err(AgentError::Canceled);
        }

        match outcome {
            Ok(output) if output.pending_choice.is_some() => {
                let pending = output.pending_choice.expect("guarded pending choice");
                run.emit_final(AgentState::NeedsUserInput {
                    question: pending.question,
                    options: pending.options,
                    tool_call_id: pending.tool_call_id,
                    selected_window_id: output.selected_window_id,
                    new_history: output.history.clone(),
                });
                Ok((output.reply, output.history))
            }
            Ok(output) => {
                tracing::info!(
                    target: LIFECYCLE_LOG_TARGET,
                    run_id = run.run_id,
                    elapsed_ms,
                    reply_chars = output.reply.chars().count(),
                    history_messages = output.history.len(),
                    "request completed"
                );
                run.emit_final(AgentState::Completed {
                    summary: output.reply.clone(),
                    new_history: output.history.clone(),
                });
                Ok((output.reply, output.history))
            }
            Err(RunFailure { error, history }) => {
                tracing::error!(
                    target: LIFECYCLE_LOG_TARGET,
                    run_id = run.run_id,
                    elapsed_ms,
                    error = %error,
                    "request failed"
                );
                run.emit_final(AgentState::Error {
                    message: error.to_string(),
                    new_history: Some(history),
                });
                Err(error)
            }
        }
    }

    async fn run_loop(
        &self,
        run: &RunHandle,
        user_prompt: &str,
        prior_history: Vec<ChatMessage>,
        selected_window_id: Option<u32>,
    ) -> Result<RunOutput, RunFailure> {
        let run_id = run.run_id;

        // history_tail tracks just the conversation turns (no system msg, no tool internals)
        // so the caller can persist and pass them back next time.
        let mut history_tail = prior_history.clone();
        history_tail.push(ChatMessage::user(user_prompt));

        let memory_started = Instant::now();
        tracing::info!(target: LIFECYCLE_LOG_TARGET, run_id, "memory retrieval started");
        let recall = tokio::select! {
            biased;
            _ = run.canceled() => {
                return Err(RunFailure::new(AgentError::Canceled, &history_tail));
            }
            recall = tokio::time::timeout(
                MEMORY_RECALL_TIMEOUT,
                self.memory.recall(user_prompt, None, 5),
            ) => recall,
        };
        let context_items = match recall {
            Ok(Ok(items)) => {
                tracing::info!(
                    target: LIFECYCLE_LOG_TARGET,
                    run_id,
                    items = items.len(),
                    elapsed_ms = memory_started.elapsed().as_millis() as u64,
                    "memory retrieval completed"
                );
                items
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    target: LIFECYCLE_LOG_TARGET,
                    run_id,
                    error = %error,
                    "memory retrieval failed; continuing without memory context"
                );
                Vec::new()
            }
            Err(_) => {
                tracing::warn!(
                    target: LIFECYCLE_LOG_TARGET,
                    run_id,
                    timeout_ms = MEMORY_RECALL_TIMEOUT.as_millis() as u64,
                    "memory retrieval timed out; continuing without memory context"
                );
                Vec::new()
            }
        };

        if run.is_canceled() {
            return Err(RunFailure::new(AgentError::Canceled, &history_tail));
        }

        let mut system_prompt = "\
You are Function, a fast, proactive, native agentic desktop AI assistant.

Operating System Awareness:
- You run directly on the host computer. Detect the OS and interact using native capabilities.
- NEVER assume Windows when running on macOS or Linux, and vice-versa.
- NEVER use Windows commands like `start chrome.exe` or `dir` on macOS or Linux.
- NEVER use raw shell commands to open or control desktop applications. Always use the native desktop tools.

Primary High-Level Desktop Capabilities:
1. `open_app`: Launch or focus applications natively by name (e.g. {\"name\": \"Google Chrome\"}, {\"name\": \"Terminal\"}). Uses native OS APIs on macOS, Windows, and Linux.
2. `close_app`: Gracefully quit or terminate applications by name.
3. `computer_screen`: Use `target_windows` to find an app window, then keep its `window_id` for the workflow. Its window-scoped capture and input can operate while the window is behind another app; do not activate or raise it just to observe or control it. Refresh observations with `target_screenshot`, and prefer `target_accessibility` plus `target_accessibility_action` for labeled controls. Use target-scoped mouse/keyboard actions for canvas or custom UI and verify each action with a fresh target screenshot. Use full-desktop `screenshot` only when requested or when no target window is available. A window must remain capturable by macOS; respect Screen Recording and Accessibility permissions and report when unavailable.
4. `click`: Click at specified (x, y) coordinates or current cursor location.
5. `double_click`: Double click at specified (x, y) coordinates.
6. `type_text`: Type text or unicode characters into the active input field or window.
7. `press_key`: Send key presses (e.g. \"return\", \"space\", \"tab\", \"escape\") or keyboard shortcuts (e.g. \"cmd+t\", \"ctrl+c\", \"alt+f4\").
8. `scroll`: Scroll vertically or horizontally by a given amount.
9. `execute_command`: Run shell commands ONLY when no high-level native tool exists. Safe commands run automatically; destructive commands pause for user confirmation.
10. `fs`: Read, write, or list filesystem directories and files.
11. `web_search`: Search the web for current documentation, news, or knowledge.

Agentic Multi-Step & Observation Loop:
- Plan -> Execute -> Observe -> Recover/Iterate -> Conclude.
- When a user decision is required, call `request_user_choice` with a concise question and 2–6 concise, mutually exclusive options. Do not continue the task before the user chooses. The UI also recognizes the older numbered `Options:` format as a fallback.
- For background control, choose the target from `target_windows`; never use global mouse/keyboard tools or app focus commands after selection. Target-scoped input is delivered to that app without moving or typing through the user's physical mouse and keyboard.
- The colored AI cursor is a preview-only overlay in Function's computer-use panel. It is not the macOS pointer, is not drawn into the target app, and does not move the user's real cursor.
- For multi-step tasks (e.g., 'Open Chrome, go to YouTube, and check views'):
  1. Use `open_app` to launch the application.
  2. List windows and capture the selected target with `computer_screen` before acting. Reuse its `window_id` for each scoped input action.
  3. Click, type, or press keys to navigate to the target.
  4. Capture the target window again to inspect the new visual context and verify the result.
  5. Continue iteratively until the user's objective is fully accomplished.
- If an action or tool fails, do NOT immediately abort or dump raw errors. Inspect the structured error, take a screenshot if visual insight helps, attempt an alternative recovery path, and continue.
- Keep the final response clear, concise, and beautifully formatted in markdown.\n\nPersistent Memory:\n- When the user explicitly asks you to remember or save a useful fact or stable preference, call the `memory` tool with action `remember`. Do not claim it was saved unless the tool reports success.\n- Use the `memory` tool with action `recall` when the user asks what you remember or a saved fact is relevant.\n- Only delete memory when the user asks you to forget a specific item or clear a category.\n- Never save passwords, API keys, access tokens, recovery phrases, private keys, or other secrets."
            .to_string();

        let memory_items = context_items.len();
        if !context_items.is_empty() {
            system_prompt.push_str("\n\nRelevant Context:\n");
            for item in context_items {
                system_prompt.push_str(&format!("- {}: {}\n", item.key, item.value));
            }
        }

        let mut tool_definitions: Vec<ToolDefinition> = self
            .tools
            .list()
            .into_iter()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters_schema(),
            })
            .collect();
        tool_definitions.push(ToolDefinition {
            name: "request_user_choice".into(),
            description: "Pause this task and ask the user to choose before continuing. Use only when a user decision is required; provide 2 to 6 concise options.".into(),
            parameters: serde_json::json!({
                "type":"object",
                "properties":{
                    "question":{"type":"string"},
                    "options":{"type":"array","minItems":2,"maxItems":6,"items":{"type":"string"}}
                },
                "required":["question","options"],
                "additionalProperties":false
            }),
        });

        // Build the full message list: system + prior history + new user turn
        let mut messages = Vec::with_capacity(prior_history.len() + 2);
        let system_prompt_chars = system_prompt.chars().count();
        messages.push(ChatMessage::system(system_prompt));
        messages.extend(prior_history);
        messages.push(ChatMessage::user(user_prompt));

        tracing::info!(
            target: LIFECYCLE_LOG_TARGET,
            run_id,
            system_prompt_chars,
            memory_items,
            tools = tool_definitions.len(),
            messages = messages.len(),
            "context prepared"
        );

        let mut step = 0;
        let mut final_result = String::new();
        let mut consecutive_repeated_tool_calls = 0;
        let mut last_step_tool_signature = String::new();
        let mut selected_window = selected_window_id;

        while step < self.max_steps {
            if !run.wait_until_resumed().await {
                return Err(RunFailure::new(AgentError::Canceled, &history_tail));
            }
            step += 1;

            let raw_provider = match self.provider.read() {
                Ok(provider) => provider.clone(),
                Err(e) => {
                    return Err(RunFailure::new(
                        AgentError::Provider(format!("Provider lock poisoned: {e}")),
                        &history_tail,
                    ))
                }
            };

            let retry_run = run.clone();
            let provider: Arc<dyn LlmProvider> =
                Arc::new(RetryingLlmProvider::with_status_callback(
                    raw_provider,
                    Arc::new(move |status_msg| {
                        tracing::warn!(
                            target: LIFECYCLE_LOG_TARGET,
                            run_id,
                            step,
                            status = %status_msg,
                            "provider retry started"
                        );
                        retry_run.emit(AgentState::Processing {
                            thought_summary: Some(status_msg),
                        });
                    }),
                ));

            let limit_setting = *self
                .input_token_limit
                .read()
                .unwrap_or_else(|e| e.into_inner());
            let target_budget = limit_setting.token_budget(provider.context_limit("default"));

            let context_optimization_enabled = *self
                .context_optimization_enabled
                .read()
                .unwrap_or_else(|e| e.into_inner());

            let estimated_tokens = context::estimate_tokens(&messages, &tool_definitions);
            if estimated_tokens > target_budget {
                if let Some((index, message)) = messages
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, message)| message.content.chars().count())
                {
                    tracing::warn!(
                        target: LIFECYCLE_LOG_TARGET,
                        run_id,
                        step,
                        estimated_tokens,
                        target_budget,
                        largest_message_index = index,
                        largest_message_role = ?message.role,
                        largest_message_chars = message.content.chars().count(),
                        largest_message_images = message.images.as_ref().map_or(0, Vec::len),
                        largest_message_tool_calls = message.tool_calls.as_ref().map_or(0, Vec::len),
                        "oversized context detected; largest message identified"
                    );
                }
            }

            // Size-only diagnostics: locate prompt growth without logging prompt text,
            // screenshot bytes, credentials, or tool output contents.
            tracing::info!(
                target: LIFECYCLE_LOG_TARGET,
                run_id,
                step,
                message_count = messages.len(),
                text_chars = messages.iter().map(|m| m.content.chars().count()).sum::<usize>(),
                image_count = messages.iter().map(|m| m.images.as_ref().map_or(0, Vec::len)).sum::<usize>(),
                image_chars = messages.iter().flat_map(|m| m.images.iter().flatten()).map(|image| image.chars().count()).sum::<usize>(),
                tool_call_count = messages.iter().map(|m| m.tool_calls.as_ref().map_or(0, Vec::len)).sum::<usize>(),
                tool_schema_count = tool_definitions.len(),
                tool_schema_chars = tool_definitions.iter().map(|t| t.name.chars().count() + t.description.chars().count() + t.parameters.to_string().chars().count()).sum::<usize>(),
                estimated_tokens,
                target_budget,
                "preflight request size"
            );
            for (message_index, message) in messages.iter().enumerate() {
                tracing::debug!(
                    target: LIFECYCLE_LOG_TARGET,
                    run_id,
                    step,
                    message_index,
                    role = ?message.role,
                    chars = message.content.chars().count(),
                    images = message.images.as_ref().map_or(0, Vec::len),
                    image_chars = message.images.as_ref().map(|images| images.iter().map(|image| image.chars().count()).sum::<usize>()).unwrap_or(0),
                    tool_call_count = message.tool_calls.as_ref().map_or(0, Vec::len),
                    tool_output = message.tool_call_id.is_some(),
                    "request message size"
                );
            }

            let compaction_started = Instant::now();
            // Context optimization can be disabled by the user, but never allow an
            // oversized request to escape the safety guard. Compact whenever enabled
            // or whenever the request exceeds the configured input budget.
            let (request_messages, compaction) = if context_optimization_enabled
                || estimated_tokens > target_budget
            {
                context::compact_messages(&messages, &tool_definitions, target_budget)
            } else {
                (
                    messages.clone(),
                    context::ContextCompactionReport {
                        before_tokens: estimated_tokens,
                        after_tokens: estimated_tokens,
                        target_budget,
                        ..Default::default()
                    },
                )
            };

            if context_optimization_enabled || compaction.before_tokens > compaction.after_tokens {
                tracing::info!(
                    target: LIFECYCLE_LOG_TARGET,
                    run_id,
                    step,
                    provider = %provider.name(),
                    before_tokens = compaction.before_tokens,
                    after_tokens = compaction.after_tokens,
                    budget = target_budget,
                    removed_messages = compaction.removed_messages,
                    removed_images = compaction.removed_images,
                    truncated_outputs = compaction.truncated_outputs,
                    elapsed_ms = compaction_started.elapsed().as_millis() as u64,
                    "context compaction completed"
                );
                if compaction.budget_unreachable {
                    tracing::warn!(
                        target: LIFECYCLE_LOG_TARGET,
                        run_id,
                        step,
                        after_tokens = compaction.after_tokens,
                        budget = target_budget,
                        "context exceeds the input token budget even after compaction"
                    );
                }

                if compaction.before_tokens > compaction.after_tokens {
                    let msg = format!(
                        "Context optimized {} → {} tokens",
                        context::format_token_k(compaction.before_tokens),
                        context::format_token_k(compaction.after_tokens)
                    );
                    run.emit(AgentState::Processing {
                        thought_summary: Some(msg),
                    });
                }
            }

            // Never send a request that the local estimator already knows
            // exceeds the configured/provider context budget. Dispatching anyway
            // turns a recoverable local issue into a provider HTTP 400.
            if compaction.budget_unreachable || compaction.after_tokens > target_budget {
                let message = format!(
                    "Request is still too large after context compaction (estimated {} tokens; budget {}). Shorten the task or reduce the tool/context payload before retrying.",
                    context::format_token_k(compaction.after_tokens),
                    context::format_token_k(target_budget),
                );
                tracing::error!(
                    target: LIFECYCLE_LOG_TARGET,
                    run_id,
                    step,
                    estimated_tokens = compaction.after_tokens,
                    target_budget,
                    "blocking oversized request before provider dispatch"
                );
                return Err(RunFailure::new(AgentError::Provider(message), &history_tail));
            }

            let req = CompletionRequest {
                model: "default".to_string(),
                messages: request_messages,
                tools: tool_definitions.clone(),
                temperature: Some(0.7),
            };

            // Apply configured request delay before dispatch (separate from rate-limit retries)
            let delay_ms = self.request_delay_ms.load(Ordering::Relaxed);
            if delay_ms > 0 {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                    _ = run.canceled() => {
                        return Err(RunFailure::new(AgentError::Canceled, &history_tail));
                    }
                }
            }

            let request_started = Instant::now();
            tracing::info!(
                target: LIFECYCLE_LOG_TARGET,
                run_id,
                step,
                provider = %provider.name(),
                messages = req.messages.len(),
                estimated_tokens = compaction.after_tokens,
                "provider request started"
            );

            let stream_run = run.clone();
            let mut stream_accumulated = String::new();
            let mut first_token_seen = false;
            let on_token = Box::new(move |token: String| {
                if !first_token_seen {
                    first_token_seen = true;
                    tracing::info!(
                        target: LIFECYCLE_LOG_TARGET,
                        run_id,
                        step,
                        first_token_ms = request_started.elapsed().as_millis() as u64,
                        "first stream token received"
                    );
                }
                stream_accumulated.push_str(&token);
                stream_run.emit(AgentState::Streaming {
                    chunk: token,
                    accumulated: stream_accumulated.clone(),
                });
            });

            let response = tokio::select! {
                biased;
                _ = run.canceled() => {
                    tracing::info!(
                        target: LIFECYCLE_LOG_TARGET,
                        run_id,
                        step,
                        elapsed_ms = request_started.elapsed().as_millis() as u64,
                        "provider request abandoned after cancellation"
                    );
                    return Err(RunFailure::new(AgentError::Canceled, &history_tail));
                }
                response = provider.complete_stream(req, on_token) => response,
            };

            let response = match response {
                Ok(res) => res,
                Err(e) => {
                    // Some OpenAI-compatible providers/models reject image content with HTTP 400,
                    // even when they accept ordinary tool calls. A screenshot should not abort the
                    // whole computer-control task: retry once without image payloads while keeping
                    // the textual tool results and the rest of the conversation intact.
                    let error_text = e.to_string();
                    let has_images = messages.iter().any(|message| {
                        message.images.as_ref().is_some_and(|images| !images.is_empty())
                    });
                    // Some OpenRouter routes reject tool calling for the selected model.
                    // Retry without web_search instead of aborting otherwise-valid desktop work.
                    let unsupported_tool_routing = error_text.contains("API error (400)")
                        && (error_text.to_ascii_lowercase().contains("no endpoints found that support tool use")
                            || error_text.to_ascii_lowercase().contains("does not support tool use"));
                    if unsupported_tool_routing {
                        let before = tool_definitions.len();
                        tool_definitions.retain(|tool| tool.name != "web_search");
                        if tool_definitions.len() < before {
                            tracing::warn!(
                                target: LIFECYCLE_LOG_TARGET,
                                run_id,
                                step,
                                provider = %provider.name(),
                                "Provider route does not support tool use; retrying without web_search"
                            );
                            run.emit(AgentState::Processing {
                                thought_summary: Some("This model route cannot use web search. Retrying with supported desktop tools...".to_string()),
                            });
                            continue;
                        }
                    }
                    if error_text.contains("API error (400)") && has_images {
                        for message in &mut messages {
                            if message.images.as_ref().is_some_and(|images| !images.is_empty()) {
                                message.images = None;
                                if message.content.trim() == "Visual screen observation:" {
                                    message.content = "Visual screen observation unavailable: the configured provider rejected image input. Continue using available textual tool results.".to_string();
                                }
                            }
                        }
                        tracing::warn!(
                            target: LIFECYCLE_LOG_TARGET,
                            run_id,
                            step,
                            provider = %provider.name(),
                            error = %error_text,
                            "Provider rejected screenshot input; retrying without image payloads"
                        );
                        run.emit(AgentState::Processing {
                            thought_summary: Some("This provider rejected screenshot input. Retrying without the image...".to_string()),
                        });
                        continue;
                    }

                    tracing::error!(
                        target: LIFECYCLE_LOG_TARGET,
                        run_id,
                        step,
                        provider = %provider.name(),
                        elapsed_ms = request_started.elapsed().as_millis() as u64,
                        error = %e,
                        "provider request failed"
                    );
                    return Err(RunFailure::new(
                        AgentError::Provider(e.to_string()),
                        &history_tail,
                    ));
                }
            };

            let response_msg = response.message;
            let tool_call_count = response_msg.tool_calls.as_ref().map_or(0, Vec::len);
            tracing::info!(
                target: LIFECYCLE_LOG_TARGET,
                run_id,
                step,
                provider = %provider.name(),
                elapsed_ms = request_started.elapsed().as_millis() as u64,
                content_chars = response_msg.content.chars().count(),
                tool_calls = tool_call_count,
                finish_reason = ?response.finish_reason,
                "provider request completed"
            );

            if run.is_canceled() {
                return Err(RunFailure::new(AgentError::Canceled, &history_tail));
            }

            if tool_call_count == 0 {
                if response_msg.content.trim().is_empty() {
                    return Err(RunFailure::new(
                        AgentError::Provider(
                            "The model returned an empty response. Please try again.".to_string(),
                        ),
                        &history_tail,
                    ));
                }
                final_result = response_msg.content.clone();
                messages.push(response_msg);
                break;
            }

            let tool_calls = response_msg.tool_calls.clone().unwrap_or_default();

            if let Some(choice_call) = tool_calls.iter().find(|call| call.name == "request_user_choice") {
                if tool_calls.len() != 1 {
                    return Err(RunFailure::new(AgentError::Tool(
                        "request_user_choice must be the only tool call in its response".into(),
                    ), &history_tail));
                }
                let (question, options) = parse_choice_request(&choice_call.arguments)
                    .map_err(|error| RunFailure::new(error, &history_tail))?;
                messages.push(response_msg);
                let history = messages.into_iter().skip(1).collect();
                return Ok(RunOutput {
                    reply: question.clone(),
                    history,
                    pending_choice: Some(PendingChoice {
                        question,
                        options,
                        tool_call_id: choice_call.id.clone(),
                    }),
                    selected_window_id: selected_window,
                });
            }

            // Create signature for current tool calls to detect repeated tool-call loops
            let current_sig = tool_calls
                .iter()
                .map(|c| format!("{}:{}", c.name, c.arguments))
                .collect::<Vec<_>>()
                .join("|");

            if !current_sig.is_empty() && current_sig == last_step_tool_signature {
                consecutive_repeated_tool_calls += 1;
            } else {
                consecutive_repeated_tool_calls = 1;
                last_step_tool_signature = current_sig.clone();
            }

            if consecutive_repeated_tool_calls >= 3 {
                let loop_msg = format!(
                    "Identical tool call repeated {} times: {}",
                    consecutive_repeated_tool_calls, tool_calls[0].name
                );
                tracing::warn!(
                    target: LIFECYCLE_LOG_TARGET,
                    run_id,
                    step,
                    tool = %tool_calls[0].name,
                    repeated_count = consecutive_repeated_tool_calls,
                    "Repeated tool-call loop detected"
                );
                return Err(RunFailure::new(
                    AgentError::RepeatedToolCalls(loop_msg),
                    &history_tail,
                ));
            }

            messages.push(response_msg);

            for call in &tool_calls {
                if !run.wait_until_resumed().await {
                    return Err(RunFailure::new(AgentError::Canceled, &history_tail));
                }

                run.emit(AgentState::Acting {
                    action_description: describe_tool_call(call),
                });

                let mut scoped_call = call.clone();
                if let Err(error) = enforce_background_input_scope(&mut scoped_call, &mut selected_window) {
                    messages.push(ChatMessage::tool(
                        call.id.clone(),
                        serde_json::json!({"error":error.to_string()}).to_string(),
                    ));
                    tracing::warn!(target: LIFECYCLE_LOG_TARGET, run_id, tool = %call.name, error = %error, "blocked out-of-context desktop action");
                    continue;
                }

                let tool_started = Instant::now();
                tracing::info!(
                    target: LIFECYCLE_LOG_TARGET,
                    run_id,
                    step,
                    tool = %call.name,
                    argument_bytes = call.arguments.to_string().len(),
                    "tool call started"
                );

                let result = match self.run_tool(run, &scoped_call).await {
                    Some(result) => result,
                    None => {
                        tracing::info!(
                            target: LIFECYCLE_LOG_TARGET,
                            run_id,
                            step,
                            tool = %call.name,
                            elapsed_ms = tool_started.elapsed().as_millis() as u64,
                            "tool call abandoned after cancellation"
                        );
                        return Err(RunFailure::new(AgentError::Canceled, &history_tail));
                    }
                };

                if let (Some(window_id), Some(title), Some(encoded)) = (
                    result.output.get("window_id").and_then(|value| value.as_u64()).and_then(|value| u32::try_from(value).ok()),
                    result.output.get("title").and_then(|value| value.as_str()),
                    result.output.get("base64").and_then(|value| value.as_str()),
                ) {
                    use base64::Engine;
                    if let Ok(frame) = base64::engine::general_purpose::STANDARD.decode(encoded) {
                        run.emit(AgentState::ComputerUsePreview {
                            target_window_id: window_id,
                            target_title: title.to_string(),
                            frame,
                            cursor_x: result.output.get("cursor_x").and_then(|value| value.as_i64()).map(|value| value as i32),
                            cursor_y: result.output.get("cursor_y").and_then(|value| value.as_i64()).map(|value| value as i32),
                        });
                    }
                }

                tracing::info!(
                    target: LIFECYCLE_LOG_TARGET,
                    run_id,
                    step,
                    tool = %call.name,
                    success = result.success,
                    summary = %result.summary,
                    elapsed_ms = tool_started.elapsed().as_millis() as u64,
                    "tool call completed"
                );

                // Extract screenshot base64 if visual observation was captured
                let screenshot_base64 = result.output.get("base64").and_then(|v| v.as_str()).map(str::to_string);

                // Screenshot bytes are attached to the following image message. Never
                // also serialize the same base64 payload into the tool-result text: it
                // balloons requests and still survives image-only fallback retries.
                let mut tool_output = result.output.clone();
                if screenshot_base64.is_some() {
                    if let Some(object) = tool_output.as_object_mut() {
                        object.remove("base64");
                    }
                }
                messages.push(ChatMessage::tool(
                    call.id.clone(),
                    tool_output.to_string(),
                ));

                if let Some(b64) = screenshot_base64 {
                    messages.push(ChatMessage::user_with_images(
                        "Visual screen observation:",
                        vec![b64],
                    ));
                }
            }

            run.emit(AgentState::Processing {
                thought_summary: Some("Evaluating progress...".to_string()),
            });
        }

        if final_result.is_empty() {
            tracing::warn!(
                target: LIFECYCLE_LOG_TARGET,
                run_id,
                step_limit = self.max_steps,
                "Agent execution step limit reached"
            );
            return Err(RunFailure::new(
                AgentError::StepLimitExceeded,
                &history_tail,
            ));
        }

        // Append the assistant reply to the history tail for the caller to store
        history_tail.push(ChatMessage::assistant(final_result.clone()));
        Ok(RunOutput {
            reply: final_result,
            history: history_tail,
            pending_choice: None,
            selected_window_id: selected_window,
        })
    }

    /// Run a tool off the async worker threads so blocking tool code (shell
    /// commands, AppleScript, screen capture) cannot stall the runtime, and so
    /// cancellation and the tool timeout can always interrupt the wait.
    /// Returns `None` if the run was canceled while the tool was running.
    async fn run_tool(
        &self,
        run: &RunHandle,
        call: &function_providers::ToolCall,
    ) -> Option<ToolResult> {
        let ctx = ToolContext {
            session_id: "default".to_string(),
            // Function is configured as a fully agentic assistant.
            // Tool implementations still enforce RESTRICTED actions,
            // but confirmation-gated actions are allowed after the
            // user explicitly enabled maximum agentic permissions.
            allow_sensitive: true,
        };
        let tools = self.tools.clone();
        let name = call.name.clone();
        let arguments = call.arguments.clone();
        let runtime = tokio::runtime::Handle::current();
        let task = tokio::task::spawn_blocking(move || {
            runtime.block_on(async move { tools.execute(&name, arguments, &ctx).await })
        });

        let outcome = tokio::select! {
            biased;
            _ = run.canceled() => return None,
            outcome = tokio::time::timeout(TOOL_EXECUTION_TIMEOUT, task) => outcome,
        };

        let result = match outcome {
            Ok(Ok(Ok(res))) => res,
            Ok(Ok(Err(function_tools::ToolError::RequiresConfirmation))) => {
                run.emit(AgentState::WaitingForConfirmation {
                    action: call.name.clone(),
                    details: format!("{}", call.arguments),
                });
                ToolResult::failure(
                    format!("Action '{}' requires user confirmation", call.name),
                    "Action paused: Operation requires explicit user confirmation before executing.",
                )
            }
            Ok(Ok(Err(e))) => {
                ToolResult::failure(format!("Failed to run {}", call.name), e.to_string())
            }
            Ok(Err(join_error)) => {
                tracing::error!(
                    target: LIFECYCLE_LOG_TARGET,
                    run_id = run.run_id,
                    tool = %call.name,
                    error = %join_error,
                    "tool call panicked"
                );
                ToolResult::failure(
                    format!("Failed to run {}", call.name),
                    format!("Tool crashed: {join_error}"),
                )
            }
            Err(_) => {
                tracing::warn!(
                    target: LIFECYCLE_LOG_TARGET,
                    run_id = run.run_id,
                    tool = %call.name,
                    timeout_secs = TOOL_EXECUTION_TIMEOUT.as_secs(),
                    "tool call timed out"
                );
                ToolResult::failure(
                    format!("{} timed out", call.name),
                    format!(
                        "Tool did not finish within {} seconds",
                        TOOL_EXECUTION_TIMEOUT.as_secs()
                    ),
                )
            }
        };
        Some(result)
    }
}

fn limit_spoken_sentences(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut sentence_count = 0;
    for (index, character) in text.char_indices() {
        if matches!(character, '.' | '!' | '?')
            && text[index + character.len_utf8()..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace)
        {
            sentence_count += 1;
            if sentence_count == 2 {
                return text[..index + character.len_utf8()].trim().to_string();
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use function_memory::InMemoryMemoryStore;
    use function_providers::MockLlmProvider;

    #[test]
    fn parses_numbered_clarification_choices_from_assistant_text() {
        let result = parse_clarification_options(
            "Research is still loading.\n\nOptions:\n\n1. Read the full answer first.\n\n2. Write a clearly labeled draft.\n\nWhich would you like? I will continue afterward.",
        )
        .expect("question with options should be interactive");

        assert_eq!(
            result.display_text,
            "Research is still loading."
        );
        assert_eq!(result.question, "Which would you like? I will continue afterward.");
        assert_eq!(
            result.options,
            ["Read the full answer first.", "Write a clearly labeled draft."]
        );
    }

    #[test]
    fn does_not_convert_an_unstructured_question_to_choices() {
        assert!(parse_clarification_options("Which file should I use?").is_none());
    }

    struct ChoiceRequestProvider;

    #[async_trait::async_trait]
    impl LlmProvider for ChoiceRequestProvider {
        fn name(&self) -> &str { "choice-request-test" }
        fn as_any(&self) -> &dyn std::any::Any { self }

        async fn complete(&self, request: CompletionRequest) -> Result<function_providers::CompletionResponse, function_providers::ProviderError> {
            assert!(request.tools.iter().any(|tool| tool.name == "request_user_choice"));
            Ok(function_providers::CompletionResponse {
                message: ChatMessage {
                    role: function_providers::MessageRole::Assistant,
                    content: String::new(),
                    images: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![function_providers::ToolCall {
                        id: "choice_1".into(),
                        name: "request_user_choice".into(),
                        arguments: serde_json::json!({"question":"Which source should I use?","options":["Official announcement","Wait for research"]}),
                        thought_signature: None,
                    }]),
                    thought_signature: None,
                },
                finish_reason: Some("tool_calls".into()),
            })
        }
    }

    #[tokio::test]
    async fn choice_tool_emits_typed_pending_input_and_stops_the_run() {
        let agent = Agent::new(
            Arc::new(ChoiceRequestProvider),
            ToolRegistry::new(),
            Arc::new(InMemoryMemoryStore::new()),
        );
        let mut states = agent.subscribe_state();

        let (_, history) = agent.execute_with_history_in_window("Research this", vec![], Some(19)).await.unwrap();
        assert!(matches!(history.last(), Some(ChatMessage { tool_calls: Some(_), .. })));

        let mut choice = None;
        let mut completed = false;
        while let Ok(state) = states.try_recv() {
            if let AgentState::NeedsUserInput { question, options, tool_call_id, selected_window_id, .. } = state {
                choice = Some((question, options, tool_call_id, selected_window_id));
            } else if matches!(state, AgentState::Completed { .. }) {
                completed = true;
            }
        }
        assert!(!completed, "a pending choice must stop before ordinary completion");
        assert_eq!(choice, Some((
            "Which source should I use?".into(),
            vec!["Official announcement".into(), "Wait for research".into()],
            "choice_1".into(),
            Some(19),
        )));
    }

    #[test]
    fn choice_requests_reject_invalid_option_counts_and_empty_labels() {
        assert!(parse_choice_request(&serde_json::json!({"question":"Pick?","options":["Only one"]})).is_err());
        assert!(parse_choice_request(&serde_json::json!({"question":"Pick?","options":["A", " "]})).is_err());
    }

    #[test]
    fn malformed_target_id_never_falls_back_to_global_input() {
        let mut selected = None;
        let mut call = function_providers::ToolCall {
            id: "bad-target".into(), name: "computer_mouse".into(),
            arguments: serde_json::json!({"action":"click","target_window_id":-1,"x":1,"y":1}),
            thought_signature: None,
        };
        assert!(enforce_background_input_scope(&mut call, &mut selected).is_err());
        assert_eq!(selected, None);
    }

    #[test]
    fn selecting_an_option_returns_the_pending_tool_call_result() {
        let response = choice_result_message("choice_1", "Official announcement");
        assert_eq!(response.role, function_providers::MessageRole::Tool);
        assert_eq!(response.tool_call_id.as_deref(), Some("choice_1"));
        assert_eq!(serde_json::from_str::<serde_json::Value>(&response.content).unwrap(),
            serde_json::json!({"user_response":"Official announcement"}));
    }

    #[test]
    fn selected_background_window_scopes_low_level_input_and_blocks_global_input() {
        let mut selected = None;
        let mut observe = function_providers::ToolCall {
            id: "1".into(), name: "computer_screen".into(),
            arguments: serde_json::json!({"action":"target_screenshot","window_id":42}), thought_signature: None,
        };
        enforce_background_input_scope(&mut observe, &mut selected).unwrap();
        assert_eq!(selected, Some(42));

        let mut type_text = function_providers::ToolCall {
            id: "2".into(), name: "computer_keyboard".into(),
            arguments: serde_json::json!({"action":"type","text":"private test"}), thought_signature: None,
        };
        enforce_background_input_scope(&mut type_text, &mut selected).unwrap();
        assert_eq!(type_text.arguments["target_window_id"], 42);

        let mut mouse = function_providers::ToolCall {
            id: "mouse".into(), name: "computer_mouse".into(),
            arguments: serde_json::json!({"action":"click","x":12,"y":18}), thought_signature: None,
        };
        enforce_background_input_scope(&mut mouse, &mut selected).unwrap();
        assert_eq!(mouse.arguments["target_window_id"], 42);

        let mut global_click = function_providers::ToolCall {
            id: "3".into(), name: "click".into(),
            arguments: serde_json::json!({"x":10,"y":20}), thought_signature: None,
        };
        assert!(enforce_background_input_scope(&mut global_click, &mut selected).is_err());

        let mut focus_window = function_providers::ToolCall {
            id: "focus".into(), name: "computer_apps".into(),
            arguments: serde_json::json!({"action":"focus_window","window_title":"TextEdit"}), thought_signature: None,
        };
        assert!(enforce_background_input_scope(&mut focus_window, &mut selected).is_err());

        let mut switch = function_providers::ToolCall {
            id: "4".into(), name: "computer_mouse".into(),
            arguments: serde_json::json!({"action":"click","target_window_id":77,"x":5,"y":6}), thought_signature: None,
        };
        enforce_background_input_scope(&mut switch, &mut selected).unwrap();
        assert_eq!(selected, Some(77));
    }

    #[test]
    fn speech_text_is_limited_to_two_sentences() {
        assert_eq!(
            limit_spoken_sentences("First answer. Second answer! Third answer."),
            "First answer. Second answer!"
        );
    }

    #[tokio::test]
    async fn test_agent_execution() {
        let provider = Arc::new(MockLlmProvider::new("Task completed successfully"));
        let tools = ToolRegistry::new();
        let memory = Arc::new(InMemoryMemoryStore::new());
        let agent = Agent::new(provider, tools, memory);

        let mut rx = agent.subscribe_state();
        let result = agent.execute_task("Check status").await.unwrap();
        assert_eq!(result, "Task completed successfully");

        let state = rx.recv().await.unwrap();
        assert!(matches!(state, AgentState::Processing { .. }));
    }

    struct RepeatingToolMockProvider;

    #[async_trait::async_trait]
    impl function_providers::LlmProvider for RepeatingToolMockProvider {
        fn name(&self) -> &str {
            "repeating-tool-mock"
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        async fn complete(
            &self,
            _req: function_providers::CompletionRequest,
        ) -> Result<function_providers::CompletionResponse, function_providers::ProviderError>
        {
            Ok(function_providers::CompletionResponse {
                message: ChatMessage {
                    role: function_providers::MessageRole::Assistant,
                    content: String::new(),
                    images: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![function_providers::ToolCall {
                        id: "call_repeat_1".to_string(),
                        name: "execute_command".to_string(),
                        arguments: serde_json::json!({ "command": "echo test" }),
                        thought_signature: None,
                    }]),
                    thought_signature: None,
                },
                finish_reason: Some("tool_calls".to_string()),
            })
        }
    }

    #[tokio::test]
    async fn test_agent_max_steps_configuration_and_preserves_history() {
        let provider = Arc::new(RepeatingToolMockProvider);
        let tools = ToolRegistry::new();
        let memory = Arc::new(InMemoryMemoryStore::new());
        let agent = Agent::new(provider, tools, memory).with_max_steps(2);

        let mut rx = agent.subscribe_state();
        let result = agent.execute_task("Run infinite loop").await;

        assert!(matches!(result, Err(AgentError::StepLimitExceeded)));

        let mut found_error_with_history = false;
        while let Ok(state) = rx.try_recv() {
            if let AgentState::Error { message, new_history } = state {
                assert!(message.contains("Execution step limit exceeded"));
                assert!(new_history.is_some());
                let hist = new_history.unwrap();
                assert!(!hist.is_empty());
                found_error_with_history = true;
            }
        }
        assert!(found_error_with_history);
    }

    #[tokio::test]
    async fn test_agent_detects_repeated_tool_call_loop() {
        let provider = Arc::new(RepeatingToolMockProvider);
        let tools = ToolRegistry::new();
        let memory = Arc::new(InMemoryMemoryStore::new());
        let agent = Agent::new(provider, tools, memory).with_max_steps(10);

        let mut rx = agent.subscribe_state();
        let result = agent.execute_task("Do repeating tool call").await;

        assert!(matches!(result, Err(AgentError::RepeatedToolCalls(_))));

        let mut found_error_with_history = false;
        while let Ok(state) = rx.try_recv() {
            if let AgentState::Error { message, new_history } = state {
                assert!(message.contains("Repeated tool call loop detected"));
                assert!(new_history.is_some());
                found_error_with_history = true;
            }
        }
        assert!(found_error_with_history);
    }

    #[tokio::test]
    async fn test_agent_can_replace_provider_at_runtime() {
        let initial = Arc::new(MockLlmProvider::new("initial response"));
        let replacement = Arc::new(MockLlmProvider::new("replacement response"));
        let tools = ToolRegistry::new();
        let memory = Arc::new(InMemoryMemoryStore::new());
        let agent = Agent::new(initial, tools, memory);

        agent.set_provider(replacement);

        let response = agent.execute_task("hello").await.unwrap();
        assert_eq!(response, "replacement response");
    }
}
