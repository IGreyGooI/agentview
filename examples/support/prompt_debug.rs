//! Bounded, file-backed request capture for live OpenAI Responses examples.
//!
//! The observer receives an already handed-off Frame-native request body. It
//! contains JSON request data only, never HTTP headers or credentials. Call
//! [`PromptDebugCapture::finish`] after the provider has been dropped so the
//! background writer can drain its queue and join before process exit.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
        Arc,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use agentview::provider::async_openai::OpenAiResponsesRequestSnapshot;
use anyhow::Context as _;
use serde_json::{Map, Value};

const REQUEST_QUEUE_CAPACITY: usize = 8;

/// Owns a bounded queue and background writer for exact Responses request bodies.
pub struct PromptDebugCapture {
    run_dir: PathBuf,
    sender: Option<SyncSender<QueuedRequest>>,
    worker: Option<thread::JoinHandle<WriterReport>>,
    next_request: Arc<AtomicU64>,
    queue_status: Arc<QueueStatus>,
}

impl PromptDebugCapture {
    /// Starts a capture run under `target/agentview-debug/<unique-run-id>`.
    pub fn new() -> anyhow::Result<Self> {
        let run_dir = create_run_dir()?;
        Self::with_run_dir(run_dir)
    }

    /// Returns the directory that will contain one numbered directory per request.
    pub fn output_dir(&self) -> &Path {
        &self.run_dir
    }

    /// Returns a bounded callback suitable for `with_request_observer`.
    ///
    /// The callback moves the owned snapshot through `try_send`; it never
    /// performs filesystem I/O or waits for the writer.
    pub fn observer(&self) -> impl Fn(OpenAiResponsesRequestSnapshot) + Send + Sync + 'static {
        let sender = self
            .sender
            .as_ref()
            .expect("PromptDebugCapture observer requested after finish")
            .clone();
        let next_request = Arc::clone(&self.next_request);
        let queue_status = Arc::clone(&self.queue_status);

        move |snapshot| {
            let ordinal = next_request.fetch_add(1, Ordering::Relaxed);
            let request = QueuedRequest { ordinal, snapshot };
            match sender.try_send(request) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    queue_status.full.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {
                    queue_status.disconnected.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Drains all queued request artifacts and joins the writer thread.
    ///
    /// The provider that owns the callback must be dropped before calling this
    /// method. The runnable example does that by moving the provider into its
    /// application driver before invoking `finish`.
    pub async fn finish(mut self) -> anyhow::Result<()> {
        drop(self.sender.take());
        let worker = self
            .worker
            .take()
            .context("prompt debug writer was already finished")?;
        let report = tokio::task::spawn_blocking(move || worker.join())
            .await
            .context("prompt debug writer join task failed")?
            .map_err(|_| anyhow::anyhow!("prompt debug writer panicked"))?;

        let queue_full = self.queue_status.full.load(Ordering::Relaxed);
        let disconnected = self.queue_status.disconnected.load(Ordering::Relaxed);
        if queue_full != 0 {
            eprintln!(
                "[agentview-debug] incomplete capture: queue was full {queue_full} time(s); \
                 those request snapshot(s) were dropped"
            );
        }
        if disconnected != 0 {
            eprintln!(
                "[agentview-debug] incomplete capture: writer was disconnected for {disconnected} \
                 request snapshot(s)"
            );
        }
        if !report.errors.is_empty() {
            eprintln!(
                "[agentview-debug] writer completed with {} error(s)",
                report.errors.len()
            );
        }

        if queue_full != 0 || disconnected != 0 || !report.errors.is_empty() {
            let mut problems = Vec::new();
            if queue_full != 0 {
                problems.push(format!("queue dropped {queue_full} request snapshot(s)"));
            }
            if disconnected != 0 {
                problems.push(format!(
                    "writer disconnected for {disconnected} request snapshot(s)"
                ));
            }
            problems.extend(report.errors);
            anyhow::bail!(
                "prompt debug capture was incomplete: {}",
                problems.join("; ")
            );
        }

        eprintln!(
            "[agentview-debug] flushed {} request(s) to {}",
            report.written,
            self.run_dir.display()
        );
        Ok(())
    }

    fn with_run_dir(run_dir: PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(&run_dir)
            .with_context(|| format!("create prompt debug directory {}", run_dir.display()))?;
        let (sender, receiver) = mpsc::sync_channel(REQUEST_QUEUE_CAPACITY);
        let queue_status = Arc::new(QueueStatus::default());
        let worker_status = Arc::clone(&queue_status);
        let worker_dir = run_dir.clone();
        let worker = thread::Builder::new()
            .name("agentview-prompt-debug".to_owned())
            .spawn(move || writer_loop(receiver, worker_dir, worker_status))
            .context("start prompt debug writer")?;

        Ok(Self {
            run_dir,
            sender: Some(sender),
            worker: Some(worker),
            next_request: Arc::new(AtomicU64::new(1)),
            queue_status,
        })
    }
}

impl Drop for PromptDebugCapture {
    fn drop(&mut self) {
        if self.worker.is_some() {
            eprintln!(
                "[agentview-debug] capture dropped before finish(); queued request artifacts may be lost"
            );
        }
    }
}

#[derive(Default)]
struct QueueStatus {
    full: AtomicUsize,
    disconnected: AtomicUsize,
}

struct QueuedRequest {
    ordinal: u64,
    snapshot: OpenAiResponsesRequestSnapshot,
}

#[derive(Default)]
struct WriterReport {
    written: usize,
    errors: Vec<String>,
}

fn writer_loop(
    receiver: Receiver<QueuedRequest>,
    run_dir: PathBuf,
    queue_status: Arc<QueueStatus>,
) -> WriterReport {
    let mut report = WriterReport::default();
    let mut reported_queue_full = 0;

    while let Ok(request) = receiver.recv() {
        report_queue_full(&queue_status, &mut reported_queue_full);
        let QueuedRequest { ordinal, snapshot } = request;
        let result = write_request(
            &run_dir,
            ordinal,
            snapshot.body(),
            &format!("{:?}", snapshot.frame_revision()),
            &format!("{:?}", snapshot.frame_basis()),
        );
        match result {
            Ok(()) => report.written += 1,
            Err(error) => {
                let message = format!("request artifact write failed: {error}");
                eprintln!("[agentview-debug] {message}");
                report.errors.push(message);
            }
        }
    }
    report_queue_full(&queue_status, &mut reported_queue_full);
    report
}

fn report_queue_full(queue_status: &QueueStatus, reported: &mut usize) {
    let current = queue_status.full.load(Ordering::Relaxed);
    if current > *reported {
        eprintln!(
            "[agentview-debug] queue full: {} request snapshot(s) dropped so far; \
             increase the writer capacity before relying on this capture",
            current
        );
        *reported = current;
    }
}

fn write_request(
    run_dir: &Path,
    ordinal: u64,
    body: &[u8],
    revision: &str,
    basis: &str,
) -> Result<(), String> {
    let request_dir = run_dir.join(format!("{ordinal:04}"));
    fs::create_dir_all(&request_dir)
        .map_err(|error| format!("create {}: {error}", request_dir.display()))?;
    let request_path = request_dir.join("request.json");
    let prompt_path = request_dir.join("prompt.txt");
    let prompt = render_readable_prompt(body, revision, basis);

    eprintln!(
        "[agentview-debug] request {ordinal:04} handed_off\n  request.json: {}\n  prompt.txt: {}\n\n{prompt}",
        request_path.display(),
        prompt_path.display(),
    );

    let mut failures = Vec::new();
    if let Err(error) = fs::write(&request_path, body) {
        failures.push(format!("write {}: {error}", request_path.display()));
    }
    if let Err(error) = fs::write(&prompt_path, prompt) {
        failures.push(format!("write {}: {error}", prompt_path.display()));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

pub(crate) fn render_readable_prompt(body: &[u8], revision: &str, basis: &str) -> String {
    let mut output = String::new();
    let _ = writeln!(&mut output, "# AgentView Responses request");
    let _ = writeln!(&mut output, "handoff: handed_off");
    let _ = writeln!(&mut output, "frame revision: {revision}");
    let _ = writeln!(&mut output, "frame basis: {basis}\n");
    let _ = writeln!(
        &mut output,
        "scope: exact outbound provider request\ncontext: may include earlier states, messages, and tool interactions beyond the current Component requirements\n"
    );

    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(request)) => render_request(&mut output, &request),
        Ok(value) => append_section(
            &mut output,
            "Request body (unexpected JSON shape)",
            &pretty_json(&value),
        ),
        Err(error) => {
            let text = String::from_utf8_lossy(body);
            append_section(
                &mut output,
                "Request body (invalid JSON)",
                &format!("parse error: {error}\n\n{text}"),
            );
        }
    }
    output
}

fn render_request(output: &mut String, request: &Map<String, Value>) {
    if let Some(model) = request.get("model") {
        append_section(output, "Model", &display_value(model));
    }
    if let Some(instructions) = request.get("instructions") {
        append_section(output, "System instructions", &display_value(instructions));
    }
    match request.get("input") {
        Some(Value::Array(items)) if items.is_empty() => append_section(output, "Input", "(empty)"),
        Some(Value::Array(items)) => {
            for (index, item) in items.iter().enumerate() {
                render_input_item(output, index + 1, item);
            }
        }
        Some(value) => append_section(output, "Input (unexpected JSON shape)", &pretty_json(value)),
        None => append_section(output, "Input", "(missing)"),
    }
    match request.get("tools") {
        Some(Value::Array(tools)) if tools.is_empty() => append_section(output, "Tools", "(none)"),
        Some(Value::Array(tools)) => {
            for (index, tool) in tools.iter().enumerate() {
                render_tool(output, index + 1, tool);
            }
        }
        Some(value) => append_section(output, "Tools (unexpected JSON shape)", &pretty_json(value)),
        None => append_section(output, "Tools", "(missing)"),
    }

    let settings = request
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "model" | "instructions" | "input" | "tools"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<_, _>>();
    if !settings.is_empty() {
        append_section(
            output,
            "Other request settings",
            &pretty_json(&Value::Object(settings)),
        );
    }
}

fn render_input_item(output: &mut String, index: usize, item: &Value) {
    let Some(object) = item.as_object() else {
        append_section(
            output,
            &unknown_input_heading(index, None),
            &pretty_json(item),
        );
        return;
    };
    match object.get("type").and_then(Value::as_str) {
        Some("message") => render_message(output, index, object, item),
        Some("function_call") => render_tool_call(output, index, object),
        Some("function_call_output") => render_tool_result(output, index, object),
        _ => append_section(
            output,
            &unknown_input_heading(index, object.get("type").and_then(Value::as_str)),
            &pretty_json(item),
        ),
    }
}

fn unknown_input_heading(index: usize, kind: Option<&str>) -> String {
    match kind {
        Some(kind) => format!("Input item {index} (unknown type: {kind})"),
        None => format!("Input item {index} (unknown)"),
    }
}

fn render_message(output: &mut String, index: usize, object: &Map<String, Value>, item: &Value) {
    let Some(role) = object.get("role").and_then(Value::as_str) else {
        append_section(
            output,
            &format!("Input item {index} (unknown message)"),
            &pretty_json(item),
        );
        return;
    };
    let Some(content) = object.get("content") else {
        append_section(
            output,
            &format!("{role} message {index} (unknown)"),
            &pretty_json(item),
        );
        return;
    };

    let mut rendered = String::new();
    if let Some(status) = object.get("status") {
        let _ = writeln!(&mut rendered, "status: {}\n", display_value(status));
    }
    if let Some(phase) = object.get("phase") {
        let _ = writeln!(&mut rendered, "phase: {}\n", display_value(phase));
    }
    render_message_content(&mut rendered, content);
    append_metadata(
        &mut rendered,
        object,
        &["type", "role", "content", "status", "phase"],
    );
    append_section(
        output,
        &format!("{} message {index}", display_role(role)),
        &rendered,
    );
}

fn render_message_content(output: &mut String, content: &Value) {
    match content {
        Value::String(text) => output.push_str(text),
        Value::Array(parts) if parts.is_empty() => output.push_str("(empty)"),
        Value::Array(parts) => {
            for (index, part) in parts.iter().enumerate() {
                if index != 0 {
                    output.push_str("\n\n");
                }
                let Some(object) = part.as_object() else {
                    output.push_str(&pretty_json(part));
                    continue;
                };
                match object.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("output_text") => match object.get("text") {
                        Some(text) => output.push_str(&display_value(text)),
                        None => output.push_str(&pretty_json(part)),
                    },
                    Some("refusal") => match object.get("refusal") {
                        Some(refusal) => output.push_str(&display_value(refusal)),
                        None => output.push_str(&pretty_json(part)),
                    },
                    _ => output.push_str(&pretty_json(part)),
                }
            }
        }
        value => output.push_str(&pretty_json(value)),
    }
}

fn render_tool_call(output: &mut String, index: usize, object: &Map<String, Value>) {
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let call_id = object
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let arguments = object
        .get("arguments")
        .map(display_value)
        .unwrap_or_else(|| "(missing)".to_owned());
    let mut rendered = format!("call_id: {call_id}\narguments:\n{arguments}");
    append_metadata(
        &mut rendered,
        object,
        &["type", "name", "call_id", "arguments"],
    );
    append_section(output, &format!("Tool call {index}: {name}"), &rendered);
}

fn render_tool_result(output: &mut String, index: usize, object: &Map<String, Value>) {
    let call_id = object
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let result = object
        .get("output")
        .map(display_value)
        .unwrap_or_else(|| "(missing)".to_owned());
    let mut rendered = format!("call_id: {call_id}\nresult:\n{result}");
    append_metadata(&mut rendered, object, &["type", "call_id", "output"]);
    append_section(output, &format!("Tool result {index}"), &rendered);
}

fn render_tool(output: &mut String, index: usize, tool: &Value) {
    let Some(object) = tool.as_object() else {
        append_section(
            output,
            &format!("Tool {index} (unknown)"),
            &pretty_json(tool),
        );
        return;
    };
    if object.get("type").and_then(Value::as_str) != Some("function") {
        append_section(
            output,
            &format!("Tool {index} (unknown)"),
            &pretty_json(tool),
        );
        return;
    }
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let description = object
        .get("description")
        .map(display_value)
        .unwrap_or_else(|| "(missing)".to_owned());
    let parameters = object
        .get("parameters")
        .map(pretty_json)
        .unwrap_or_else(|| "(missing)".to_owned());
    let mut rendered = format!("description:\n{description}\n\nparameters:\n{parameters}");
    if let Some(strict) = object.get("strict") {
        let _ = write!(&mut rendered, "\n\nstrict: {}", display_value(strict));
    }
    append_metadata(
        &mut rendered,
        object,
        &["type", "name", "description", "parameters", "strict"],
    );
    append_section(output, &format!("Tool {index}: {name}"), &rendered);
}

fn append_metadata(output: &mut String, object: &Map<String, Value>, known: &[&str]) {
    let metadata = object
        .iter()
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<_, _>>();
    if !metadata.is_empty() {
        let _ = write!(
            output,
            "\n\nmetadata:\n{}",
            pretty_json(&Value::Object(metadata))
        );
    }
}

fn append_section(output: &mut String, heading: &str, content: &str) {
    let _ = writeln!(output, "## {heading}\n");
    output.push_str(content);
    if !content.ends_with('\n') {
        output.push('\n');
    }
    output.push('\n');
}

fn display_role(role: &str) -> &str {
    match role {
        "system" => "System",
        "developer" => "Developer",
        "user" => "User",
        "assistant" => "Assistant",
        _ => role,
    }
}

fn display_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        value => pretty_json(value),
    }
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn create_run_dir() -> anyhow::Result<PathBuf> {
    let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/agentview-debug");
    fs::create_dir_all(&parent)
        .with_context(|| format!("create prompt debug root {}", parent.display()))?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = process::id();
    for attempt in 0_u32..1024 {
        let run_dir = parent.join(format!("run-{timestamp}-{pid}-{attempt}"));
        match create_private_directory(&run_dir) {
            Ok(()) => return Ok(run_dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create prompt debug run {}", run_dir.display()));
            }
        }
    }
    anyhow::bail!("could not allocate a unique prompt debug run directory")
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)
}
