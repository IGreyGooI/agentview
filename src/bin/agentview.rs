use std::collections::VecDeque;
use std::env;
use std::io::{BufRead as _, Read, Write as _};
use std::net::SocketAddr;
use std::panic::{self, AssertUnwindSafe};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::Duration;

use agentview::component::{
    execution::{
        ExternalAct, ExternalApplication, ExternalApplicationFault, ExternalObservation,
        ExternalObservationKind, ProviderEvent,
    },
    prelude::*,
};
use agentview::{
    pom::{Document, TextNode, XmlNode},
    pom_resolution::resolve_artifact_document,
    transcript::{CanonicalInputItem, ConversationRole},
};
use anyhow::Context;
use futures::FutureExt as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio::time::timeout;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const INTERNAL_DAEMON_ARG: &str = "--__agentview-daemon";
const INTERNAL_SHUTDOWN_ARG: &str = "--__agentview-shutdown";
const ADDR_ENV: &str = "AGENTVIEW_ADDR";
const TOKEN_ENV: &str = "AGENTVIEW_TOKEN";
const LLVM_PROFILE_FILE_ENV: &str = "LLVM_PROFILE_FILE";
const DEFAULT_DAEMON_LLVM_PROFILE_FILE: &str = "target/llvm-profraw/daemon-%p-%m.profraw";
const DEFAULT_ADDR: &str = "127.0.0.1:47631";
const DAEMON_CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const DAEMON_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
// A legal multi-megabyte act can spend several seconds in local projection
// reconciliation and JSON encoding before the response reaches the socket.
const DAEMON_RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const DAEMON_RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_PENDING_AUTHENTICATIONS: usize = 32;
const MAX_DAEMON_AUTH_LINE_BYTES: usize = 4096;
const MAX_PROTOCOL_WIRE_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROTOCOL_FRAMES: usize = 65_536;
const MAX_DAEMON_LINE_BYTES: usize = MAX_PROTOCOL_WIRE_BYTES * 6 + 4096;
const MAX_DAEMON_RESPONSE_JSON_BYTES: usize = MAX_DAEMON_LINE_BYTES - 1;
const DAEMON_JSON_BUFFER_BYTES: usize = 64 * 1024;
const DAEMON_REQUEST_HMAC_BUFFER_BYTES: usize = 64 * 1024;
const MAX_EXTERNAL_CLI_STATE_BYTES: usize = 8 * 1024 * 1024;
const EXTERNAL_CLI_MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
const EXTERNAL_CLI_MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
const EXTERNAL_CLI_MAX_COMPONENT_BYTES: usize = 16 * 1024 * 1024;
const MIN_SESSION_TOKEN_BYTES: usize = 32;
const MAX_SESSION_TOKEN_BYTES: usize = 1024;
const SERVER_PROOF_LABEL: &[u8] = b"agentview-daemon-server-v1";
const CLIENT_AUTH_LABEL: &[u8] = b"agentview-daemon-client-auth-v1";
const CLIENT_PROOF_LABEL: &[u8] = b"agentview-daemon-client-v1";

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
struct ExternalCliProps {
    snapshot: Arc<Mutex<ExternalCliState>>,
    snapshot_hidden: Arc<AtomicBool>,
    exported_state: Arc<Mutex<Option<Signal<ExternalCliComponentState>>>>,
    prepared_progress: Arc<OnceLock<Arc<ExternalCliPreparedProgress>>>,
    #[cfg(test)]
    panic_on_text: Option<&'static str>,
    #[cfg(test)]
    error_on_text: Option<&'static str>,
    #[cfg(test)]
    panic_after_speculation_started: Option<Arc<ExternalCliSpeculationBarrier>>,
}

const EXTERNAL_CLI_STATE_SEPARATOR: &str = " | ";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExternalCliTextKind {
    Delta,
    Complete,
}

impl ExternalCliTextKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Delta => "delta:",
            Self::Complete => "complete:",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ExternalCliEventContent {
    Raw(String),
    Prefixed {
        kind: ExternalCliTextKind,
        text: Arc<String>,
    },
}

impl ExternalCliEventContent {
    fn len(&self) -> usize {
        match self {
            Self::Raw(rendered) => rendered.len(),
            Self::Prefixed { kind, text } => kind.prefix().len() + text.len(),
        }
    }

    fn append_to(&self, rendered: &mut String) {
        match self {
            Self::Raw(event) => rendered.push_str(event),
            Self::Prefixed { kind, text } => {
                rendered.push_str(kind.prefix());
                rendered.push_str(text);
            }
        }
    }

    fn into_rendered(self) -> String {
        match self {
            Self::Raw(rendered) => rendered,
            Self::Prefixed { kind, text } => {
                let mut rendered = String::with_capacity(kind.prefix().len() + text.len());
                rendered.push_str(kind.prefix());
                rendered.push_str(&text);
                rendered
            }
        }
    }

    #[cfg(test)]
    fn rendered(&self) -> String {
        let mut rendered = String::with_capacity(self.len());
        self.append_to(&mut rendered);
        rendered
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExternalCliEvent {
    rendered: ExternalCliEventContent,
    canonical_content_bytes: usize,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct ExternalCliWorkDiagnostics {
    entry_canonicalizations: std::sync::atomic::AtomicUsize,
    entry_canonical_input_bytes: std::sync::atomic::AtomicUsize,
    content_serializations: std::sync::atomic::AtomicUsize,
    full_snapshot_renders: std::sync::atomic::AtomicUsize,
    visibility_signal_updates: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExternalCliWorkSnapshot {
    entry_canonicalizations: usize,
    entry_canonical_input_bytes: usize,
    content_serializations: usize,
    full_snapshot_renders: usize,
    visibility_signal_updates: usize,
}

#[cfg(test)]
impl ExternalCliWorkDiagnostics {
    fn reset(&self) {
        for counter in [
            &self.entry_canonicalizations,
            &self.entry_canonical_input_bytes,
            &self.content_serializations,
            &self.full_snapshot_renders,
            &self.visibility_signal_updates,
        ] {
            counter.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> ExternalCliWorkSnapshot {
        ExternalCliWorkSnapshot {
            entry_canonicalizations: self
                .entry_canonicalizations
                .load(std::sync::atomic::Ordering::Relaxed),
            entry_canonical_input_bytes: self
                .entry_canonical_input_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            content_serializations: self
                .content_serializations
                .load(std::sync::atomic::Ordering::Relaxed),
            full_snapshot_renders: self
                .full_snapshot_renders
                .load(std::sync::atomic::Ordering::Relaxed),
            visibility_signal_updates: self
                .visibility_signal_updates
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ExternalCliState {
    events: Arc<VecDeque<ExternalCliEvent>>,
    rendered_bytes: usize,
    canonical_content_bytes: usize,
    #[cfg(test)]
    diagnostics: Option<Arc<ExternalCliWorkDiagnostics>>,
}

impl PartialEq for ExternalCliState {
    fn eq(&self, other: &Self) -> bool {
        self.events == other.events
            && self.rendered_bytes == other.rendered_bytes
            && self.canonical_content_bytes == other.canonical_content_bytes
    }
}

impl Eq for ExternalCliState {}

impl ExternalCliState {
    #[cfg(test)]
    fn with_diagnostics() -> (Self, Arc<ExternalCliWorkDiagnostics>) {
        let diagnostics = Arc::new(ExternalCliWorkDiagnostics::default());
        (
            Self {
                diagnostics: Some(Arc::clone(&diagnostics)),
                ..Self::default()
            },
            diagnostics,
        )
    }

    fn rendered(&self) -> String {
        #[cfg(test)]
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics
                .full_snapshot_renders
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let mut rendered = String::with_capacity(self.rendered_bytes);
        for (index, event) in self.events.iter().enumerate() {
            if index > 0 {
                rendered.push_str(EXTERNAL_CLI_STATE_SEPARATOR);
            }
            event.rendered.append_to(&mut rendered);
        }
        rendered
    }

    fn push_external_text(
        &mut self,
        kind: ExternalCliTextKind,
        text: String,
    ) -> anyhow::Result<()> {
        self.push_external_text_shared(kind, Arc::new(text))
    }

    fn push_external_text_shared(
        &mut self,
        kind: ExternalCliTextKind,
        text: Arc<String>,
    ) -> anyhow::Result<()> {
        let entry = self.meter_prefixed_event(kind, text)?;
        self.push_metered_with_limits(
            entry,
            MAX_EXTERNAL_CLI_STATE_BYTES,
            max_external_cli_snapshot_item_bytes(),
        )
    }

    #[cfg(test)]
    fn push(&mut self, entry: String) -> anyhow::Result<()> {
        self.push_with_limits(
            entry,
            MAX_EXTERNAL_CLI_STATE_BYTES,
            max_external_cli_snapshot_item_bytes(),
        )
    }

    #[cfg(test)]
    fn push_with_limits(
        &mut self,
        entry: String,
        max_raw_bytes: usize,
        max_canonical_item_bytes: usize,
    ) -> anyhow::Result<()> {
        let entry = self.meter_event(bounded_suffix(entry, max_raw_bytes))?;
        self.push_metered_with_limits(entry, max_raw_bytes, max_canonical_item_bytes)
    }

    fn push_metered_with_limits(
        &mut self,
        mut entry: ExternalCliEvent,
        max_raw_bytes: usize,
        max_canonical_item_bytes: usize,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            external_state_empty_item_bytes() <= max_canonical_item_bytes,
            "external CLI canonical snapshot cap is smaller than its fixed envelope"
        );
        if entry.rendered.len() > max_raw_bytes {
            let rendered = bounded_suffix(entry.rendered.into_rendered(), max_raw_bytes);
            entry = self.meter_event(rendered)?;
        }
        if canonical_external_state_item_bytes_from_parts(
            entry.rendered.len(),
            entry.canonical_content_bytes,
        ) > max_canonical_item_bytes
        {
            entry =
                self.fit_event_suffix(entry.rendered.into_rendered(), max_canonical_item_bytes)?;
        }

        let separator_content_bytes = external_state_separator_content_bytes();
        let has_existing = !self.events.is_empty();
        let mut projected_raw_bytes = self
            .rendered_bytes
            .checked_add(entry.rendered.len())
            .and_then(|bytes| {
                bytes.checked_add(usize::from(has_existing) * EXTERNAL_CLI_STATE_SEPARATOR.len())
            })
            .context("external CLI raw snapshot accounting overflowed")?;
        let mut projected_canonical_content_bytes = self
            .canonical_content_bytes
            .checked_add(entry.canonical_content_bytes)
            .and_then(|bytes| {
                bytes.checked_add(usize::from(has_existing) * separator_content_bytes)
            })
            .context("external CLI canonical snapshot accounting overflowed")?;

        let mut evict = 0;
        // Metering and the eviction plan finish before the infallible queue commit.
        while evict < self.events.len()
            && (projected_raw_bytes > max_raw_bytes
                || canonical_external_state_item_bytes_from_parts(
                    projected_raw_bytes,
                    projected_canonical_content_bytes,
                ) > max_canonical_item_bytes)
        {
            let removed = &self.events[evict];
            projected_raw_bytes -= removed.rendered.len() + EXTERNAL_CLI_STATE_SEPARATOR.len();
            projected_canonical_content_bytes -=
                removed.canonical_content_bytes + separator_content_bytes;
            evict += 1;
        }
        anyhow::ensure!(
            projected_raw_bytes <= max_raw_bytes
                && canonical_external_state_item_bytes_from_parts(
                    projected_raw_bytes,
                    projected_canonical_content_bytes,
                ) <= max_canonical_item_bytes,
            "external CLI state cannot fit its bounded snapshot budget"
        );

        for _ in 0..evict {
            self.pop_front();
        }
        if !self.events.is_empty() {
            self.rendered_bytes += EXTERNAL_CLI_STATE_SEPARATOR.len();
            self.canonical_content_bytes += separator_content_bytes;
        }
        self.rendered_bytes += entry.rendered.len();
        self.canonical_content_bytes += entry.canonical_content_bytes;
        Arc::make_mut(&mut self.events).push_back(entry);

        debug_assert_eq!(self.rendered_bytes, projected_raw_bytes);
        debug_assert_eq!(
            self.canonical_content_bytes,
            projected_canonical_content_bytes
        );
        Ok(())
    }

    fn meter_event(&self, rendered: String) -> anyhow::Result<ExternalCliEvent> {
        self.record_entry_meter(rendered.len());
        self.record_content_serialization();
        Ok(ExternalCliEvent {
            canonical_content_bytes: canonical_external_state_content_bytes(&rendered)?,
            rendered: ExternalCliEventContent::Raw(rendered),
        })
    }

    fn meter_prefixed_event(
        &self,
        kind: ExternalCliTextKind,
        text: Arc<String>,
    ) -> anyhow::Result<ExternalCliEvent> {
        self.record_entry_meter(kind.prefix().len() + text.len());
        // Exact equality reuses an already validated serializer contribution;
        // distinct text still passes through the structured JSON/XML meter.
        let text_content_bytes = match self.events.back() {
            Some(ExternalCliEvent {
                rendered:
                    ExternalCliEventContent::Prefixed {
                        kind: previous_kind,
                        text: previous_text,
                    },
                canonical_content_bytes,
            }) if previous_text == &text => canonical_content_bytes
                .checked_sub(external_cli_text_prefix_content_bytes(*previous_kind))
                .context("external CLI cached event accounting underflowed")?,
            _ => {
                self.record_content_serialization();
                canonical_external_state_content_bytes(&text)?
            }
        };
        let canonical_content_bytes = external_cli_text_prefix_content_bytes(kind)
            .checked_add(text_content_bytes)
            .context("external CLI event canonical accounting overflowed")?;
        Ok(ExternalCliEvent {
            rendered: ExternalCliEventContent::Prefixed { kind, text },
            canonical_content_bytes,
        })
    }

    fn record_entry_meter(&self, _rendered_bytes: usize) {
        #[cfg(test)]
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics
                .entry_canonicalizations
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            diagnostics
                .entry_canonical_input_bytes
                .fetch_add(_rendered_bytes, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn record_content_serialization(&self) {
        #[cfg(test)]
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics
                .content_serializations
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn fit_event_suffix(
        &self,
        rendered: String,
        max_canonical_item_bytes: usize,
    ) -> anyhow::Result<ExternalCliEvent> {
        const SUFFIX_METER_CHUNK_BYTES: usize = 64 * 1024;

        // Scan each retained chunk once; only the single boundary chunk uses
        // binary search, so an oversized event still has a linear byte term.
        let mut fitting_start = rendered.len();
        let mut retained_content_bytes = 0usize;
        while fitting_start > 0 {
            let mut chunk_start = fitting_start.saturating_sub(SUFFIX_METER_CHUNK_BYTES);
            while !rendered.is_char_boundary(chunk_start) {
                chunk_start += 1;
            }
            let chunk = self.meter_event(rendered[chunk_start..fitting_start].to_owned())?;
            let projected_content_bytes = retained_content_bytes
                .checked_add(chunk.canonical_content_bytes)
                .context("external CLI suffix accounting overflowed")?;
            if canonical_external_state_item_bytes_from_parts(
                rendered.len() - chunk_start,
                projected_content_bytes,
            ) <= max_canonical_item_bytes
            {
                fitting_start = chunk_start;
                retained_content_bytes = projected_content_bytes;
                continue;
            }

            let chunk_end = fitting_start;
            let mut failing_within_chunk = chunk_start;
            let mut fitting_within_chunk = chunk_end;
            while let Some(middle) =
                char_boundary_between(&rendered, failing_within_chunk, fitting_within_chunk)
            {
                let candidate = self.meter_event(rendered[middle..chunk_end].to_owned())?;
                let candidate_content_bytes = retained_content_bytes
                    .checked_add(candidate.canonical_content_bytes)
                    .context("external CLI suffix accounting overflowed")?;
                if canonical_external_state_item_bytes_from_parts(
                    rendered.len() - middle,
                    candidate_content_bytes,
                ) <= max_canonical_item_bytes
                {
                    fitting_within_chunk = middle;
                } else {
                    failing_within_chunk = middle;
                }
            }
            fitting_start = fitting_within_chunk;
            break;
        }

        let fitted = self.meter_event(rendered[fitting_start..].to_owned())?;
        anyhow::ensure!(
            canonical_external_state_item_bytes_from_parts(
                fitted.rendered.len(),
                fitted.canonical_content_bytes,
            ) <= max_canonical_item_bytes,
            "external CLI event suffix cannot fit its canonical snapshot budget"
        );
        Ok(fitted)
    }

    fn pop_front(&mut self) {
        let removed = Arc::make_mut(&mut self.events)
            .pop_front()
            .expect("non-empty event queue");
        self.rendered_bytes -= removed.rendered.len();
        self.canonical_content_bytes -= removed.canonical_content_bytes;
        if !self.events.is_empty() {
            self.rendered_bytes -= EXTERNAL_CLI_STATE_SEPARATOR.len();
            self.canonical_content_bytes -= external_state_separator_content_bytes();
        }
    }

    #[cfg(test)]
    fn canonical_item_bytes(&self) -> usize {
        canonical_external_state_item_bytes_from_parts(
            self.rendered_bytes,
            self.canonical_content_bytes,
        )
    }

    #[cfg(test)]
    fn record_visibility_signal_update(&self) {
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics
                .visibility_signal_updates
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[derive(Serialize)]
struct FullReserveEnvelope<'a> {
    version: u8,
    replay: &'a [CanonicalInputItem],
    staged_inputs: &'a [CanonicalInputItem],
    component: Option<()>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExternalCliComponentState {
    snapshot: ExternalCliState,
    render_snapshot: bool,
}

impl ExternalCliComponentState {
    fn visible(snapshot: ExternalCliState) -> Self {
        Self {
            snapshot,
            render_snapshot: true,
        }
    }

    fn rendered(&self) -> String {
        if self.render_snapshot {
            self.snapshot.rendered()
        } else {
            String::new()
        }
    }
}

fn canonical_external_state_item(rendered: &str) -> anyhow::Result<CanonicalInputItem> {
    ensure_xml_renderable_text(rendered)?;
    let node = XmlNode::try_build("external_state", |children| {
        if !rendered.is_empty() {
            children.text(TextNode::new(rendered));
        }
        Ok(())
    })?;
    let pom = resolve_artifact_document(Document::from_xml(node))?;
    Ok(CanonicalInputItem::message(ConversationRole::User, pom))
}

fn canonical_external_state_item_bytes(rendered: &str) -> anyhow::Result<usize> {
    Ok(serde_json_canonicalizer::to_vec(&canonical_external_state_item(rendered)?)?.len())
}

fn external_state_empty_item_bytes() -> usize {
    static EMPTY_ITEM_BYTES: OnceLock<usize> = OnceLock::new();
    *EMPTY_ITEM_BYTES.get_or_init(|| {
        canonical_external_state_item_bytes("").expect("fixed empty CLI state must canonicalize")
    })
}

#[derive(Default)]
struct CanonicalJsonStringCounter {
    bytes: usize,
    first: Option<u8>,
    last: Option<u8>,
}

impl std::io::Write for CanonicalJsonStringCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if self.first.is_none() {
            self.first = buffer.first().copied();
        }
        if let Some(last) = buffer.last() {
            self.last = Some(*last);
        }
        self.bytes = self.bytes.checked_add(buffer.len()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "canonical CLI state text length overflowed",
            )
        })?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn canonical_external_state_content_bytes(rendered: &str) -> anyhow::Result<usize> {
    ensure_xml_renderable_text(rendered)?;
    let mut encoded = CanonicalJsonStringCounter::default();
    {
        let mut buffered =
            std::io::BufWriter::with_capacity(DAEMON_JSON_BUFFER_BYTES, &mut encoded);
        serde_json::to_writer(&mut buffered, &rendered)?;
        buffered.flush()?;
    }
    anyhow::ensure!(
        encoded.first == Some(b'"') && encoded.last == Some(b'"'),
        "canonical CLI state text was not encoded as a JSON string"
    );
    encoded
        .bytes
        .checked_sub(2)
        .context("canonical CLI state text was missing JSON string delimiters")
}

fn external_cli_text_prefix_content_bytes(kind: ExternalCliTextKind) -> usize {
    static DELTA_PREFIX_BYTES: OnceLock<usize> = OnceLock::new();
    static COMPLETE_PREFIX_BYTES: OnceLock<usize> = OnceLock::new();
    let (bytes, prefix) = match kind {
        ExternalCliTextKind::Delta => (&DELTA_PREFIX_BYTES, kind.prefix()),
        ExternalCliTextKind::Complete => (&COMPLETE_PREFIX_BYTES, kind.prefix()),
    };
    *bytes.get_or_init(|| {
        canonical_external_state_content_bytes(prefix)
            .expect("fixed external CLI event prefix must canonicalize")
    })
}

fn external_state_nonempty_envelope_bytes() -> usize {
    static NONEMPTY_ENVELOPE_BYTES: OnceLock<usize> = OnceLock::new();
    *NONEMPTY_ENVELOPE_BYTES.get_or_init(|| {
        // Derive F(text) = fixed nonempty envelope + encoded text from the
        // structured serializer itself; no XML or JSON escape table is copied.
        let one = canonical_external_state_item_bytes("x")
            .expect("fixed nonempty CLI state must canonicalize");
        let two = canonical_external_state_item_bytes("xx")
            .expect("fixed nonempty CLI state probe must canonicalize");
        let encoded_x = two
            .checked_sub(one)
            .expect("adding fixed CLI text cannot shrink its canonical item");
        one.checked_sub(encoded_x)
            .expect("fixed encoded CLI text fits its canonical item")
    })
}

fn canonical_external_state_item_bytes_from_parts(
    rendered_bytes: usize,
    canonical_content_bytes: usize,
) -> usize {
    if rendered_bytes == 0 {
        external_state_empty_item_bytes()
    } else {
        external_state_nonempty_envelope_bytes() + canonical_content_bytes
    }
}

fn external_state_separator_content_bytes() -> usize {
    static SEPARATOR_CONTENT_BYTES: OnceLock<usize> = OnceLock::new();
    *SEPARATOR_CONTENT_BYTES.get_or_init(|| {
        canonical_external_state_content_bytes(EXTERNAL_CLI_STATE_SEPARATOR)
            .expect("fixed CLI state separator must canonicalize")
    })
}

fn max_external_cli_snapshot_item_bytes() -> usize {
    static MAXIMUM: OnceLock<usize> = OnceLock::new();
    *MAXIMUM.get_or_init(|| {
        let empty_items: &[CanonicalInputItem] = &[];
        let envelope = FullReserveEnvelope {
            version: 1,
            replay: empty_items,
            staged_inputs: empty_items,
            component: None,
        };
        let non_component_bytes = serde_json_canonicalizer::to_vec(&envelope)
            .expect("fixed Full envelope must canonicalize")
            .len()
            .checked_sub(b"null".len())
            .expect("Full envelope contains its null Component");
        // JSON string encoding uses at most six bytes per decoded UTF-8 byte,
        // and all accepted frame content must also fit the complete wire cap.
        let max_canonical_text_bytes = MAX_PROTOCOL_WIRE_BYTES.min(
            EXTERNAL_CLI_MAX_TEXT_BYTES
                .checked_mul(6)
                .expect("External text bound fits usize"),
        );
        let interrupted_item_bytes = serde_json_canonicalizer::to_vec(
            &CanonicalInputItem::interrupted_assistant_text(String::new(), None),
        )
        .expect("fixed assistant item must canonicalize")
        .len()
        .checked_add(max_canonical_text_bytes)
        .expect("External protocol bound fits usize");
        let hidden_state_bytes = canonical_external_state_item_bytes("")
            .expect("fixed hidden CLI state must canonicalize");

        // One rotated Application retains exactly: visible snapshot, one
        // worst-case interrupted act, and the hidden post-act snapshot.
        EXTERNAL_CLI_MAX_FRAME_BYTES
            .checked_sub(EXTERNAL_CLI_MAX_COMPONENT_BYTES)
            .and_then(|bytes| bytes.checked_sub(non_component_bytes))
            .and_then(|bytes| bytes.checked_sub(interrupted_item_bytes))
            .and_then(|bytes| bytes.checked_sub(hidden_state_bytes))
            .and_then(|bytes| bytes.checked_sub(2))
            .expect("External profile has room for one bounded CLI act")
    })
}

fn bounded_suffix(value: String, limit: usize) -> String {
    if value.len() <= limit {
        return value;
    }
    let mut start = value.len() - limit;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value[start..].to_owned()
}

fn char_boundary_between(value: &str, failing: usize, fitting: usize) -> Option<usize> {
    debug_assert!(failing < fitting);
    debug_assert!(value.is_char_boundary(failing));
    debug_assert!(value.is_char_boundary(fitting));

    let mut middle = failing + (fitting - failing) / 2;
    while middle > failing && !value.is_char_boundary(middle) {
        middle -= 1;
    }
    if middle == failing {
        middle += 1;
        while middle < fitting && !value.is_char_boundary(middle) {
            middle += 1;
        }
    }
    (middle < fitting).then_some(middle)
}

fn ensure_xml_renderable_text(value: &str) -> anyhow::Result<()> {
    if let Some(invalid) = value.chars().find(|character| {
        let code_point = u32::from(*character);
        !matches!(code_point, 0x9 | 0xA | 0xD)
            && !(0x20..=0xD7FF).contains(&code_point)
            && !(0xE000..=0xFFFD).contains(&code_point)
            && !(0x10000..=0x10FFFF).contains(&code_point)
    }) {
        anyhow::bail!(
            "external text character U+{:04X} is not allowed in XML content",
            u32::from(invalid)
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct ExternalProtocolFrameHeader {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ExternalCliProtocolFrame {
    TextDelta { text: String },
    TextComplete { text: String },
    Disconnect,
}

struct ExternalCliPreparedEvent {
    index: usize,
    kind: ExternalCliTextKind,
    text: Arc<String>,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExternalCliPreparedOutcome {
    Open = 0,
    Succeeded = 1,
    Failed = 2,
}

struct ExternalCliPreparedProgress {
    events: Vec<OnceLock<Arc<ExternalCliPreparedEvent>>>,
    published: AtomicUsize,
    accepted: AtomicUsize,
    outcome: AtomicU8,
    publication: Mutex<()>,
    changed: Notify,
}

impl ExternalCliPreparedProgress {
    fn new() -> anyhow::Result<Self> {
        let event_limit = MAX_PROTOCOL_FRAMES
            .checked_add(1)
            .context("external CLI prepared verifier event limit overflowed")?;
        let mut events = Vec::new();
        events
            .try_reserve_exact(event_limit)
            .context("external CLI prepared verifier could not reserve bounded storage")?;
        events.resize_with(event_limit, OnceLock::new);
        Ok(Self {
            events,
            published: AtomicUsize::new(0),
            accepted: AtomicUsize::new(0),
            outcome: AtomicU8::new(ExternalCliPreparedOutcome::Open as u8),
            publication: Mutex::new(()),
            changed: Notify::new(),
        })
    }

    fn publish(
        &self,
        kind: ExternalCliTextKind,
        text: Arc<String>,
    ) -> anyhow::Result<Arc<ExternalCliPreparedEvent>> {
        let _publication = self
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(
            self.outcome.load(Ordering::Acquire) == ExternalCliPreparedOutcome::Open as u8,
            "external CLI prepared verifier was already closed"
        );
        let index = self.published.load(Ordering::Relaxed);
        let slot = self
            .events
            .get(index)
            .context("external CLI prepared verifier exceeded configured event limit")?;
        let event = Arc::new(ExternalCliPreparedEvent { index, kind, text });
        slot.set(Arc::clone(&event))
            .map_err(|_| anyhow::anyhow!("external CLI prepared event was published twice"))?;
        self.published.store(index + 1, Ordering::Release);
        drop(_publication);
        self.changed.notify_one();
        Ok(event)
    }

    fn complete_success(&self, events: usize) -> anyhow::Result<()> {
        let _publication = self
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(
            self.outcome.load(Ordering::Acquire) == ExternalCliPreparedOutcome::Open as u8,
            "external CLI prepared verifier was already closed"
        );
        anyhow::ensure!(
            self.published.load(Ordering::Acquire) == events,
            "external CLI prepared verifier event count diverged"
        );
        self.outcome.store(
            ExternalCliPreparedOutcome::Succeeded as u8,
            Ordering::Release,
        );
        drop(_publication);
        self.changed.notify_one();
        Ok(())
    }

    fn close_failed(&self) {
        let _publication = self
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = self
            .outcome
            .compare_exchange(
                ExternalCliPreparedOutcome::Open as u8,
                ExternalCliPreparedOutcome::Failed as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        drop(_publication);
        if changed {
            self.changed.notify_one();
        }
    }

    fn verify_and_accept_ready(
        &self,
        kind: ExternalCliTextKind,
        text: &str,
    ) -> anyhow::Result<bool> {
        let accepted = self.accepted.load(Ordering::Relaxed);
        if accepted < self.published.load(Ordering::Acquire) {
            let prepared = self
                .events
                .get(accepted)
                .and_then(OnceLock::get)
                .context("external CLI prepared event publication was incomplete")?;
            anyhow::ensure!(
                prepared.index == accepted
                    && prepared.kind == kind
                    && prepared.text.as_str() == text,
                "external CLI prepared event did not match the admitted event"
            );
            self.accepted
                .compare_exchange(accepted, accepted + 1, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| {
                    anyhow::anyhow!("external CLI prepared verifier accepted out of order")
                })?;
            return Ok(true);
        }
        anyhow::ensure!(
            self.outcome.load(Ordering::Acquire) == ExternalCliPreparedOutcome::Open as u8,
            "external CLI prepared verifier closed before the admitted event"
        );
        Ok(false)
    }

    async fn wait_and_accept(&self, kind: ExternalCliTextKind, text: &str) -> anyhow::Result<()> {
        loop {
            let notified = self.changed.notified();
            if self.verify_and_accept_ready(kind, text)? {
                return Ok(());
            }
            notified.await;
        }
    }

    fn is_exact_success(&self, events: usize) -> bool {
        self.outcome.load(Ordering::Acquire) == ExternalCliPreparedOutcome::Succeeded as u8
            && self.published.load(Ordering::Acquire) == events
            && self.accepted.load(Ordering::Acquire) == events
    }

    fn replay_accepted_prefix(
        &self,
        mut baseline: ExternalCliState,
    ) -> anyhow::Result<ExternalCliState> {
        let accepted = self.accepted.load(Ordering::Acquire);
        anyhow::ensure!(
            accepted <= self.published.load(Ordering::Acquire),
            "external CLI prepared verifier accepted count diverged"
        );
        for index in 0..accepted {
            let event = self
                .events
                .get(index)
                .and_then(OnceLock::get)
                .context("external CLI accepted prefix publication was incomplete")?;
            baseline.push_external_text_shared(event.kind, Arc::clone(&event.text))?;
        }
        Ok(baseline)
    }
}

struct ExternalCliPreparedWorkerGuard {
    progress: Arc<ExternalCliPreparedProgress>,
    completed: bool,
}

impl ExternalCliPreparedWorkerGuard {
    fn new(progress: Arc<ExternalCliPreparedProgress>) -> Self {
        Self {
            progress,
            completed: false,
        }
    }

    fn complete(&mut self, events: usize) -> anyhow::Result<()> {
        self.progress.complete_success(events)?;
        self.completed = true;
        Ok(())
    }
}

impl Drop for ExternalCliPreparedWorkerGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.progress.close_failed();
        }
    }
}

struct ExternalCliSpeculativeCandidate {
    snapshot: ExternalCliState,
    events: usize,
}

struct ExternalCliSpeculationCancellation {
    cancelled: Arc<AtomicBool>,
    abort: Option<tokio::task::AbortHandle>,
    progress: Option<Arc<ExternalCliPreparedProgress>>,
    #[cfg(test)]
    barrier: Option<Arc<ExternalCliSpeculationBarrier>>,
}

#[cfg(test)]
#[derive(Default)]
struct ExternalCliSpeculationBarrierState {
    entered: bool,
    released: bool,
    cancelled: bool,
    finished: bool,
}

#[cfg(test)]
#[derive(Default)]
struct ExternalCliSpeculationBarrier {
    state: Mutex<ExternalCliSpeculationBarrierState>,
    changed: std::sync::Condvar,
}

#[cfg(test)]
impl ExternalCliSpeculationBarrier {
    fn enter_and_wait(&self) {
        let mut state = self.state.lock().unwrap();
        state.entered = true;
        self.changed.notify_all();
        while !state.released && !state.cancelled {
            state = self.changed.wait(state).unwrap();
        }
    }

    fn wait_until_entered(&self) {
        let mut state = self.state.lock().unwrap();
        while !state.entered {
            state = self.changed.wait(state).unwrap();
        }
    }

    fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        state.cancelled = true;
        self.changed.notify_all();
    }

    fn finish(&self) {
        let mut state = self.state.lock().unwrap();
        state.finished = true;
        self.changed.notify_all();
    }

    fn snapshot(&self) -> ExternalCliSpeculationBarrierState {
        let state = self.state.lock().unwrap();
        ExternalCliSpeculationBarrierState {
            entered: state.entered,
            released: state.released,
            cancelled: state.cancelled,
            finished: state.finished,
        }
    }
}

impl ExternalCliSpeculationCancellation {
    fn new(
        cancelled: Arc<AtomicBool>,
        abort: tokio::task::AbortHandle,
        progress: Option<Arc<ExternalCliPreparedProgress>>,
        #[cfg(test)] barrier: Option<Arc<ExternalCliSpeculationBarrier>>,
    ) -> Self {
        Self {
            cancelled,
            abort: Some(abort),
            progress,
            #[cfg(test)]
            barrier,
        }
    }

    fn disarm(&mut self) {
        self.abort = None;
        self.progress = None;
    }

    fn cancel(&mut self) {
        if let Some(abort) = self.abort.take() {
            self.cancelled.store(true, Ordering::Release);
            if let Some(progress) = self.progress.take() {
                progress.close_failed();
            }
            #[cfg(test)]
            if let Some(barrier) = &self.barrier {
                barrier.cancel();
            }
            abort.abort();
        }
    }
}

impl Drop for ExternalCliSpeculationCancellation {
    fn drop(&mut self) {
        self.cancel();
    }
}

async fn coordinate_external_cli_speculation<
    Operation,
    Speculation,
    OperationFuture,
    SpeculationFuture,
>(
    operation_future: OperationFuture,
    speculative_future: SpeculationFuture,
    cancellation: &mut ExternalCliSpeculationCancellation,
) -> Result<(Operation, Speculation), Box<dyn std::any::Any + Send + 'static>>
where
    OperationFuture:
        std::future::Future<Output = Result<Operation, Box<dyn std::any::Any + Send + 'static>>>,
    SpeculationFuture: std::future::Future<Output = Speculation>,
{
    let mut operation_future = Box::pin(operation_future);
    let mut speculative_future = Box::pin(speculative_future);
    let mut operation_result = None;
    let mut speculative_result = None;
    tokio::select! {
        biased;
        operation = &mut operation_future => operation_result = Some(operation),
        speculative = &mut speculative_future => speculative_result = Some(speculative),
    }
    let completed = if let Some(operation) = operation_result {
        drop(operation_future);
        let operation = match operation {
            Ok(operation) => operation,
            Err(payload) => {
                cancellation.cancel();
                drop(speculative_future);
                return Err(payload);
            }
        };
        (operation, speculative_future.await)
    } else {
        let speculative = speculative_result.expect("speculative future completed first");
        drop(speculative_future);
        let operation = match operation_future.await {
            Ok(operation) => operation,
            Err(payload) => {
                cancellation.cancel();
                return Err(payload);
            }
        };
        (operation, speculative)
    };
    cancellation.disarm();
    Ok(completed)
}

fn prepare_external_cli_candidate(
    protocol: &str,
    mut snapshot: ExternalCliState,
    progress: Arc<ExternalCliPreparedProgress>,
    cancelled: &AtomicBool,
) -> anyhow::Result<Option<ExternalCliSpeculativeCandidate>> {
    let mut worker = ExternalCliPreparedWorkerGuard::new(Arc::clone(&progress));
    let mut accumulated = String::new();
    let mut frames = 0_usize;
    let mut events = 0_usize;
    let mut explicitly_completed = false;

    for line in protocol.lines() {
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        frames = frames
            .checked_add(1)
            .context("external CLI speculative frame count overflowed")?;
        if frames > MAX_PROTOCOL_FRAMES {
            return Ok(None);
        }
        match serde_json::from_str::<ExternalCliProtocolFrame>(line) {
            Ok(ExternalCliProtocolFrame::TextDelta { text }) => {
                if accumulated
                    .len()
                    .checked_add(text.len())
                    .is_none_or(|bytes| bytes > EXTERNAL_CLI_MAX_TEXT_BYTES)
                {
                    return Ok(None);
                }
                let text = Arc::new(text);
                accumulated.push_str(&text);
                if snapshot
                    .push_external_text_shared(ExternalCliTextKind::Delta, Arc::clone(&text))
                    .is_err()
                {
                    return Ok(None);
                }
                progress.publish(ExternalCliTextKind::Delta, text)?;
                events += 1;
            }
            Ok(ExternalCliProtocolFrame::TextComplete { text }) => {
                if text.len() > EXTERNAL_CLI_MAX_TEXT_BYTES {
                    return Ok(None);
                }
                let text = Arc::new(text);
                if snapshot
                    .push_external_text_shared(ExternalCliTextKind::Complete, Arc::clone(&text))
                    .is_err()
                {
                    return Ok(None);
                }
                progress.publish(ExternalCliTextKind::Complete, text)?;
                events += 1;
                explicitly_completed = true;
                break;
            }
            Ok(ExternalCliProtocolFrame::Disconnect) | Err(_) => return Ok(None),
        }
    }
    if cancelled.load(Ordering::Acquire) {
        return Ok(None);
    }
    if !explicitly_completed {
        let accumulated = Arc::new(accumulated);
        if snapshot
            .push_external_text_shared(ExternalCliTextKind::Complete, Arc::clone(&accumulated))
            .is_err()
        {
            return Ok(None);
        }
        progress.publish(ExternalCliTextKind::Complete, accumulated)?;
        events += 1;
    }
    worker.complete(events)?;
    Ok(Some(ExternalCliSpeculativeCandidate { snapshot, events }))
}

#[component]
fn external_cli_application(props: ExternalCliProps) -> Component {
    let initial_state = props
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let state = use_signal(move || ExternalCliComponentState::visible(initial_state));
    *props
        .exported_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state.clone());
    let rendered = state
        .with(ExternalCliComponentState::rendered)
        .expect("mounted external CLI Signal");
    let event_state = state.clone();
    let business_snapshot = Arc::clone(&props.snapshot);
    let snapshot_hidden = Arc::clone(&props.snapshot_hidden);
    let prepared_progress = Arc::clone(&props.prepared_progress);
    #[cfg(test)]
    let panic_on_text = props.panic_on_text;
    #[cfg(test)]
    let error_on_text = props.error_on_text;
    #[cfg(test)]
    let panic_after_speculation_started = props.panic_after_speculation_started.clone();
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let state = event_state.clone();
        let business_snapshot = Arc::clone(&business_snapshot);
        let snapshot_hidden = Arc::clone(&snapshot_hidden);
        let prepared_progress = Arc::clone(&prepared_progress);
        #[cfg(test)]
        let panic_after_speculation_started = panic_after_speculation_started.clone();
        async move {
            #[cfg(test)]
            if let Some(payload) = panic_on_text {
                if let Some(barrier) = &panic_after_speculation_started {
                    barrier.wait_until_entered();
                }
                panic::panic_any(payload.to_owned());
            }
            let (kind, text) = match event {
                TextTurnEvent::TextDelta(text) => (ExternalCliTextKind::Delta, text),
                TextTurnEvent::TextComplete(text) => (ExternalCliTextKind::Complete, text),
            };
            let prepared = prepared_progress.get().cloned();
            if let Some(prepared) = prepared {
                if !prepared.verify_and_accept_ready(kind, &text)? {
                    prepared.wait_and_accept(kind, &text).await?;
                }
            } else {
                business_snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push_external_text(kind, text)?;
            }
            #[cfg(test)]
            if let Some(message) = error_on_text {
                anyhow::bail!(message);
            }
            if !snapshot_hidden.swap(true, Ordering::AcqRel) {
                #[cfg(test)]
                business_snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .record_visibility_signal_update();
                state.update(|state| state.render_snapshot = false)?;
            }
            Ok::<(), anyhow::Error>(())
        }
    });

    view! {
        #[system_once]
        protocol { "External CLI protocol. Return text through the advertised act stream." }
        external_state { "{rendered}" }
    }
}

type ExternalCliApplication = ExternalApplication;

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliCommand {
    Help,
    Observe { full_re_render: bool },
    ActText { text: String },
    ActProtocol,
    InternalDaemon,
    InternalShutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum DaemonRequest {
    Observe { full_re_render: bool },
    Act { protocol: String },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum DaemonAuthFrame {
    ClientHello {
        nonce: [u8; 32],
    },
    ServerHello {
        challenge: [u8; 32],
        proof: [u8; 32],
    },
    ClientProof {
        proof: [u8; 32],
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum AuthenticatedDaemonRequest {
    Observe { full_re_render: bool },
    Act { protocol_bytes: usize },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthenticatedDaemonEnvelope {
    authentication: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DaemonResponse {
    Observation {
        event: String,
        mode: String,
        generation: String,
        base_generation: Option<String>,
        content: String,
    },
    Ok,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum DaemonResponseEnvelope {
    Observation { response_bytes: usize },
    Ok { response_bytes: usize },
    Error { response_bytes: usize },
}

type DaemonResponseTicket = [u8; 32];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum DaemonProvisionalResponseEnvelope {
    Provisional {
        ticket: DaemonResponseTicket,
        response: DaemonResponseEnvelope,
    },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DaemonInitialResponseEnvelope {
    Provisional(DaemonProvisionalResponseEnvelope),
    Ordinary(DaemonResponseEnvelope),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "disposition", rename_all = "snake_case", deny_unknown_fields)]
enum DaemonResponseDisposition {
    Commit { ticket: DaemonResponseTicket },
    Replace { ticket: DaemonResponseTicket },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum DaemonResponseValidation {
    Observation {
        event: String,
        mode: String,
        generation: String,
        base_generation: Option<String>,
        #[serde(rename = "content")]
        _content: serde::de::IgnoredAny,
    },
    Ok,
    Error {
        message: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum ValidatedDaemonResponseKind {
    Observation { mode: String },
    Ok,
    Error,
}

#[derive(Debug)]
struct ClientDaemonResponse {
    envelope: DaemonResponseEnvelope,
    wire: String,
    error_message: Option<String>,
    validated_kind: ValidatedDaemonResponseKind,
}

struct EncodedDaemonRequest<'a> {
    header: Vec<u8>,
    payload: &'a [u8],
}

struct EncodedDaemonResponse {
    envelope: DaemonResponseEnvelope,
    line: Vec<u8>,
}

struct PreparedDaemonResponse {
    response: DaemonResponse,
    encoded: EncodedDaemonResponse,
}

struct BoundedJsonBuffer {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

struct HmacJsonWriter<'a> {
    mac: &'a mut HmacSha256,
}

enum SendOnceFault {
    Unavailable(anyhow::Error),
    NoRetry(anyhow::Error),
}

impl SendOnceFault {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Unavailable(fault) | Self::NoRetry(fault) => fault,
        }
    }
}

impl BoundedJsonBuffer {
    fn new(capacity: usize, limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity.min(limit)),
            limit,
            exceeded: false,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl std::io::Write for BoundedJsonBuffer {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let required = self.bytes.len().checked_add(buffer.len()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "external daemon response length overflowed",
            )
        })?;
        if required > self.limit {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "external daemon response exceeded configured wire limit",
            ));
        }
        if required > self.bytes.capacity() {
            self.bytes
                .try_reserve_exact(required - self.bytes.len())
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::OutOfMemory,
                        "external daemon response allocation failed",
                    )
                })?;
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Write for HmacJsonWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.mac.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode_daemon_request(request: &DaemonRequest) -> anyhow::Result<EncodedDaemonRequest<'_>> {
    let (header, payload) = match request {
        DaemonRequest::Observe { full_re_render } => (
            AuthenticatedDaemonRequest::Observe {
                full_re_render: *full_re_render,
            },
            &[][..],
        ),
        DaemonRequest::Act { protocol } => (
            AuthenticatedDaemonRequest::Act {
                protocol_bytes: protocol.len(),
            },
            protocol.as_bytes(),
        ),
        DaemonRequest::Shutdown => (AuthenticatedDaemonRequest::Shutdown, &[][..]),
    };
    anyhow::ensure!(
        payload.len() <= MAX_PROTOCOL_WIRE_BYTES,
        "external daemon request exceeded configured wire limit"
    );
    let header = serde_json::to_vec(&header)?;
    anyhow::ensure!(
        header.len() < MAX_DAEMON_AUTH_LINE_BYTES,
        "external daemon request header exceeded configured wire limit"
    );
    Ok(EncodedDaemonRequest { header, payload })
}

fn encode_daemon_response(
    response: &DaemonResponse,
) -> anyhow::Result<(DaemonResponseEnvelope, Vec<u8>)> {
    let capacity = match response {
        DaemonResponse::Observation { content, .. } => {
            content.len().saturating_mul(2).saturating_add(2048)
        }
        DaemonResponse::Error { message } => message.len().saturating_mul(2).saturating_add(256),
        DaemonResponse::Ok => 32,
    };
    let mut line = BoundedJsonBuffer::new(capacity, MAX_DAEMON_RESPONSE_JSON_BYTES);
    let encoded = {
        let mut buffered = std::io::BufWriter::with_capacity(DAEMON_JSON_BUFFER_BYTES, &mut line);
        serde_json::to_writer(&mut buffered, response)
            .map_err(anyhow::Error::from)
            .and_then(|()| buffered.flush().map_err(anyhow::Error::from))
    };
    if let Err(fault) = encoded {
        if line.exceeded {
            anyhow::bail!("external daemon response exceeded configured wire limit");
        }
        return Err(fault);
    }
    let line = line.into_inner();
    let envelope = match response {
        DaemonResponse::Observation { .. } => DaemonResponseEnvelope::Observation {
            response_bytes: line.len(),
        },
        DaemonResponse::Ok => DaemonResponseEnvelope::Ok {
            response_bytes: line.len(),
        },
        DaemonResponse::Error { .. } => DaemonResponseEnvelope::Error {
            response_bytes: line.len(),
        },
    };
    Ok((envelope, line))
}

fn validate_observation_response_prefix(
    wire: &str,
    event: String,
    mode: String,
    generation: String,
    base_generation: Option<String>,
) -> anyhow::Result<()> {
    let expected = serde_json::to_vec(&DaemonResponse::Observation {
        event,
        mode,
        generation,
        base_generation,
        content: String::new(),
    })
    .context("external daemon response was invalid")?;
    let expected_prefix = expected
        .strip_suffix(b"\"\"}")
        .context("external daemon response was invalid")?;
    let wire = wire.as_bytes();
    anyhow::ensure!(
        wire.starts_with(expected_prefix)
            && wire.get(expected_prefix.len()) == Some(&b'\"')
            && wire.ends_with(b"\"}"),
        "external daemon response was invalid"
    );
    Ok(())
}

fn validate_daemon_response_wire(
    envelope: &DaemonResponseEnvelope,
    wire: &str,
) -> anyhow::Result<(ValidatedDaemonResponseKind, Option<String>)> {
    let validation = serde_json::from_str::<DaemonResponseValidation>(wire)
        .context("external daemon response was invalid")?;
    match (envelope, validation) {
        (
            DaemonResponseEnvelope::Observation { .. },
            DaemonResponseValidation::Observation {
                event,
                mode,
                generation,
                base_generation,
                _content: _,
            },
        ) => {
            let validated_mode = mode.clone();
            validate_observation_response_prefix(wire, event, mode, generation, base_generation)?;
            Ok((
                ValidatedDaemonResponseKind::Observation {
                    mode: validated_mode,
                },
                None,
            ))
        }
        (DaemonResponseEnvelope::Ok { .. }, DaemonResponseValidation::Ok) => {
            Ok((ValidatedDaemonResponseKind::Ok, None))
        }
        (DaemonResponseEnvelope::Error { .. }, DaemonResponseValidation::Error { message }) => {
            Ok((ValidatedDaemonResponseKind::Error, Some(message)))
        }
        _ => anyhow::bail!("external daemon response did not match its envelope"),
    }
}

fn validate_client_daemon_response(
    envelope: DaemonResponseEnvelope,
    wire: String,
) -> anyhow::Result<ClientDaemonResponse> {
    let expected_bytes = daemon_response_bytes(&envelope);
    anyhow::ensure!(
        wire.len() == expected_bytes,
        "external daemon response length did not match its envelope"
    );
    let (validated_kind, error_message) = validate_daemon_response_wire(&envelope, &wire)?;
    Ok(ClientDaemonResponse {
        envelope,
        wire,
        error_message,
        validated_kind,
    })
}

fn daemon_response_bytes(envelope: &DaemonResponseEnvelope) -> usize {
    match envelope {
        DaemonResponseEnvelope::Observation { response_bytes }
        | DaemonResponseEnvelope::Ok { response_bytes }
        | DaemonResponseEnvelope::Error { response_bytes } => *response_bytes,
    }
}

async fn read_client_response_body<R>(
    reader: &mut R,
    envelope: DaemonResponseEnvelope,
) -> anyhow::Result<ClientDaemonResponse>
where
    R: AsyncBufRead + Unpin,
{
    let framed_bytes = daemon_response_bytes(&envelope)
        .checked_add(1)
        .context("external daemon response length overflowed")?;
    anyhow::ensure!(
        framed_bytes <= MAX_DAEMON_LINE_BYTES,
        "external daemon response exceeded configured wire limit"
    );
    let mut response = vec![0_u8; framed_bytes];
    reader
        .read_exact(&mut response)
        .await
        .context("failed to read external daemon response")?;
    anyhow::ensure!(
        response.pop() == Some(b'\n'),
        "external daemon response was not terminated"
    );
    let response = String::from_utf8(response).context("external daemon response was not UTF-8")?;
    validate_client_daemon_response(envelope, response)
}

async fn read_strict_daemon_line<R>(reader: &mut R, label: &'static str) -> anyhow::Result<String>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = read_bounded_line(reader, MAX_DAEMON_AUTH_LINE_BYTES, label).await?;
    anyhow::ensure!(
        line.pop() == Some('\n'),
        "external daemon {label} was not terminated"
    );
    Ok(line)
}

async fn read_client_daemon_response<R>(
    reader: &mut R,
    request: &DaemonRequest,
) -> anyhow::Result<ClientDaemonResponse>
where
    R: AsyncBufRead + Unpin,
{
    let initial = read_strict_daemon_line(reader, "response envelope").await?;
    let initial = serde_json::from_str::<DaemonInitialResponseEnvelope>(&initial)
        .context("external daemon response envelope was invalid")?;
    match initial {
        DaemonInitialResponseEnvelope::Ordinary(envelope) => {
            read_client_response_body(reader, envelope).await
        }
        DaemonInitialResponseEnvelope::Provisional(
            DaemonProvisionalResponseEnvelope::Provisional { ticket, response },
        ) => {
            anyhow::ensure!(
                matches!(request, DaemonRequest::Act { .. }),
                "external daemon provisional response was not valid for this request"
            );
            anyhow::ensure!(
                matches!(&response, DaemonResponseEnvelope::Observation { .. }),
                "external daemon provisional response was not an Observation"
            );
            let provisional = read_client_response_body(reader, response).await?;
            anyhow::ensure!(
                matches!(
                    &provisional.validated_kind,
                    ValidatedDaemonResponseKind::Observation { mode } if mode == "full"
                ),
                "external daemon provisional response was not a Full observation"
            );
            let disposition = read_strict_daemon_line(reader, "response disposition").await?;
            let disposition = serde_json::from_str::<DaemonResponseDisposition>(&disposition)
                .context("external daemon response disposition was invalid")?;
            match disposition {
                DaemonResponseDisposition::Commit {
                    ticket: disposition_ticket,
                } if disposition_ticket == ticket => Ok(provisional),
                DaemonResponseDisposition::Replace {
                    ticket: disposition_ticket,
                } if disposition_ticket == ticket => {
                    drop(provisional);
                    let replacement =
                        read_strict_daemon_line(reader, "replacement response envelope").await?;
                    let replacement = serde_json::from_str::<DaemonResponseEnvelope>(&replacement)
                        .context("external daemon replacement response envelope was invalid")?;
                    read_client_response_body(reader, replacement).await
                }
                DaemonResponseDisposition::Commit { .. }
                | DaemonResponseDisposition::Replace { .. } => {
                    anyhow::bail!("external daemon response disposition ticket did not match")
                }
            }
        }
    }
}

struct ExternalCliSession {
    application: ExternalCliApplication,
    business_snapshot: Option<Arc<Mutex<ExternalCliState>>>,
    component_state: Option<Signal<ExternalCliComponentState>>,
    prepared_progress: Option<Arc<OnceLock<Arc<ExternalCliPreparedProgress>>>>,
    pending_observation: Option<ExternalObservation>,
    #[cfg(test)]
    speculation_barrier: Option<Arc<ExternalCliSpeculationBarrier>>,
    #[cfg(test)]
    shutdown_fault: bool,
}

impl ExternalCliSession {
    fn mount(initial_state: ExternalCliState) -> anyhow::Result<Self> {
        let snapshot = Arc::new(Mutex::new(initial_state));
        let exported_state = Arc::new(Mutex::new(None));
        let prepared_progress = Arc::new(OnceLock::new());
        let props = ExternalCliProps {
            snapshot: Arc::clone(&snapshot),
            snapshot_hidden: Arc::new(AtomicBool::new(false)),
            exported_state: Arc::clone(&exported_state),
            prepared_progress,
            #[cfg(test)]
            panic_on_text: None,
            #[cfg(test)]
            error_on_text: None,
            #[cfg(test)]
            panic_after_speculation_started: None,
        };
        Self::mount_with_props(props, exported_state, snapshot)
    }

    #[cfg(test)]
    fn mount_panicking(
        initial_state: ExternalCliState,
        panic_on_text: &'static str,
    ) -> anyhow::Result<Self> {
        let snapshot = Arc::new(Mutex::new(initial_state));
        let exported_state = Arc::new(Mutex::new(None));
        let prepared_progress = Arc::new(OnceLock::new());
        let props = ExternalCliProps {
            snapshot: Arc::clone(&snapshot),
            snapshot_hidden: Arc::new(AtomicBool::new(false)),
            exported_state: Arc::clone(&exported_state),
            prepared_progress,
            panic_on_text: Some(panic_on_text),
            error_on_text: None,
            panic_after_speculation_started: None,
        };
        Self::mount_with_props(props, exported_state, snapshot)
    }

    #[cfg(test)]
    fn mount_panicking_after_speculation_started(
        initial_state: ExternalCliState,
        panic_on_text: &'static str,
        barrier: Arc<ExternalCliSpeculationBarrier>,
    ) -> anyhow::Result<Self> {
        let snapshot = Arc::new(Mutex::new(initial_state));
        let exported_state = Arc::new(Mutex::new(None));
        let prepared_progress = Arc::new(OnceLock::new());
        let props = ExternalCliProps {
            snapshot: Arc::clone(&snapshot),
            snapshot_hidden: Arc::new(AtomicBool::new(false)),
            exported_state: Arc::clone(&exported_state),
            prepared_progress,
            panic_on_text: Some(panic_on_text),
            error_on_text: None,
            panic_after_speculation_started: Some(barrier),
        };
        Self::mount_with_props(props, exported_state, snapshot)
    }

    #[cfg(test)]
    fn mount_erroring(
        initial_state: ExternalCliState,
        error_on_text: &'static str,
    ) -> anyhow::Result<Self> {
        let snapshot = Arc::new(Mutex::new(initial_state));
        let exported_state = Arc::new(Mutex::new(None));
        let prepared_progress = Arc::new(OnceLock::new());
        let props = ExternalCliProps {
            snapshot: Arc::clone(&snapshot),
            snapshot_hidden: Arc::new(AtomicBool::new(false)),
            exported_state: Arc::clone(&exported_state),
            prepared_progress,
            panic_on_text: None,
            error_on_text: Some(error_on_text),
            panic_after_speculation_started: None,
        };
        Self::mount_with_props(props, exported_state, snapshot)
    }

    #[cfg(test)]
    fn mount_failing_shutdown(initial_state: ExternalCliState) -> anyhow::Result<Self> {
        let mut session = Self::mount(initial_state)?;
        session.shutdown_fault = true;
        Ok(session)
    }

    fn mount_with_props(
        props: ExternalCliProps,
        exported_state: Arc<Mutex<Option<Signal<ExternalCliComponentState>>>>,
        business_snapshot: Arc<Mutex<ExternalCliState>>,
    ) -> anyhow::Result<Self> {
        let prepared_progress = Arc::clone(&props.prepared_progress);
        #[cfg(test)]
        let speculation_barrier = props.panic_after_speculation_started.clone();
        let application =
            ExternalApplication::new_root(move || external_cli_application(props.clone()))
                .context("external CLI application must mount")?;
        let component_state = exported_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .context("external CLI application did not export its state")?;
        Ok(Self {
            application,
            business_snapshot: Some(business_snapshot),
            component_state: Some(component_state),
            prepared_progress: Some(prepared_progress),
            pending_observation: None,
            #[cfg(test)]
            speculation_barrier,
            #[cfg(test)]
            shutdown_fault: false,
        })
    }

    #[cfg(test)]
    fn from_application(application: ExternalCliApplication) -> Self {
        Self {
            application,
            business_snapshot: None,
            component_state: None,
            prepared_progress: None,
            pending_observation: None,
            speculation_barrier: None,
            shutdown_fault: false,
        }
    }

    fn state_signal(&self) -> anyhow::Result<&Signal<ExternalCliComponentState>> {
        self.component_state
            .as_ref()
            .context("external CLI application state is unavailable")
    }

    fn snapshot(&self) -> anyhow::Result<ExternalCliState> {
        Ok(self
            .business_snapshot
            .as_ref()
            .context("external CLI application state is unavailable")?
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }

    fn install_prepared_progress(
        &self,
        progress: Arc<ExternalCliPreparedProgress>,
    ) -> anyhow::Result<()> {
        self.prepared_progress
            .as_ref()
            .context("external CLI prepared verifier is unavailable")?
            .set(progress)
            .map_err(|_| anyhow::anyhow!("external CLI prepared verifier was already installed"))
    }

    fn install_snapshot(&self, snapshot: ExternalCliState) -> anyhow::Result<()> {
        let visible_snapshot = snapshot.clone();
        *self
            .business_snapshot
            .as_ref()
            .context("external CLI replacement state is unavailable")?
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
        self.state_signal()?
            .set(ExternalCliComponentState::visible(visible_snapshot))
            .context("external CLI replacement state is unavailable")
    }

    async fn shutdown(self) -> Result<(), ExternalApplicationFault> {
        #[cfg(test)]
        let shutdown_fault = self.shutdown_fault;
        let result = self.application.shutdown().await;
        #[cfg(test)]
        if shutdown_fault {
            return Err(ExternalApplicationFault::ShutdownTaskFailed);
        }
        result
    }
}

struct DaemonState {
    external: Option<ExternalCliSession>,
}

struct AuthenticatedDaemonConnection {
    stream: TcpStream,
    request: DaemonRequest,
}

#[tokio::main]
async fn main() {
    if let Err(fault) = run().await {
        eprintln!("{fault}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    match parse_cli(env::args().skip(1))? {
        CliCommand::Help => {
            print!("{}", help_text());
            Ok(())
        }
        CliCommand::Observe { full_re_render } => {
            let token = session_token()?;
            let response =
                request_with_autostart(&token, &DaemonRequest::Observe { full_re_render }).await?;
            print_response(response)
        }
        CliCommand::ActText { text } => {
            let token = session_token()?;
            let protocol = encode_text_delta(&text)?;
            let response = request_with_autostart(&token, &DaemonRequest::Act { protocol }).await?;
            print_response(response)
        }
        CliCommand::ActProtocol => {
            let token = session_token()?;
            let protocol = read_protocol_stdin()?;
            let response = request_with_autostart(&token, &DaemonRequest::Act { protocol }).await?;
            print_response(response)
        }
        CliCommand::InternalDaemon => {
            let token = session_token()?;
            run_daemon(daemon_addr()?, token_digest(&token)).await
        }
        CliCommand::InternalShutdown => shutdown_daemon(&session_token()?).await,
    }
}

fn parse_cli(args: impl IntoIterator<Item = String>) -> anyhow::Result<CliCommand> {
    let args = args.into_iter().collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(CliCommand::Help),
        [arg] if is_help_arg(arg) => Ok(CliCommand::Help),
        [arg] if arg == INTERNAL_DAEMON_ARG => Ok(CliCommand::InternalDaemon),
        [arg] if arg == INTERNAL_SHUTDOWN_ARG => Ok(CliCommand::InternalShutdown),
        [arg] if arg == "observe" => Ok(CliCommand::Observe {
            full_re_render: false,
        }),
        [cmd, flag] if cmd == "observe" && flag == "--full-re-render" => Ok(CliCommand::Observe {
            full_re_render: true,
        }),
        [cmd, flag] if cmd == "act" && flag == "--protocol" => Ok(CliCommand::ActProtocol),
        [cmd, text] if cmd == "act" => Ok(CliCommand::ActText { text: text.clone() }),
        [cmd] if cmd == "act" => {
            anyhow::bail!(
                "usage: agentview act <text> | agentview act --protocol\n\n{}",
                help_text()
            )
        }
        _ => anyhow::bail!("{}", help_text()),
    }
}

fn is_help_arg(argument: &str) -> bool {
    matches!(argument, "--help" | "-h" | "help")
}

fn help_text() -> &'static str {
    concat!(
        "agentview\n",
        "\n",
        "USAGE:\n",
        "  agentview observe [--full-re-render]\n",
        "  agentview act <text>\n",
        "  agentview act --protocol\n",
        "\n",
        "COMMANDS:\n",
        "  observe    Finish the current reaction and return the next observation\n",
        "  act        Submit text or a JSON-lines text protocol on stdin, then observe\n",
        "  help       Print this help\n",
        "\n",
        "PROTOCOL FRAMES:\n",
        "  {\"type\":\"text_delta\",\"text\":\"...\"}\n",
        "  {\"type\":\"text_complete\",\"text\":\"...\"}  optional, stops polling\n",
        "  {\"type\":\"disconnect\"}                            abnormal termination\n",
        "  stdin EOF                                               normal termination\n",
        "\n",
        "ENVIRONMENT:\n",
        "  AGENTVIEW_ADDR=127.0.0.1:<port>  Isolate concurrent CLI sessions\n",
        "  AGENTVIEW_TOKEN=<32+ byte secret>  Authenticate one CLI session\n",
    )
}

fn encode_text_delta(text: &str) -> anyhow::Result<String> {
    let frame = serde_json::json!({ "type": "text_delta", "text": text });
    Ok(format!("{}\n", serde_json::to_string(&frame)?))
}

fn read_protocol_stdin() -> anyhow::Result<String> {
    let stdin = std::io::stdin();
    read_protocol(&mut stdin.lock())
}

fn read_protocol(reader: &mut impl std::io::BufRead) -> anyhow::Result<String> {
    let mut protocol = String::new();
    let mut frames = 0_usize;
    loop {
        let start = protocol.len();
        let remaining = MAX_PROTOCOL_WIRE_BYTES + 1 - start.min(MAX_PROTOCOL_WIRE_BYTES + 1);
        if remaining == 0 {
            anyhow::bail!("external text protocol exceeded configured wire limit");
        }
        let read = reader
            .take(remaining as u64)
            .read_line(&mut protocol)
            .context("failed to read external text protocol from stdin")?;
        if protocol.len() > MAX_PROTOCOL_WIRE_BYTES {
            anyhow::bail!("external text protocol exceeded configured wire limit");
        }
        if read > 0 {
            frames += 1;
            if frames > MAX_PROTOCOL_FRAMES {
                anyhow::bail!("external text protocol exceeded configured frame limit");
            }
        }
        if read == 0 || protocol_frame_stops_input(&protocol[start..]) {
            break;
        }
    }
    Ok(protocol)
}

fn protocol_frame_stops_input(line: &str) -> bool {
    serde_json::from_str::<ExternalProtocolFrameHeader>(line)
        .is_ok_and(|frame| matches!(frame.kind.as_str(), "text_complete" | "disconnect"))
}

fn daemon_addr() -> anyhow::Result<SocketAddr> {
    let raw = env::var(ADDR_ENV).unwrap_or_else(|_| DEFAULT_ADDR.to_owned());
    let addr = raw
        .parse::<SocketAddr>()
        .with_context(|| format!("failed to parse {ADDR_ENV}={raw:?} as host:port"))?;
    if !addr.ip().is_loopback() {
        anyhow::bail!("agentview daemon address must be loopback, got {addr}");
    }
    Ok(addr)
}

fn session_token() -> anyhow::Result<String> {
    let token = env::var(TOKEN_ENV)
        .with_context(|| format!("{TOKEN_ENV} must be set to a private session token"))?;
    if !(MIN_SESSION_TOKEN_BYTES..=MAX_SESSION_TOKEN_BYTES).contains(&token.len()) {
        anyhow::bail!(
            "{TOKEN_ENV} must contain between {MIN_SESSION_TOKEN_BYTES} and \
             {MAX_SESSION_TOKEN_BYTES} bytes"
        );
    }
    if token.chars().any(char::is_control) {
        anyhow::bail!("{TOKEN_ENV} must not contain control characters");
    }
    Ok(token)
}

fn token_digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn random_nonce() -> anyhow::Result<[u8; 32]> {
    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce)
        .context("failed to generate external daemon authentication nonce")?;
    Ok(nonce)
}

fn protocol_mac(key: &[u8; 32], label: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a 32-byte session key");
    mac.update(label);
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

fn write_buffered_daemon_request_json(
    writer: impl std::io::Write,
    request: &DaemonRequest,
) -> anyhow::Result<()> {
    let mut writer = std::io::BufWriter::with_capacity(DAEMON_REQUEST_HMAC_BUFFER_BYTES, writer);
    serde_json::to_writer(&mut writer, request)
        .context("failed to authenticate external daemon request")?;
    writer
        .flush()
        .context("failed to flush external daemon request authentication")
}

fn daemon_request_authenticator(
    key: &[u8; 32],
    challenge: &[u8; 32],
    request: &DaemonRequest,
) -> anyhow::Result<HmacSha256> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a 32-byte session key");
    mac.update(CLIENT_PROOF_LABEL);
    mac.update(challenge);
    write_buffered_daemon_request_json(HmacJsonWriter { mac: &mut mac }, request)?;
    Ok(mac)
}

fn protocol_mac_matches(
    key: &[u8; 32],
    label: &[u8],
    parts: &[&[u8]],
    candidate: &[u8; 32],
) -> bool {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a 32-byte session key");
    mac.update(label);
    for part in parts {
        mac.update(part);
    }
    mac.verify_slice(candidate).is_ok()
}

async fn request_with_autostart(
    token: &str,
    request: &DaemonRequest,
) -> anyhow::Result<ClientDaemonResponse> {
    let addr = daemon_addr()?;
    match send_once(addr, token, request).await {
        Ok(response) => return Ok(response),
        Err(SendOnceFault::NoRetry(fault)) => return Err(fault),
        Err(SendOnceFault::Unavailable(_)) => {}
    }

    spawn_daemon(addr, token)?;

    let mut last_fault = None;
    for _ in 0..100 {
        match send_once(addr, token, request).await {
            Ok(response) => return Ok(response),
            Err(SendOnceFault::Unavailable(fault)) => last_fault = Some(fault),
            Err(SendOnceFault::NoRetry(fault)) => return Err(fault),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    Err(last_fault.unwrap_or_else(|| anyhow::anyhow!("failed to reach agentview server")))
}

async fn shutdown_daemon(token: &str) -> anyhow::Result<()> {
    match send_once(daemon_addr()?, token, &DaemonRequest::Shutdown).await {
        Ok(ClientDaemonResponse {
            envelope: DaemonResponseEnvelope::Ok { .. },
            ..
        }) => Ok(()),
        Ok(ClientDaemonResponse {
            envelope: DaemonResponseEnvelope::Error { .. },
            error_message: Some(message),
            ..
        }) => anyhow::bail!("{message}"),
        Ok(response) => anyhow::bail!("unexpected shutdown response: {:?}", response.envelope),
        Err(fault) => Err(fault.into_error()),
    }
}

fn spawn_daemon(addr: SocketAddr, token: &str) -> anyhow::Result<()> {
    let current_exe = env::current_exe()?;
    let mut command = Command::new(current_exe);
    configure_daemon_environment(&mut command, addr, token);
    command
        .arg(INTERNAL_DAEMON_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);

    command
        .spawn()
        .context("failed to launch hidden agentview server")?;
    Ok(())
}

fn configure_daemon_environment(command: &mut Command, addr: SocketAddr, token: &str) {
    // Without Cargo's coverage path, an instrumented daemon writes default_*.profraw in the CWD.
    let explicit_llvm_profile_file = command
        .get_envs()
        .find(|(name, _)| *name == std::ffi::OsStr::new(LLVM_PROFILE_FILE_ENV));
    let llvm_profile_file = match explicit_llvm_profile_file {
        Some((_, value)) => value.map(std::ffi::OsStr::to_os_string),
        None => env::var_os(LLVM_PROFILE_FILE_ENV),
    };
    let llvm_profile_file =
        llvm_profile_file.unwrap_or_else(|| DEFAULT_DAEMON_LLVM_PROFILE_FILE.into());

    #[cfg(windows)]
    let windows_launch_environment = ["SystemRoot", "WINDIR"]
        .map(|name| (name, env::var_os(name)))
        .into_iter()
        .filter_map(|(name, value)| value.map(|value| (name, value)));

    command.env_clear();
    #[cfg(windows)]
    command.envs(windows_launch_environment);
    command
        .env(LLVM_PROFILE_FILE_ENV, llvm_profile_file)
        .env(ADDR_ENV, addr.to_string())
        .env(TOKEN_ENV, token);
}

async fn send_once(
    addr: SocketAddr,
    token: &str,
    request: &DaemonRequest,
) -> Result<ClientDaemonResponse, SendOnceFault> {
    let session_key = token_digest(token);
    let encoded_request = encode_daemon_request(request).map_err(SendOnceFault::NoRetry)?;
    let mut stream = timeout(DAEMON_CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .with_context(|| format!("timed out connecting to {addr}"))
        .and_then(|stream| stream.with_context(|| format!("failed to connect to {addr}")))
        .map_err(SendOnceFault::Unavailable)?;

    let response = timeout(DAEMON_RESPONSE_TIMEOUT, async move {
        let client_nonce = random_nonce()?;
        let hello = serde_json::to_vec(&DaemonAuthFrame::ClientHello {
            nonce: client_nonce,
        })?;
        stream.write_all(&hello).await?;
        stream.write_all(b"\n").await?;

        let mut reader = BufReader::new(stream);
        let proof =
            read_bounded_line(&mut reader, MAX_DAEMON_AUTH_LINE_BYTES, "server proof").await?;
        let (challenge, server_proof) = match serde_json::from_str::<DaemonAuthFrame>(&proof)
            .context("external daemon proof was invalid")?
        {
            DaemonAuthFrame::ServerHello { challenge, proof } => (challenge, proof),
            DaemonAuthFrame::ClientHello { .. } | DaemonAuthFrame::ClientProof { .. } => {
                anyhow::bail!("external daemon proof was invalid")
            }
        };
        if !protocol_mac_matches(
            &session_key,
            SERVER_PROOF_LABEL,
            &[&client_nonce, &challenge],
            &server_proof,
        ) {
            anyhow::bail!("not authorized");
        }

        let client_proof = protocol_mac(
            &session_key,
            CLIENT_AUTH_LABEL,
            &[&challenge, &client_nonce],
        );
        let client_proof = serde_json::to_vec(&DaemonAuthFrame::ClientProof {
            proof: client_proof,
        })?;
        reader.get_mut().write_all(&client_proof).await?;
        reader.get_mut().write_all(b"\n").await?;

        let authentication: [u8; 32] =
            daemon_request_authenticator(&session_key, &challenge, request)?
                .finalize()
                .into_bytes()
                .into();
        let envelope = serde_json::to_vec(&AuthenticatedDaemonEnvelope { authentication })?;
        if envelope.len() >= MAX_DAEMON_AUTH_LINE_BYTES {
            anyhow::bail!("external daemon authentication frame exceeded configured wire limit");
        }
        reader.get_mut().write_all(&encoded_request.header).await?;
        reader.get_mut().write_all(b"\n").await?;
        reader.get_mut().write_all(encoded_request.payload).await?;
        reader.get_mut().write_all(&envelope).await?;
        reader.get_mut().write_all(b"\n").await?;
        reader.get_mut().shutdown().await?;

        read_client_daemon_response(&mut reader, request).await
    })
    .await
    .with_context(|| format!("timed out waiting for agentview server at {addr}"))
    .and_then(|response| response)
    .map_err(SendOnceFault::NoRetry)?;

    Ok(response)
}

async fn read_bounded_line<R>(
    reader: &mut R,
    limit: usize,
    label: &'static str,
) -> anyhow::Result<String>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await
        .with_context(|| format!("failed to read external daemon {label}"))?;
    if bytes.len() > limit {
        anyhow::bail!("external daemon {label} exceeded configured wire limit");
    }
    String::from_utf8(bytes).with_context(|| format!("external daemon {label} was not UTF-8"))
}

fn print_response(response: ClientDaemonResponse) -> anyhow::Result<()> {
    match response.envelope {
        DaemonResponseEnvelope::Error { .. } => anyhow::bail!(
            "{}",
            response
                .error_message
                .context("external daemon error response had no message")?
        ),
        DaemonResponseEnvelope::Observation { .. } | DaemonResponseEnvelope::Ok { .. } => {
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            stdout.write_all(response.wire.as_bytes())?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
            Ok(())
        }
    }
}

async fn run_daemon(addr: SocketAddr, token_digest: [u8; 32]) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let mut state = new_daemon_state();
    let mut authentications = JoinSet::new();

    loop {
        tokio::select! {
            accepted = listener.accept(), if authentications.len() < MAX_PENDING_AUTHENTICATIONS => {
                let (stream, _) = accepted?;
                authentications.spawn(authenticate_connection(token_digest, stream));
            }
            completed = authentications.join_next(), if !authentications.is_empty() => {
                match completed.expect("non-empty authentication set") {
                    Ok(Ok(connection)) => {
                        match handle_authenticated_connection(&mut state, connection).await {
                            Ok(true) => {
                                authentications.abort_all();
                                break;
                            }
                            Ok(false) => {}
                            Err(fault) => {
                                tracing::warn!(error = %fault, "external daemon connection failed");
                            }
                        }
                    }
                    Ok(Err(fault)) => {
                        tracing::warn!(error = %fault, "external daemon connection failed");
                    }
                    Err(fault) => {
                        tracing::warn!(error = %fault, "external daemon authentication task failed");
                    }
                }
            }
        }
    }
    Ok(())
}

fn new_daemon_state() -> DaemonState {
    let external = ExternalCliSession::mount(ExternalCliState::default())
        .expect("external CLI application must mount");
    DaemonState {
        external: Some(external),
    }
}

async fn authenticate_connection(
    token_digest: [u8; 32],
    stream: TcpStream,
) -> anyhow::Result<AuthenticatedDaemonConnection> {
    let mut reader = BufReader::new(stream);
    let hello = timeout(
        DAEMON_REQUEST_TIMEOUT,
        read_bounded_line(&mut reader, MAX_DAEMON_AUTH_LINE_BYTES, "client hello"),
    )
    .await
    .context("timed out reading external daemon client hello")??;
    let client_nonce = match serde_json::from_str::<DaemonAuthFrame>(&hello) {
        Ok(DaemonAuthFrame::ClientHello { nonce }) => nonce,
        _ => anyhow::bail!("external daemon client hello was invalid"),
    };
    let challenge = random_nonce()?;
    let proof = protocol_mac(
        &token_digest,
        SERVER_PROOF_LABEL,
        &[&client_nonce, &challenge],
    );
    write_auth_frame(
        reader.get_mut(),
        &DaemonAuthFrame::ServerHello { challenge, proof },
    )
    .await?;

    let client_proof = timeout(
        DAEMON_REQUEST_TIMEOUT,
        read_bounded_line(&mut reader, MAX_DAEMON_AUTH_LINE_BYTES, "client proof"),
    )
    .await
    .context("timed out reading external daemon client proof")??;
    let client_proof = match serde_json::from_str::<DaemonAuthFrame>(&client_proof) {
        Ok(DaemonAuthFrame::ClientProof { proof }) => proof,
        _ => anyhow::bail!("external daemon client proof was invalid"),
    };
    if !protocol_mac_matches(
        &token_digest,
        CLIENT_AUTH_LABEL,
        &[&challenge, &client_nonce],
        &client_proof,
    ) {
        write_response(
            reader.into_inner(),
            &DaemonResponse::Error {
                message: "not authorized".to_owned(),
            },
        )
        .await?;
        anyhow::bail!("external daemon client was not authorized");
    }

    let (request, request_payload, envelope) = timeout(DAEMON_REQUEST_TIMEOUT, async {
        let mut request_header = read_bounded_line(
            &mut reader,
            MAX_DAEMON_AUTH_LINE_BYTES,
            "authenticated request header",
        )
        .await?;
        anyhow::ensure!(
            request_header.pop() == Some('\n'),
            "external daemon authenticated request header was not terminated"
        );
        let request = serde_json::from_str::<AuthenticatedDaemonRequest>(&request_header)
            .context("external daemon authenticated request header was invalid")?;
        let protocol_bytes = match request {
            AuthenticatedDaemonRequest::Act { protocol_bytes } => protocol_bytes,
            AuthenticatedDaemonRequest::Observe { .. } | AuthenticatedDaemonRequest::Shutdown => 0,
        };
        anyhow::ensure!(
            protocol_bytes <= MAX_PROTOCOL_WIRE_BYTES,
            "external text protocol exceeded configured wire limit"
        );

        let mut request_payload = vec![0_u8; protocol_bytes];
        reader
            .read_exact(&mut request_payload)
            .await
            .context("failed to read external daemon authenticated request payload")?;

        let envelope = read_bounded_line(
            &mut reader,
            MAX_DAEMON_AUTH_LINE_BYTES,
            "authenticated request footer",
        )
        .await?;
        let envelope = serde_json::from_str::<AuthenticatedDaemonEnvelope>(&envelope)
            .context("external daemon authenticated request footer was invalid")?;
        anyhow::Ok((request, request_payload, envelope))
    })
    .await
    .context("timed out reading external daemon authenticated request")??;
    let request = match request {
        AuthenticatedDaemonRequest::Observe { full_re_render } => {
            DaemonRequest::Observe { full_re_render }
        }
        AuthenticatedDaemonRequest::Act { .. } => DaemonRequest::Act {
            protocol: String::from_utf8(request_payload)
                .context("external daemon request payload was not UTF-8")?,
        },
        AuthenticatedDaemonRequest::Shutdown => DaemonRequest::Shutdown,
    };
    if daemon_request_authenticator(&token_digest, &challenge, &request)?
        .verify_slice(&envelope.authentication)
        .is_err()
    {
        write_response(
            reader.into_inner(),
            &DaemonResponse::Error {
                message: "not authorized".to_owned(),
            },
        )
        .await?;
        anyhow::bail!("external daemon request was not authorized");
    }
    Ok(AuthenticatedDaemonConnection {
        stream: reader.into_inner(),
        request,
    })
}

async fn handle_authenticated_connection(
    state: &mut DaemonState,
    connection: AuthenticatedDaemonConnection,
) -> anyhow::Result<bool> {
    let response = match connection.request {
        DaemonRequest::Observe { full_re_render } => {
            let external = state
                .external
                .as_mut()
                .context("external daemon application is shutting down")?;
            observe_external(external, full_re_render).await
        }
        DaemonRequest::Act { protocol } => {
            let external = state
                .external
                .take()
                .context("external daemon application is shutting down")?;
            let mut stream = connection.stream;
            let transition =
                act_external_with_provisional(external, protocol, Some(&mut stream)).await;
            let ExternalCliTransition {
                session,
                response,
                preencoded_response,
                provisional_disposition,
            } = transition;
            state.external = session;
            write_external_cli_transition_response(
                &mut stream,
                &response,
                preencoded_response,
                provisional_disposition,
            )
            .await?;
            return Ok(false);
        }
        DaemonRequest::Shutdown => {
            let external = state
                .external
                .take()
                .context("external daemon application is already shutting down")?;
            let response = match external.shutdown().await {
                Ok(()) => DaemonResponse::Ok,
                Err(fault) => DaemonResponse::Error {
                    message: fault.to_string(),
                },
            };
            write_response(connection.stream, &response).await?;
            return Ok(true);
        }
    };

    write_response(connection.stream, &response).await?;
    Ok(false)
}

async fn observe_external(
    session: &mut ExternalCliSession,
    full_re_render: bool,
) -> DaemonResponse {
    if let Some(observation) = session.pending_observation.take() {
        return observation_response(
            "observe",
            Ok::<ExternalObservation, std::convert::Infallible>(observation),
        );
    }
    let observation = if full_re_render {
        session.application.observe_full().await
    } else {
        session.application.observe().await
    };
    observation_response("observe", observation)
}

struct ExternalCliTransition {
    session: Option<ExternalCliSession>,
    response: DaemonResponse,
    preencoded_response: Option<EncodedDaemonResponse>,
    provisional_disposition: Option<DaemonResponseDisposition>,
}

impl ExternalCliTransition {
    fn retained(session: ExternalCliSession, response: DaemonResponse) -> Self {
        Self {
            session: Some(session),
            response,
            preencoded_response: None,
            provisional_disposition: None,
        }
    }

    fn retained_preencoded(
        session: ExternalCliSession,
        response: DaemonResponse,
        preencoded_response: EncodedDaemonResponse,
    ) -> Self {
        Self {
            session: Some(session),
            response,
            preencoded_response: Some(preencoded_response),
            provisional_disposition: None,
        }
    }

    fn retained_provisional_commit(
        session: ExternalCliSession,
        response: DaemonResponse,
        ticket: DaemonResponseTicket,
    ) -> Self {
        Self {
            session: Some(session),
            response,
            preencoded_response: None,
            provisional_disposition: Some(DaemonResponseDisposition::Commit { ticket }),
        }
    }

    fn with_provisional_replacement(mut self, ticket: Option<DaemonResponseTicket>) -> Self {
        if let Some(ticket) = ticket {
            self.preencoded_response = None;
            self.provisional_disposition = Some(DaemonResponseDisposition::Replace { ticket });
        }
        self
    }

    fn closed(response: DaemonResponse) -> Self {
        Self {
            session: None,
            response,
            preencoded_response: None,
            provisional_disposition: None,
        }
    }
}

#[cfg(test)]
async fn act_external(session: ExternalCliSession, protocol: String) -> ExternalCliTransition {
    act_external_with_provisional(session, protocol, None).await
}

async fn act_external_with_provisional(
    session: ExternalCliSession,
    protocol: String,
    provisional_stream: Option<&mut TcpStream>,
) -> ExternalCliTransition {
    if protocol.len() > MAX_PROTOCOL_WIRE_BYTES {
        return ExternalCliTransition::retained(
            session,
            DaemonResponse::Error {
                message: "external text protocol exceeded configured wire limit".to_owned(),
            },
        );
    }
    if session.pending_observation.is_some() {
        return ExternalCliTransition::retained(
            session,
            DaemonResponse::Error {
                message: "observe the pending external Full before acting".to_owned(),
            },
        );
    }

    let snapshot = match session.snapshot() {
        Ok(snapshot) => snapshot,
        Err(_) => {
            return ExternalCliTransition::retained(
                session,
                DaemonResponse::Error {
                    message: "external CLI application state is unavailable".to_owned(),
                },
            );
        }
    };
    let replacement = match ExternalCliSession::mount(snapshot) {
        Ok(replacement) => replacement,
        Err(_) => {
            return ExternalCliTransition::retained(
                session,
                DaemonResponse::Error {
                    message: "external CLI replacement could not be created".to_owned(),
                },
            );
        }
    };

    rotate_external_act_with_provisional(session, replacement, protocol, provisional_stream).await
}

#[cfg(test)]
async fn rotate_external_act(
    current: ExternalCliSession,
    replacement: ExternalCliSession,
    protocol: String,
) -> ExternalCliTransition {
    rotate_external_act_with_provisional(current, replacement, protocol, None).await
}

async fn rotate_external_act_with_provisional(
    mut current: ExternalCliSession,
    mut replacement: ExternalCliSession,
    protocol: String,
    mut provisional_stream: Option<&mut TcpStream>,
) -> ExternalCliTransition {
    let baseline = match current.snapshot() {
        Ok(snapshot) => snapshot,
        Err(_) => {
            return close_failed_rotation(
                current,
                replacement,
                None,
                "external CLI application state is unavailable",
            )
            .await;
        }
    };
    let progress = match ExternalCliPreparedProgress::new() {
        Ok(progress) => Arc::new(progress),
        Err(_) => {
            return close_failed_rotation(
                current,
                replacement,
                None,
                "external CLI prepared verifier could not reserve bounded storage",
            )
            .await;
        }
    };
    if current
        .install_prepared_progress(Arc::clone(&progress))
        .is_err()
    {
        return close_failed_rotation(
            current,
            replacement,
            None,
            "external CLI prepared verifier could not be installed",
        )
        .await;
    }
    let candidate_baseline = baseline.clone();
    let diagnostic_protocol = Arc::new(protocol.clone());
    let candidate_protocol = Arc::clone(&diagnostic_protocol);
    let speculation_cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&speculation_cancelled);
    #[cfg(test)]
    let speculation_barrier = current.speculation_barrier.clone();
    #[cfg(test)]
    let worker_barrier = speculation_barrier.clone();
    let worker_progress = Arc::clone(&progress);
    let candidate_task = tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        if let Some(barrier) = &worker_barrier {
            barrier.enter_and_wait();
        }
        let candidate = prepare_external_cli_candidate(
            candidate_protocol.as_str(),
            candidate_baseline,
            worker_progress,
            &worker_cancelled,
        );
        #[cfg(test)]
        if let Some(barrier) = &worker_barrier {
            barrier.finish();
        }
        candidate
    });
    let candidate_abort = candidate_task.abort_handle();
    let mut cancellation = ExternalCliSpeculationCancellation::new(
        speculation_cancelled,
        candidate_abort,
        Some(Arc::clone(&progress)),
        #[cfg(test)]
        speculation_barrier,
    );

    let operation_future = async {
        AssertUnwindSafe(
            current
                .application
                .act(ExternalAct::__from_cli_json_lines(protocol)),
        )
        .catch_unwind()
        .await
    };
    let speculative_future = async {
        let candidate = candidate_task
            .await
            .context("external CLI speculative candidate task failed")??;
        let Some(candidate) = candidate else {
            return Ok::<_, anyhow::Error>(None);
        };
        replacement.install_snapshot(candidate.snapshot.clone())?;
        let full = AssertUnwindSafe(replacement.application.observe())
            .catch_unwind()
            .await;
        let prepared_response = match &full {
            Ok(Ok(observation)) => {
                let response = successful_observation_response("act", observation);
                prepare_daemon_response(response).ok()
            }
            Ok(Err(_)) | Err(_) => None,
        };
        let provisional_ticket = match (prepared_response.as_ref(), provisional_stream.as_mut()) {
            (Some(prepared), Some(stream)) => {
                let ticket = random_nonce()?;
                write_provisional_response(stream, ticket, &prepared.encoded).await?;
                Some(ticket)
            }
            (None, _) | (_, None) => None,
        };
        Ok(Some((
            candidate,
            full,
            prepared_response,
            provisional_ticket,
        )))
    };
    let (operation, speculative) = match coordinate_external_cli_speculation(
        operation_future,
        speculative_future,
        &mut cancellation,
    )
    .await
    {
        Ok(completed) => completed,
        Err(payload) => {
            shutdown_after_operation_panic(current, replacement).await;
            panic::resume_unwind(payload);
        }
    };
    let (
        candidate,
        speculative_full,
        speculative_response,
        provisional_ticket,
        speculation_tainted,
    ) = match speculative {
        Ok(Some((candidate, Ok(full), prepared_response, provisional_ticket))) => (
            Some(candidate),
            Some(full),
            prepared_response,
            provisional_ticket,
            false,
        ),
        Ok(Some((_, Err(payload), _, _))) => {
            shutdown_after_operation_panic(current, replacement).await;
            panic::resume_unwind(payload);
        }
        Ok(None) => (None, None, None, None, false),
        Err(_) => (None, None, None, None, true),
    };
    let operation_error = act_operation_error(&operation, diagnostic_protocol.as_str());

    let candidate_verified = operation.is_ok()
        && candidate
            .as_ref()
            .is_some_and(|candidate| progress.is_exact_success(candidate.events));
    let snapshot = if candidate_verified {
        candidate
            .as_ref()
            .expect("verified prepared candidate is present")
            .snapshot
            .clone()
    } else {
        match progress.replay_accepted_prefix(baseline) {
            Ok(snapshot) => snapshot,
            Err(_) => {
                return close_failed_rotation(
                    current,
                    replacement,
                    operation_error,
                    "external CLI accepted prefix could not be recovered",
                )
                .await
                .with_provisional_replacement(provisional_ticket);
            }
        }
    };
    let mut discarded_cleanup_error = None;
    let full = if candidate_verified {
        match speculative_full.expect("verified candidate prepared one speculative Full") {
            Ok(full) => full,
            Err(fault) => {
                return close_failed_rotation(
                    current,
                    replacement,
                    operation_error,
                    &fault.to_string(),
                )
                .await
                .with_provisional_replacement(provisional_ticket);
            }
        }
    } else {
        if candidate.is_some() || speculation_tainted {
            let recovery = match ExternalCliSession::mount(snapshot) {
                Ok(recovery) => recovery,
                Err(_) => {
                    return close_failed_rotation(
                        current,
                        replacement,
                        operation_error,
                        "external CLI recovery replacement could not be created",
                    )
                    .await
                    .with_provisional_replacement(provisional_ticket);
                }
            };
            discarded_cleanup_error =
                replacement
                    .shutdown()
                    .await
                    .err()
                    .map(|fault| DaemonResponse::Error {
                        message: fault.to_string(),
                    });
            replacement = recovery;
        } else if replacement.install_snapshot(snapshot).is_err() {
            return close_failed_rotation(
                current,
                replacement,
                operation_error,
                "external CLI replacement state is unavailable",
            )
            .await
            .with_provisional_replacement(provisional_ticket);
        }
        match replacement.application.observe().await {
            Ok(full) => full,
            Err(fault) => {
                return close_failed_rotation(
                    current,
                    replacement,
                    operation_error,
                    &fault.to_string(),
                )
                .await
                .with_provisional_replacement(provisional_ticket);
            }
        }
    };

    let cleanup_error = current
        .shutdown()
        .await
        .err()
        .map(|fault| DaemonResponse::Error {
            message: fault.to_string(),
        });
    replacement.pending_observation = Some(full);
    if let Some(error) = operation_error
        .or(discarded_cleanup_error)
        .or(cleanup_error)
    {
        return ExternalCliTransition::retained(replacement, error)
            .with_provisional_replacement(provisional_ticket);
    }

    let full = replacement
        .pending_observation
        .take()
        .expect("successful rotation prepared one Full");
    if candidate_verified {
        if let Some(prepared) = speculative_response {
            drop(full);
            if let Some(ticket) = provisional_ticket {
                return ExternalCliTransition::retained_provisional_commit(
                    replacement,
                    prepared.response,
                    ticket,
                );
            }
            return ExternalCliTransition::retained_preencoded(
                replacement,
                prepared.response,
                prepared.encoded,
            );
        }
    }
    let response = observation_response(
        "act",
        Ok::<ExternalObservation, std::convert::Infallible>(full),
    );
    ExternalCliTransition::retained(replacement, response)
        .with_provisional_replacement(provisional_ticket)
}

fn act_operation_error<E>(
    operation: &Result<ExternalObservation, E>,
    diagnostic_protocol: &str,
) -> Option<DaemonResponse>
where
    E: std::fmt::Display,
{
    operation.as_ref().err().map(|fault| DaemonResponse::Error {
        message: external_protocol_diagnostic(diagnostic_protocol)
            .map(str::to_owned)
            .unwrap_or_else(|| fault.to_string()),
    })
}

#[cfg(test)]
fn select_rotation_error(
    operation_error: Option<DaemonResponse>,
    cleanup_error: Option<DaemonResponse>,
) -> Option<DaemonResponse> {
    operation_error.or(cleanup_error)
}

async fn close_failed_rotation(
    current: ExternalCliSession,
    replacement: ExternalCliSession,
    operation_error: Option<DaemonResponse>,
    message: &str,
) -> ExternalCliTransition {
    let replacement_cleanup = replacement.shutdown().await;
    let current_cleanup = current.shutdown().await;
    let transition_error = DaemonResponse::Error {
        message: message.to_owned(),
    };
    let cleanup_error = replacement_cleanup
        .err()
        .or_else(|| current_cleanup.err())
        .map(|fault| DaemonResponse::Error {
            message: fault.to_string(),
        });
    ExternalCliTransition::closed(
        operation_error
            .or(cleanup_error)
            .unwrap_or(transition_error),
    )
}

async fn shutdown_after_operation_panic(
    current: ExternalCliSession,
    replacement: ExternalCliSession,
) {
    let current = AssertUnwindSafe(current.shutdown()).catch_unwind();
    let replacement = AssertUnwindSafe(replacement.shutdown()).catch_unwind();
    let _ = tokio::join!(current, replacement);
}

fn external_protocol_diagnostic(protocol: &str) -> Option<&'static str> {
    for line in protocol.lines() {
        match serde_json::from_str::<ExternalCliProtocolFrame>(line) {
            Ok(
                ExternalCliProtocolFrame::TextDelta { text }
                | ExternalCliProtocolFrame::TextComplete { text },
            ) => {
                if ensure_xml_renderable_text(&text).is_err() {
                    return Some("external text is not allowed in XML content");
                }
            }
            Ok(ExternalCliProtocolFrame::Disconnect) => {
                return Some("external text protocol disconnected abnormally");
            }
            Err(_) => return Some("external text protocol frame was invalid"),
        }
    }
    None
}

fn successful_observation_response(
    event: &str,
    observation: &ExternalObservation,
) -> DaemonResponse {
    let mode = match observation.kind() {
        ExternalObservationKind::Full => "full",
        ExternalObservationKind::Delta => "delta",
        _ => {
            return DaemonResponse::Error {
                message: "unsupported external observation kind".to_owned(),
            };
        }
    };
    DaemonResponse::Observation {
        event: event.to_owned(),
        mode: mode.to_owned(),
        generation: format!("{:?}", observation.generation()),
        base_generation: observation
            .base_generation()
            .map(|generation| format!("{generation:?}")),
        content: observation.content().to_owned(),
    }
}

fn observation_response<E>(
    event: &str,
    observation: Result<ExternalObservation, E>,
) -> DaemonResponse
where
    E: std::fmt::Display,
{
    match observation {
        Ok(observation) => successful_observation_response(event, &observation),
        Err(fault) => DaemonResponse::Error {
            message: fault.to_string(),
        },
    }
}

fn prepare_daemon_response(response: DaemonResponse) -> anyhow::Result<PreparedDaemonResponse> {
    let (envelope, line) = encode_daemon_response(&response)?;
    Ok(PreparedDaemonResponse {
        response,
        encoded: EncodedDaemonResponse { envelope, line },
    })
}

async fn write_response(stream: TcpStream, response: &DaemonResponse) -> anyhow::Result<()> {
    let mut stream = stream;
    write_response_to(&mut stream, response).await
}

async fn write_response_to(
    stream: &mut TcpStream,
    response: &DaemonResponse,
) -> anyhow::Result<()> {
    let (envelope, line) = encode_daemon_response(response)?;
    write_encoded_response_to(stream, EncodedDaemonResponse { envelope, line }).await
}

async fn write_encoded_response_to(
    stream: &mut TcpStream,
    response: EncodedDaemonResponse,
) -> anyhow::Result<()> {
    let envelope = serde_json::to_vec(&response.envelope)?;
    if envelope.len() >= MAX_DAEMON_AUTH_LINE_BYTES {
        anyhow::bail!("external daemon response envelope exceeded configured wire limit");
    }
    timeout(DAEMON_RESPONSE_WRITE_TIMEOUT, async {
        stream.write_all(&envelope).await?;
        stream.write_all(b"\n").await?;
        stream.write_all(&response.line).await?;
        stream.write_all(b"\n").await?;
        anyhow::Ok(())
    })
    .await
    .context("timed out writing external daemon response")??;
    Ok(())
}

async fn write_provisional_response(
    stream: &mut TcpStream,
    ticket: DaemonResponseTicket,
    response: &EncodedDaemonResponse,
) -> anyhow::Result<()> {
    let envelope = serde_json::to_vec(&DaemonProvisionalResponseEnvelope::Provisional {
        ticket,
        response: response.envelope.clone(),
    })?;
    anyhow::ensure!(
        envelope.len() < MAX_DAEMON_AUTH_LINE_BYTES,
        "external daemon provisional response envelope exceeded configured wire limit"
    );
    timeout(DAEMON_RESPONSE_WRITE_TIMEOUT, async {
        stream.write_all(&envelope).await?;
        stream.write_all(b"\n").await?;
        stream.write_all(&response.line).await?;
        stream.write_all(b"\n").await?;
        anyhow::Ok(())
    })
    .await
    .context("timed out writing external daemon provisional response")??;
    Ok(())
}

async fn write_response_disposition(
    stream: &mut TcpStream,
    disposition: DaemonResponseDisposition,
) -> anyhow::Result<()> {
    let disposition = serde_json::to_vec(&disposition)?;
    anyhow::ensure!(
        disposition.len() < MAX_DAEMON_AUTH_LINE_BYTES,
        "external daemon response disposition exceeded configured wire limit"
    );
    timeout(DAEMON_RESPONSE_WRITE_TIMEOUT, async {
        stream.write_all(&disposition).await?;
        stream.write_all(b"\n").await?;
        anyhow::Ok(())
    })
    .await
    .context("timed out writing external daemon response disposition")??;
    Ok(())
}

async fn write_external_cli_transition_response(
    stream: &mut TcpStream,
    response: &DaemonResponse,
    preencoded_response: Option<EncodedDaemonResponse>,
    provisional_disposition: Option<DaemonResponseDisposition>,
) -> anyhow::Result<()> {
    match provisional_disposition {
        Some(disposition @ DaemonResponseDisposition::Commit { .. }) => {
            anyhow::ensure!(
                preencoded_response.is_none(),
                "committed provisional response unexpectedly retained a final body"
            );
            write_response_disposition(stream, disposition).await
        }
        Some(disposition @ DaemonResponseDisposition::Replace { .. }) => {
            write_response_disposition(stream, disposition).await?;
            write_response_to(stream, response).await
        }
        None => {
            if let Some(encoded) = preencoded_response {
                write_encoded_response_to(stream, encoded).await
            } else {
                write_response_to(stream, response).await
            }
        }
    }
}

async fn write_auth_frame(stream: &mut TcpStream, frame: &DaemonAuthFrame) -> anyhow::Result<()> {
    let line = serde_json::to_string(frame)?;
    if line.len() > MAX_DAEMON_AUTH_LINE_BYTES {
        anyhow::bail!("external daemon authentication frame exceeded configured wire limit");
    }
    timeout(DAEMON_RESPONSE_WRITE_TIMEOUT, async {
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        anyhow::Ok(())
    })
    .await
    .context("timed out writing external daemon authentication proof")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAEMON_ENV_PROBE_ENV: &str = "AGENTVIEW_DAEMON_ENV_PROBE";
    const DAEMON_ENV_SENTINEL: &str = "AGENTVIEW_TEST_AMBIENT_SECRET";
    const DAEMON_ENV_PROBE_ADDR: &str = "127.0.0.1:47639";
    const DAEMON_ENV_PROBE_TOKEN: &str = "test-only-daemon-token";
    const EXPLICIT_DAEMON_LLVM_PROFILE_FILE: &str =
        "target/custom-llvm-profraw/daemon-%p-%m.profraw";
    const LLVM_PROFILE_RUNTIME_ENV: &str = "__LLVM_PROFILE_RT_INIT_ONCE";

    fn generation_target(generation: &str) -> &str {
        let target = generation
            .split_once("target: TargetIdentity(")
            .expect("Frame generation contains its target")
            .1;
        target
            .split_once(')')
            .expect("target identity is delimited")
            .0
    }

    fn assert_signal_is_fenced(signal: &Signal<ExternalCliComponentState>) {
        assert!(
            signal.with(|_| ()).is_err(),
            "consumed Application left an old Signal active"
        );
    }

    #[test]
    fn structured_framing_preserves_legacy_request_bytes_and_mac_input() {
        fn reconstruct(header: &[u8], payload: &[u8]) -> DaemonRequest {
            match serde_json::from_slice::<AuthenticatedDaemonRequest>(header).unwrap() {
                AuthenticatedDaemonRequest::Observe { full_re_render } => {
                    assert!(payload.is_empty());
                    DaemonRequest::Observe { full_re_render }
                }
                AuthenticatedDaemonRequest::Act { protocol_bytes } => {
                    assert_eq!(protocol_bytes, payload.len());
                    DaemonRequest::Act {
                        protocol: String::from_utf8(payload.to_vec()).unwrap(),
                    }
                }
                AuthenticatedDaemonRequest::Shutdown => {
                    assert!(payload.is_empty());
                    DaemonRequest::Shutdown
                }
            }
        }

        fn assert_legacy_mac_input(
            key: &[u8; 32],
            challenge: &[u8; 32],
            request: &DaemonRequest,
        ) -> [u8; 32] {
            let legacy = serde_json::to_vec(request).unwrap();
            let mut streamed = Vec::new();
            serde_json::to_writer(&mut streamed, request).unwrap();
            assert_eq!(streamed, legacy, "streaming serde bytes changed");

            let expected = protocol_mac(key, CLIENT_PROOF_LABEL, &[challenge, legacy.as_slice()]);
            let actual: [u8; 32] = daemon_request_authenticator(key, challenge, request)
                .unwrap()
                .finalize()
                .into_bytes()
                .into();
            assert_eq!(actual, expected, "streaming HMAC input changed");
            assert!(daemon_request_authenticator(key, challenge, request)
                .unwrap()
                .verify_slice(&expected)
                .is_ok());
            expected
        }

        let requests = [
            DaemonRequest::Observe {
                full_re_render: true,
            },
            DaemonRequest::Act {
                protocol: "{\"type\":\"text_delta\",\"text\":\"quote-\\\"-\\\\-\\n-\u{4e2d}\"}\n"
                    .to_owned(),
            },
            DaemonRequest::Shutdown,
        ];
        let key = [0x31_u8; 32];
        let challenge = [0x52_u8; 32];
        for request in requests {
            let encoded = encode_daemon_request(&request).unwrap();
            assert!(encoded.header.len() < MAX_DAEMON_AUTH_LINE_BYTES);
            assert!(encoded.payload.len() <= MAX_PROTOCOL_WIRE_BYTES);
            let reconstructed = reconstruct(&encoded.header, encoded.payload);
            assert_eq!(reconstructed, request);
            assert_eq!(
                serde_json::to_vec(&reconstructed).unwrap(),
                serde_json::to_vec(&request).unwrap()
            );

            let authentication = assert_legacy_mac_input(&key, &challenge, &request);
            let legacy = serde_json::to_vec(&request).unwrap();
            assert!(protocol_mac_matches(
                &key,
                CLIENT_PROOF_LABEL,
                &[&challenge, &legacy],
                &authentication,
            ));
            let mut rejected = authentication;
            rejected[0] ^= 1;
            assert!(daemon_request_authenticator(&key, &challenge, &request)
                .unwrap()
                .verify_slice(&rejected)
                .is_err());

            let envelope = AuthenticatedDaemonEnvelope { authentication };
            let envelope_bytes = serde_json::to_vec(&envelope).unwrap();
            assert!(envelope_bytes.len() < MAX_DAEMON_AUTH_LINE_BYTES);
            assert_eq!(
                serde_json::from_slice::<AuthenticatedDaemonEnvelope>(&envelope_bytes).unwrap(),
                envelope
            );
        }

        let maximum = DaemonRequest::Act {
            protocol: "x".repeat(MAX_PROTOCOL_WIRE_BYTES),
        };
        assert_eq!(
            encode_daemon_request(&maximum).unwrap().payload.len(),
            MAX_PROTOCOL_WIRE_BYTES
        );
        assert_legacy_mac_input(&key, &challenge, &maximum);
        let over_limit = DaemonRequest::Act {
            protocol: "x".repeat(MAX_PROTOCOL_WIRE_BYTES + 1),
        };
        assert!(encode_daemon_request(&over_limit).is_err());
    }

    #[test]
    fn response_envelope_passes_through_exact_bounded_serde_json() {
        let responses = [
            DaemonResponse::Observation {
                event: "act".to_owned(),
                mode: "full".to_owned(),
                generation: "generation".to_owned(),
                base_generation: None,
                content: format!(
                    "{{\"quoted\":\"{}\",\"unicode\":\"\u{4e2d}\u{6587}\"}}",
                    "\\\"<&>".repeat(16_384)
                ),
            },
            DaemonResponse::Ok,
            DaemonResponse::Error {
                message: "sanitized error".to_owned(),
            },
        ];

        for response in responses {
            let expected_error = match &response {
                DaemonResponse::Error { message } => Some(message.clone()),
                DaemonResponse::Observation { .. } | DaemonResponse::Ok => None,
            };
            let expected = serde_json::to_vec(&response).unwrap();
            let (envelope, wire) = encode_daemon_response(&response).unwrap();
            assert_eq!(wire, expected);
            let envelope_bytes = serde_json::to_vec(&envelope).unwrap();
            let envelope =
                serde_json::from_slice::<DaemonResponseEnvelope>(&envelope_bytes).unwrap();
            let validated =
                validate_client_daemon_response(envelope.clone(), String::from_utf8(wire).unwrap())
                    .unwrap();
            assert_eq!(validated.envelope, envelope);
            assert_eq!(validated.wire.as_bytes(), expected);
            assert_eq!(validated.error_message, expected_error);
        }

        let large_error = DaemonResponse::Error {
            message: "x".repeat(MAX_DAEMON_AUTH_LINE_BYTES * 2),
        };
        let (large_error_envelope, large_error_wire) =
            encode_daemon_response(&large_error).unwrap();
        assert!(
            serde_json::to_vec(&large_error_envelope).unwrap().len() < MAX_DAEMON_AUTH_LINE_BYTES
        );
        assert_eq!(
            validate_client_daemon_response(
                large_error_envelope,
                String::from_utf8(large_error_wire).unwrap(),
            )
            .unwrap()
            .error_message
            .unwrap()
            .len(),
            MAX_DAEMON_AUTH_LINE_BYTES * 2
        );

        for invalid_content in [
            serde_json::Value::Null,
            serde_json::json!(17),
            serde_json::json!(true),
            serde_json::json!({"nested": "value"}),
            serde_json::json!(["value"]),
        ] {
            let wire = serde_json::json!({
                "kind": "observation",
                "event": "act",
                "mode": "full",
                "generation": "generation",
                "base_generation": null,
                "content": invalid_content,
            })
            .to_string();
            let fault = validate_client_daemon_response(
                DaemonResponseEnvelope::Observation {
                    response_bytes: wire.len(),
                },
                wire,
            )
            .unwrap_err();
            assert_eq!(
                fault.to_string(),
                "external daemon response was invalid",
                "non-string observation content was accepted"
            );
        }

        let canonical_observation = serde_json::to_string(&DaemonResponse::Observation {
            event: "act".to_owned(),
            mode: "full".to_owned(),
            generation: "generation".to_owned(),
            base_generation: None,
            content: "quoted-\"-\\-\n-\u{4e2d}".to_owned(),
        })
        .unwrap();
        let validated = validate_client_daemon_response(
            DaemonResponseEnvelope::Observation {
                response_bytes: canonical_observation.len(),
            },
            canonical_observation.clone(),
        )
        .unwrap();
        assert_eq!(validated.wire, canonical_observation);

        let reordered = concat!(
            r#"{"kind":"observation","content":"value","event":"act","mode":"full","#,
            r#""generation":"generation","base_generation":null}"#,
        )
        .to_owned();
        let extra = format!(
            "{},\"extra\":true}}",
            canonical_observation.strip_suffix('}').unwrap()
        );
        let malformed_escape = concat!(
            r#"{"kind":"observation","event":"act","mode":"full","#,
            r#""generation":"generation","base_generation":null,"content":"bad\q"}"#,
        )
        .to_owned();
        let escaped_metadata = concat!(
            r#"{"kind":"observation","event":"\u0061ct","mode":"full","#,
            r#""generation":"generation","base_generation":null,"content":"value"}"#,
        )
        .to_owned();
        let trailing_whitespace = format!("{canonical_observation} ");
        let whitespace_before_close =
            format!("{} }}", canonical_observation.strip_suffix('}').unwrap());
        for (label, wire) in [
            ("reordered fields", reordered),
            ("unknown field", extra),
            ("malformed content escape", malformed_escape),
            ("noncanonical metadata escape", escaped_metadata),
            ("trailing whitespace", trailing_whitespace),
            ("whitespace before closing brace", whitespace_before_close),
        ] {
            let fault = validate_client_daemon_response(
                DaemonResponseEnvelope::Observation {
                    response_bytes: wire.len(),
                },
                wire,
            )
            .unwrap_err();
            assert_eq!(
                fault.to_string(),
                "external daemon response was invalid",
                "{label} was accepted"
            );
        }

        let mismatched_wire = serde_json::to_string(&DaemonResponse::Ok).unwrap();
        let mismatched = validate_client_daemon_response(
            DaemonResponseEnvelope::Error {
                response_bytes: mismatched_wire.len(),
            },
            mismatched_wire,
        )
        .unwrap_err();
        assert_eq!(
            mismatched.to_string(),
            "external daemon response did not match its envelope"
        );

        let mut bounded = BoundedJsonBuffer::new(0, 8);
        assert!(bounded.write_all(b"12345678").is_ok());
        assert!(bounded.write_all(b"9").is_err());
        assert!(bounded.exceeded);
        assert_eq!(bounded.bytes, b"12345678");
        assert_eq!(MAX_DAEMON_RESPONSE_JSON_BYTES + 1, MAX_DAEMON_LINE_BYTES);
    }

    #[tokio::test]
    async fn provisional_response_commit_replace_and_failure_framing_is_strict() {
        fn push_json_line<T: Serialize>(wire: &mut Vec<u8>, value: &T) {
            serde_json::to_writer(&mut *wire, value).unwrap();
            wire.push(b'\n');
        }

        fn push_response(wire: &mut Vec<u8>, response: &EncodedDaemonResponse) {
            push_json_line(wire, &response.envelope);
            wire.extend_from_slice(&response.line);
            wire.push(b'\n');
        }

        fn provisional_wire(
            ticket: DaemonResponseTicket,
            response: &EncodedDaemonResponse,
        ) -> Vec<u8> {
            let mut wire = Vec::new();
            push_json_line(
                &mut wire,
                &DaemonProvisionalResponseEnvelope::Provisional {
                    ticket,
                    response: response.envelope.clone(),
                },
            );
            wire.extend_from_slice(&response.line);
            wire.push(b'\n');
            wire
        }

        async fn read_for(
            wire: &[u8],
            request: &DaemonRequest,
        ) -> anyhow::Result<ClientDaemonResponse> {
            let mut reader = BufReader::new(wire);
            read_client_daemon_response(&mut reader, request).await
        }

        async fn read(wire: &[u8]) -> anyhow::Result<ClientDaemonResponse> {
            read_for(
                wire,
                &DaemonRequest::Act {
                    protocol: String::new(),
                },
            )
            .await
        }

        let ticket = [0x41_u8; 32];
        let public = DaemonResponse::Observation {
            event: "act".to_owned(),
            mode: "full".to_owned(),
            generation: "generation-\"-\\-\u{4e2d}".to_owned(),
            base_generation: None,
            content: "private-until-commit-\"-\\-\n-\u{4e2d}".to_owned(),
        };
        let (envelope, line) = encode_daemon_response(&public).unwrap();
        let encoded = EncodedDaemonResponse { envelope, line };
        let mut committed_wire = provisional_wire(ticket, &encoded);
        push_json_line(
            &mut committed_wire,
            &DaemonResponseDisposition::Commit { ticket },
        );
        let committed = read(&committed_wire).await.unwrap();
        assert_eq!(committed.wire.as_bytes(), encoded.line);
        assert_eq!(
            committed.wire.as_bytes(),
            serde_json::to_vec(&public).unwrap()
        );
        assert!(committed.error_message.is_none());
        assert_eq!(
            committed.validated_kind,
            ValidatedDaemonResponseKind::Observation {
                mode: "full".to_owned()
            }
        );

        for request in [
            DaemonRequest::Observe {
                full_re_render: false,
            },
            DaemonRequest::Shutdown,
        ] {
            assert_eq!(
                read_for(&committed_wire, &request)
                    .await
                    .unwrap_err()
                    .to_string(),
                "external daemon provisional response was not valid for this request"
            );
        }
        let mut ordinary_wire = Vec::new();
        push_response(&mut ordinary_wire, &encoded);
        let ordinary = read_for(
            &ordinary_wire,
            &DaemonRequest::Observe {
                full_re_render: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(ordinary.wire.as_bytes(), encoded.line);

        for invalid_provisional in [
            DaemonResponse::Ok,
            DaemonResponse::Error {
                message: "must-not-be-provisional".to_owned(),
            },
        ] {
            let (envelope, line) = encode_daemon_response(&invalid_provisional).unwrap();
            let invalid_provisional = EncodedDaemonResponse { envelope, line };
            let mut invalid_wire = provisional_wire(ticket, &invalid_provisional);
            push_json_line(
                &mut invalid_wire,
                &DaemonResponseDisposition::Commit { ticket },
            );
            assert_eq!(
                read(&invalid_wire).await.unwrap_err().to_string(),
                "external daemon provisional response was not an Observation"
            );
        }

        let delta = DaemonResponse::Observation {
            event: "act".to_owned(),
            mode: "delta".to_owned(),
            generation: "delta-generation".to_owned(),
            base_generation: Some("delta-base".to_owned()),
            content: "must-not-be-provisional".to_owned(),
        };
        let (delta_envelope, delta_line) = encode_daemon_response(&delta).unwrap();
        let delta = EncodedDaemonResponse {
            envelope: delta_envelope,
            line: delta_line,
        };
        let mut delta_wire = provisional_wire(ticket, &delta);
        push_json_line(
            &mut delta_wire,
            &DaemonResponseDisposition::Commit { ticket },
        );
        assert_eq!(
            read(&delta_wire).await.unwrap_err().to_string(),
            "external daemon provisional response was not a Full observation"
        );

        let replacement_message = "replacement-error-\"-\\-\n-\u{4e2d}".repeat(512);
        assert!(replacement_message.len() > MAX_DAEMON_AUTH_LINE_BYTES);
        let replacement = DaemonResponse::Error {
            message: replacement_message.clone(),
        };
        let (replacement_envelope, replacement_line) =
            encode_daemon_response(&replacement).unwrap();
        let replacement = EncodedDaemonResponse {
            envelope: replacement_envelope,
            line: replacement_line,
        };
        let mut replaced_wire = provisional_wire(ticket, &encoded);
        push_json_line(
            &mut replaced_wire,
            &DaemonResponseDisposition::Replace { ticket },
        );
        push_response(&mut replaced_wire, &replacement);
        let replaced = read(&replaced_wire).await.unwrap();
        assert_eq!(replaced.wire.as_bytes(), replacement.line);
        assert_eq!(
            replaced.error_message.as_deref(),
            Some(replacement_message.as_str())
        );

        let mut wrong_ticket_wire = provisional_wire(ticket, &encoded);
        push_json_line(
            &mut wrong_ticket_wire,
            &DaemonResponseDisposition::Commit {
                ticket: [0x42_u8; 32],
            },
        );
        assert_eq!(
            read(&wrong_ticket_wire).await.unwrap_err().to_string(),
            "external daemon response disposition ticket did not match"
        );

        let missing_disposition = provisional_wire(ticket, &encoded);
        assert_eq!(
            read(&missing_disposition).await.unwrap_err().to_string(),
            "external daemon response disposition was not terminated"
        );

        let mut partial_body = Vec::new();
        push_json_line(
            &mut partial_body,
            &DaemonProvisionalResponseEnvelope::Provisional {
                ticket,
                response: encoded.envelope.clone(),
            },
        );
        partial_body.extend_from_slice(&encoded.line[..encoded.line.len() / 2]);
        assert!(read(&partial_body)
            .await
            .unwrap_err()
            .to_string()
            .contains("failed to read external daemon response"));

        let mut malformed_disposition = provisional_wire(ticket, &encoded);
        malformed_disposition.extend_from_slice(
            br#"{"disposition":"commit","ticket":[65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65,65],"extra":true}
"#,
        );
        assert_eq!(
            read(&malformed_disposition).await.unwrap_err().to_string(),
            "external daemon response disposition was invalid"
        );

        let oversized = DaemonProvisionalResponseEnvelope::Provisional {
            ticket,
            response: DaemonResponseEnvelope::Observation {
                response_bytes: MAX_DAEMON_RESPONSE_JSON_BYTES + 1,
            },
        };
        let mut oversized_wire = Vec::new();
        push_json_line(&mut oversized_wire, &oversized);
        assert_eq!(
            read(&oversized_wire).await.unwrap_err().to_string(),
            "external daemon response exceeded configured wire limit"
        );

        for line in [
            serde_json::to_vec(&DaemonProvisionalResponseEnvelope::Provisional {
                ticket,
                response: encoded.envelope.clone(),
            })
            .unwrap(),
            serde_json::to_vec(&DaemonResponseDisposition::Commit { ticket }).unwrap(),
            serde_json::to_vec(&DaemonResponseDisposition::Replace { ticket }).unwrap(),
        ] {
            assert!(line.len() < MAX_DAEMON_AUTH_LINE_BYTES);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn provisional_rotation_commits_only_after_operation_and_cleanup_succeed() {
        async fn run_rotation(
            mut current: ExternalCliSession,
        ) -> (DaemonResponseDisposition, DaemonResponse) {
            current.application.observe().await.unwrap();
            let old_signal = current.state_signal().unwrap().clone();
            let replacement = ExternalCliSession::mount(current.snapshot().unwrap()).unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let client = TcpStream::connect(address).await.unwrap();
            let (mut server, _) = listener.accept().await.unwrap();
            let protocol =
                "{\"type\":\"text_complete\",\"text\":\"provisional-lifecycle\"}\n".to_owned();
            let client_request = DaemonRequest::Act {
                protocol: protocol.clone(),
            };
            let reading = tokio::spawn(async move {
                let mut reader = BufReader::new(client);
                read_client_daemon_response(&mut reader, &client_request).await
            });

            let mut transition = rotate_external_act_with_provisional(
                current,
                replacement,
                protocol,
                Some(&mut server),
            )
            .await;
            assert_signal_is_fenced(&old_signal);
            let disposition = transition
                .provisional_disposition
                .expect("valid speculative Full must have a final disposition");
            let public_bytes = serde_json::to_vec(&transition.response).unwrap();
            let preencoded = transition.preencoded_response.take();
            write_external_cli_transition_response(
                &mut server,
                &transition.response,
                preencoded,
                Some(disposition),
            )
            .await
            .unwrap();
            let client = reading.await.unwrap().unwrap();
            assert_eq!(client.wire.as_bytes(), public_bytes);
            if let Some(session) = transition.session.take() {
                session.shutdown().await.unwrap();
            }
            (disposition, transition.response)
        }

        let (committed, committed_response) =
            run_rotation(ExternalCliSession::mount(ExternalCliState::default()).unwrap()).await;
        assert!(matches!(
            committed,
            DaemonResponseDisposition::Commit { .. }
        ));
        assert!(matches!(
            committed_response,
            DaemonResponse::Observation { ref mode, .. } if mode == "full"
        ));

        let (operation_replaced, operation_response) = run_rotation(
            ExternalCliSession::mount_erroring(
                ExternalCliState::default(),
                "injected-operation-error",
            )
            .unwrap(),
        )
        .await;
        assert!(matches!(
            operation_replaced,
            DaemonResponseDisposition::Replace { .. }
        ));
        assert!(matches!(operation_response, DaemonResponse::Error { .. }));

        let (cleanup_replaced, cleanup_response) = run_rotation(
            ExternalCliSession::mount_failing_shutdown(ExternalCliState::default()).unwrap(),
        )
        .await;
        assert!(matches!(
            cleanup_replaced,
            DaemonResponseDisposition::Replace { .. }
        ));
        assert!(matches!(
            cleanup_response,
            DaemonResponse::Error { ref message }
                if message == "external application cleanup task failed"
        ));

        let mut operation_and_cleanup = ExternalCliSession::mount_erroring(
            ExternalCliState::default(),
            "injected-operation-error",
        )
        .unwrap();
        operation_and_cleanup.shutdown_fault = true;
        let (both_replaced, both_response) = run_rotation(operation_and_cleanup).await;
        assert!(matches!(
            both_replaced,
            DaemonResponseDisposition::Replace { .. }
        ));
        assert_eq!(both_response, operation_response);
    }

    #[test]
    fn canonical_json_content_meter_matches_fresh_structured_pom_items() {
        fn assert_matches_full_item(value: &str) {
            let expected = if value.is_empty() {
                0
            } else {
                canonical_external_state_item_bytes(value)
                    .unwrap()
                    .checked_sub(external_state_nonempty_envelope_bytes())
                    .unwrap()
            };
            assert_eq!(
                canonical_external_state_content_bytes(value).unwrap(),
                expected,
                "content meter drifted for {value:?}"
            );
        }

        fn assert_matches_prefixed_item(kind: ExternalCliTextKind, value: &str) {
            let rendered = format!("{}{value}", kind.prefix());
            let expected = canonical_external_state_content_bytes(&rendered).unwrap();
            let actual = external_cli_text_prefix_content_bytes(kind)
                + canonical_external_state_content_bytes(value).unwrap();
            assert_eq!(actual, expected, "prefixed meter drifted for {rendered:?}");
        }

        for value in [
            "",
            "plain ASCII 0123456789",
            "quotes-\"\"\"",
            "backslashes-\\\\\\",
            "controls-\t-\n-\r",
            "xml-<tag attr='value'>&\"text\"</tag>",
            "unicode-\u{4e2d}\u{6587}-\u{1f642}-\u{e000}-\u{2028}-\u{2029}",
        ] {
            assert_matches_full_item(value);
            assert_matches_prefixed_item(ExternalCliTextKind::Delta, value);
            assert_matches_prefixed_item(ExternalCliTextKind::Complete, value);
        }

        let tokens = [
            "a",
            "Z",
            "0",
            "\"",
            "\\",
            "\t",
            "\n",
            "\r",
            "<",
            ">",
            "&",
            "'",
            "\u{4e2d}",
            "\u{1f642}",
            "\u{e000}",
        ];
        let mut seed = 0x8a5c_d789_635d_2dff_u64;
        for sample in 0..500 {
            let mut value = format!("unique-{sample:03}:");
            let token_count = 1 + sample % 31;
            for _ in 0..token_count {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                value.push_str(tokens[(seed as usize) % tokens.len()]);
            }
            assert_matches_full_item(&value);
            let kind = if sample % 2 == 0 {
                ExternalCliTextKind::Delta
            } else {
                ExternalCliTextKind::Complete
            };
            assert_matches_prefixed_item(kind, &value);
        }

        assert!(canonical_external_state_content_bytes("invalid-\0").is_err());
    }

    #[test]
    fn canonical_snapshot_meter_bounds_escapable_text_and_preserves_utf8_suffixes() {
        assert_eq!(max_external_cli_snapshot_item_bytes(), 8_388_276);

        let escapable = "\"".repeat(EXTERNAL_CLI_MAX_TEXT_BYTES);
        let complete = format!("complete:{escapable}");
        let mut state = ExternalCliState::default();
        state.push(format!("delta:{escapable}")).unwrap();
        state.push(complete.clone()).unwrap();

        let rendered = state.rendered();
        assert!(state.rendered_bytes <= MAX_EXTERNAL_CLI_STATE_BYTES);
        assert!(state.canonical_item_bytes() <= max_external_cli_snapshot_item_bytes());
        assert_eq!(
            state.canonical_item_bytes(),
            canonical_external_state_item_bytes(&rendered).unwrap()
        );
        assert!(complete.ends_with(&rendered));
        let omitted = complete.len() - rendered.len();
        assert!(omitted > 0);
        assert!(
            canonical_external_state_item_bytes(&complete[omitted - 1..]).unwrap()
                > max_external_cli_snapshot_item_bytes()
        );
        ensure_xml_renderable_text(&rendered).unwrap();

        let mut worst_xml = ExternalCliState::default();
        let worst_xml_event = format!("complete:{}", "&".repeat(EXTERNAL_CLI_MAX_TEXT_BYTES));
        worst_xml.push(worst_xml_event.clone()).unwrap();
        let worst_xml_rendered = worst_xml.rendered();
        assert!(worst_xml_event.ends_with(&worst_xml_rendered));
        assert_eq!(
            worst_xml.canonical_item_bytes(),
            canonical_external_state_item_bytes(&worst_xml_rendered).unwrap()
        );
        assert!(worst_xml.canonical_item_bytes() <= max_external_cli_snapshot_item_bytes());

        let mut scaled = ExternalCliState::default();
        scaled
            .push_with_limits("oldest".to_owned(), 18, usize::MAX)
            .unwrap();
        scaled
            .push_with_limits("middle".to_owned(), 18, usize::MAX)
            .unwrap();
        scaled
            .push_with_limits("unicode-\u{4e2d}\u{6587}".to_owned(), 18, usize::MAX)
            .unwrap();
        assert_eq!(scaled.rendered(), "unicode-\u{4e2d}\u{6587}");
        assert!(scaled.rendered_bytes <= 18);
        assert_eq!(
            scaled.canonical_item_bytes(),
            canonical_external_state_item_bytes(&scaled.rendered()).unwrap()
        );
        assert_eq!(
            bounded_suffix("prefix-\u{4e2d}\u{6587}".to_owned(), 5),
            "\u{6587}"
        );
    }

    #[test]
    fn cached_canonical_contributions_evict_whole_events_without_remetering() {
        let (mut state, diagnostics) = ExternalCliState::with_diagnostics();
        let event = "\"".repeat(40);
        let canonical_cap = canonical_external_state_item_bytes(&"\"".repeat(120)).unwrap();

        for _ in 0..1_000 {
            state
                .push_with_limits(event.clone(), usize::MAX, canonical_cap)
                .unwrap();
        }

        let work = diagnostics.snapshot();
        assert_eq!(work.entry_canonicalizations, 1_000);
        assert_eq!(work.entry_canonical_input_bytes, 40_000);
        assert_eq!(work.content_serializations, 1_000);
        assert_eq!(work.full_snapshot_renders, 0);
        assert!(state.events.len() < 10);
        assert!(state.canonical_item_bytes() <= canonical_cap);

        let before_invalid = state.clone();
        assert!(state
            .push_with_limits("invalid-\0".to_owned(), usize::MAX, canonical_cap)
            .is_err());
        assert_eq!(state, before_invalid);
        assert_eq!(
            state.canonical_item_bytes(),
            canonical_external_state_item_bytes(&state.rendered()).unwrap()
        );
    }

    async fn fragmented_work(frames: usize, bytes_per_frame: usize) -> ExternalCliWorkSnapshot {
        let (state, diagnostics) = ExternalCliState::with_diagnostics();
        let mut current = ExternalCliSession::mount(state).unwrap();
        current.application.observe().await.unwrap();
        let replacement = ExternalCliSession::mount(current.snapshot().unwrap()).unwrap();
        let chunk = "\"".repeat(bytes_per_frame);
        let protocol = encode_text_delta(&chunk).unwrap().repeat(frames);
        assert_eq!(frames * bytes_per_frame, 400_000);
        diagnostics.reset();

        let transition = rotate_external_act(current, replacement, protocol).await;
        let session = transition
            .session
            .expect("fragmented act keeps replacement owner");
        assert!(matches!(
            transition.response,
            DaemonResponse::Observation { ref mode, .. } if mode == "full"
        ));
        let snapshot = session.snapshot().unwrap();
        assert_eq!(
            snapshot.canonical_item_bytes(),
            canonical_external_state_item_bytes(&snapshot.rendered()).unwrap()
        );
        let work = diagnostics.snapshot();
        session.shutdown().await.unwrap();
        work
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fragmented_state_work_is_linear_in_entry_bytes_and_hides_snapshot_once() {
        let ten = fragmented_work(10, 40_000).await;
        let hundred = fragmented_work(100, 4_000).await;

        assert_eq!(ten.entry_canonicalizations, 11);
        assert_eq!(hundred.entry_canonicalizations, 101);
        assert_eq!(ten.entry_canonical_input_bytes, 800_069);
        assert_eq!(hundred.entry_canonical_input_bytes, 800_609);
        assert_eq!(ten.content_serializations, 2);
        assert_eq!(hundred.content_serializations, 2);
        assert_eq!(ten.visibility_signal_updates, 1);
        assert_eq!(hundred.visibility_signal_updates, 1);
        assert_eq!(ten.full_snapshot_renders, hundred.full_snapshot_renders);
        assert!(ten.full_snapshot_renders <= 2, "{ten:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_rotations_preserve_order_targets_continuity_and_consume_old_owners() {
        let mut session = ExternalCliSession::mount(ExternalCliState::default()).unwrap();
        let first = session.application.observe().await.unwrap();
        let mut current_generation = format!("{:?}", first.generation());
        let mut targets =
            std::collections::HashSet::from([format!("{:?}", first.frame().target())]);
        let mut expected = Vec::new();
        let mut old_signals = Vec::new();

        for index in 0..64 {
            let text = match index % 3 {
                0 => format!("plain-{index:02}"),
                1 => format!("xml-<>&\"'\\-\t-\n-\r-{index:02}"),
                _ => format!("unicode-\u{4e2d}\u{6587}-\u{1f642}-{index:02}"),
            };
            let old_signal = session.state_signal().unwrap().clone();
            let replacement = ExternalCliSession::mount(session.snapshot().unwrap()).unwrap();
            let transition =
                rotate_external_act(session, replacement, encode_text_delta(&text).unwrap()).await;
            session = transition
                .session
                .expect("successful rotation retains owner");
            assert_signal_is_fenced(&old_signal);
            old_signals.push(old_signal);

            let (full_generation, content) = match transition.response {
                DaemonResponse::Observation {
                    mode,
                    generation,
                    base_generation,
                    content,
                    ..
                } => {
                    assert_eq!(mode, "full");
                    assert!(base_generation.is_none());
                    (generation, content)
                }
                response => panic!("rotation did not return Full: {response:?}"),
            };
            let old_target = generation_target(&current_generation);
            let new_target = generation_target(&full_generation);
            assert_ne!(new_target, old_target);
            assert!(targets.insert(format!("TargetIdentity({new_target})")));
            let frame: serde_json::Value = serde_json::from_str(&content).unwrap();
            assert_eq!(frame["replay"], serde_json::json!([]));

            expected.push(format!("delta:{text}"));
            expected.push(format!("complete:{text}"));
            let snapshot = session.snapshot().unwrap();
            assert_eq!(
                snapshot
                    .events
                    .iter()
                    .map(|event| event.rendered.rendered())
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(snapshot.rendered_bytes <= MAX_EXTERNAL_CLI_STATE_BYTES);
            assert!(snapshot.canonical_item_bytes() <= max_external_cli_snapshot_item_bytes());
            assert_eq!(
                snapshot.canonical_item_bytes(),
                canonical_external_state_item_bytes(&snapshot.rendered()).unwrap()
            );

            let continuity = observe_external(&mut session, false).await;
            current_generation = match continuity {
                DaemonResponse::Observation {
                    mode,
                    generation,
                    base_generation,
                    ..
                } => {
                    assert_eq!(mode, "delta");
                    assert_eq!(base_generation.as_deref(), Some(full_generation.as_str()));
                    assert_eq!(
                        generation_target(&generation),
                        generation_target(&full_generation)
                    );
                    generation
                }
                response => panic!("new target lost continuity: {response:?}"),
            };
        }

        let final_signal = session.state_signal().unwrap().clone();
        session.shutdown().await.unwrap();
        assert_signal_is_fenced(&final_signal);
        for signal in &old_signals {
            assert_signal_is_fenced(signal);
        }
        assert_eq!(targets.len(), 65);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_and_over_limit_acts_rotate_without_acknowledgement_or_state_loss() {
        let mut session = ExternalCliSession::mount(ExternalCliState::default()).unwrap();
        session.application.observe().await.unwrap();
        let old_signal = session.state_signal().unwrap().clone();
        let replacement = ExternalCliSession::mount(session.snapshot().unwrap()).unwrap();
        let protocol = concat!(
            r#"{"type":"text_delta","text":"partial"}"#,
            "\n",
            r#"{"type":"disconnect"}"#,
            "\n",
        );
        let transition = rotate_external_act(session, replacement, protocol.to_owned()).await;
        let mut session = transition.session.expect("failed act still rotates owner");
        assert_signal_is_fenced(&old_signal);
        assert!(matches!(
            transition.response,
            DaemonResponse::Error { ref message }
                if message == "external text protocol disconnected abnormally"
        ));
        assert!(session.pending_observation.is_some());

        let recovered = observe_external(&mut session, false).await;
        let recovered_generation = match recovered {
            DaemonResponse::Observation {
                mode,
                generation,
                base_generation,
                content,
                ..
            } => {
                assert_eq!(mode, "full");
                assert!(base_generation.is_none());
                assert!(content.contains("delta:partial"));
                generation
            }
            response => panic!("failed act did not expose recovery Full: {response:?}"),
        };
        let snapshot_before_limit = session.snapshot().unwrap();
        let over_limit = encode_text_delta(&"x".repeat(EXTERNAL_CLI_MAX_TEXT_BYTES + 1)).unwrap();
        let transition = act_external(session, over_limit).await;
        let mut session = transition
            .session
            .expect("over-limit act retains replacement");
        assert!(matches!(transition.response, DaemonResponse::Error { .. }));
        assert_eq!(session.snapshot().unwrap(), snapshot_before_limit);

        let recovered_again = observe_external(&mut session, false).await;
        let recovered_again_generation = match recovered_again {
            DaemonResponse::Observation {
                mode,
                generation,
                base_generation,
                ..
            } => {
                assert_eq!(mode, "full");
                assert!(base_generation.is_none());
                generation
            }
            response => panic!("over-limit act did not leave a recovery Full: {response:?}"),
        };
        assert_ne!(
            generation_target(&recovered_generation),
            generation_target(&recovered_again_generation)
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn operation_panic_survives_both_owner_cleanups_and_error_precedence_is_stable() {
        let mut current = ExternalCliSession::mount_panicking(
            ExternalCliState::default(),
            "exact-external-operation-panic",
        )
        .unwrap();
        current.application.observe().await.unwrap();
        let current_signal = current.state_signal().unwrap().clone();
        let replacement = ExternalCliSession::mount(ExternalCliState::default()).unwrap();
        let replacement_signal = replacement.state_signal().unwrap().clone();

        let panic = match AssertUnwindSafe(rotate_external_act(
            current,
            replacement,
            encode_text_delta("panic").unwrap(),
        ))
        .catch_unwind()
        .await
        {
            Err(panic) => panic,
            Ok(_) => panic!("operation panic must resume after cleanup"),
        };
        assert_eq!(
            panic.downcast_ref::<String>().map(String::as_str),
            Some("exact-external-operation-panic")
        );
        assert_signal_is_fenced(&current_signal);
        assert_signal_is_fenced(&replacement_signal);

        let selected = select_rotation_error(
            Some(DaemonResponse::Error {
                message: "operation".to_owned(),
            }),
            Some(DaemonResponse::Error {
                message: "cleanup".to_owned(),
            }),
        );
        assert!(matches!(
            selected,
            Some(DaemonResponse::Error { ref message }) if message == "operation"
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn immediately_ready_operation_panic_cancels_unpolled_speculation() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker = tokio::spawn(std::future::pending::<()>());
        let mut cancellation = ExternalCliSpeculationCancellation::new(
            Arc::clone(&cancelled),
            worker.abort_handle(),
            None,
            None,
        );
        let speculation_polled = Arc::new(AtomicBool::new(false));
        let polled = Arc::clone(&speculation_polled);
        let speculation = std::future::poll_fn(move |_| {
            polled.store(true, Ordering::Release);
            std::task::Poll::<()>::Pending
        });
        let payload: Box<dyn std::any::Any + Send> =
            Box::new(String::from("exact-unpolled-speculation-panic"));
        let operation = std::future::ready(Err::<(), _>(payload));

        let payload =
            coordinate_external_cli_speculation(operation, speculation, &mut cancellation)
                .await
                .expect_err("ready operation panic must win");
        assert_eq!(
            payload.downcast_ref::<String>().map(String::as_str),
            Some("exact-unpolled-speculation-panic")
        );
        assert!(cancelled.load(Ordering::Acquire));
        assert!(!speculation_polled.load(Ordering::Acquire));
        let worker_fault = worker
            .await
            .expect_err("cancelled worker must not complete");
        assert!(worker_fault.is_cancelled());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn operation_panic_cancels_incomplete_speculation_before_resuming_payload() {
        let barrier = Arc::new(ExternalCliSpeculationBarrier::default());
        let mut current = ExternalCliSession::mount_panicking_after_speculation_started(
            ExternalCliState::default(),
            "exact-cancelled-speculation-panic",
            Arc::clone(&barrier),
        )
        .unwrap();
        current.application.observe().await.unwrap();
        let current_signal = current.state_signal().unwrap().clone();
        let replacement = ExternalCliSession::mount(ExternalCliState::default()).unwrap();
        let replacement_signal = replacement.state_signal().unwrap().clone();

        let mut rotation = tokio::spawn(async move {
            AssertUnwindSafe(rotate_external_act(
                current,
                replacement,
                encode_text_delta("panic-before-speculation").unwrap(),
            ))
            .catch_unwind()
            .await
        });
        let result = match tokio::time::timeout(Duration::from_secs(2), &mut rotation).await {
            Ok(result) => result.expect("rotation task must catch its operation panic"),
            Err(_) => {
                barrier.cancel();
                let _ = tokio::time::timeout(Duration::from_secs(2), rotation).await;
                panic!("operation panic waited for incomplete speculation");
            }
        };
        let payload = match result {
            Err(payload) => payload,
            Ok(_) => panic!("operation panic must resume after cleanup"),
        };
        assert_eq!(
            payload.downcast_ref::<String>().map(String::as_str),
            Some("exact-cancelled-speculation-panic")
        );
        assert_signal_is_fenced(&current_signal);
        assert_signal_is_fenced(&replacement_signal);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if barrier.snapshot().finished {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled speculative worker must finish");
        let state = barrier.snapshot();
        assert!(state.entered);
        assert!(state.cancelled);
        assert!(state.finished);
        assert!(!state.released);
    }

    struct DaemonShutdownTaskDrop {
        gate: std::sync::Arc<DaemonShutdownDropGate>,
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl Drop for DaemonShutdownTaskDrop {
        fn drop(&mut self) {
            self.gate
                .entered
                .store(true, std::sync::atomic::Ordering::Release);
            let mut released = self.gate.released.lock().unwrap();
            while !*released {
                released = self.gate.changed.wait(released).unwrap();
            }
            self.dropped
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    struct DaemonShutdownDropGate {
        entered: std::sync::atomic::AtomicBool,
        released: std::sync::Mutex<bool>,
        changed: std::sync::Condvar,
    }

    impl DaemonShutdownDropGate {
        fn new() -> Self {
            Self {
                entered: std::sync::atomic::AtomicBool::new(false),
                released: std::sync::Mutex::new(false),
                changed: std::sync::Condvar::new(),
            }
        }

        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    #[derive(Clone)]
    struct DaemonShutdownProps {
        exported: std::sync::Arc<std::sync::Mutex<Option<Signal<bool>>>>,
        started: std::sync::Arc<std::sync::atomic::AtomicBool>,
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
        gate: std::sync::Arc<DaemonShutdownDropGate>,
    }

    #[component]
    fn daemon_shutdown_probe(props: DaemonShutdownProps) -> Component {
        let visible = use_signal(|| true);
        *props.exported.lock().unwrap() = Some(visible);
        let started = std::sync::Arc::clone(&props.started);
        let dropped = std::sync::Arc::clone(&props.dropped);
        let gate = std::sync::Arc::clone(&props.gate);
        use_future(move || async move {
            let _drop = DaemonShutdownTaskDrop { gate, dropped };
            started.store(true, std::sync::atomic::Ordering::Release);
            std::future::pending::<()>().await;
        });
        view! { daemon_shutdown_probe { "mounted" } }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_response_waits_for_active_reaction_and_mount_task_cleanup() {
        let exported = std::sync::Arc::new(std::sync::Mutex::new(None));
        let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate = std::sync::Arc::new(DaemonShutdownDropGate::new());
        let props = DaemonShutdownProps {
            exported: std::sync::Arc::clone(&exported),
            started: std::sync::Arc::clone(&started),
            dropped: std::sync::Arc::clone(&dropped),
            gate: std::sync::Arc::clone(&gate),
        };
        let mut external =
            ExternalApplication::new_root(move || daemon_shutdown_probe(props.clone())).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.load(std::sync::atomic::Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("daemon mount task must start");
        external.observe().await.unwrap();
        let visible = exported.lock().unwrap().clone().unwrap();
        let mut state = DaemonState {
            external: Some(ExternalCliSession::from_application(external)),
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(address).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let connection = AuthenticatedDaemonConnection {
            stream: server,
            request: DaemonRequest::Shutdown,
        };
        let mut handling = Box::pin(handle_authenticated_connection(&mut state, connection));

        tokio::select! {
            result = &mut handling => panic!("shutdown acknowledged before task cleanup: {result:?}"),
            entered = tokio::time::timeout(Duration::from_secs(1), async {
                while !gate.entered.load(std::sync::atomic::Ordering::Acquire) {
                    tokio::task::yield_now().await;
                }
            }) => entered.expect("mount task destructor must run"),
        }
        assert!(matches!(visible.set(false), Err(SignalAccessError::Stale)));
        assert!(!dropped.load(std::sync::atomic::Ordering::Acquire));
        assert!(
            tokio::time::timeout(Duration::from_millis(25), client.read_u8())
                .await
                .is_err()
        );

        gate.release();
        assert!(handling.await.unwrap());
        assert!(dropped.load(std::sync::atomic::Ordering::Acquire));

        let mut response = String::new();
        BufReader::new(client)
            .read_line(&mut response)
            .await
            .unwrap();
        assert!(matches!(
            serde_json::from_str::<DaemonResponse>(&response).unwrap(),
            DaemonResponse::Ok
        ));
    }

    #[test]
    fn daemon_probe_accepts_only_exact_llvm_environment_names() {
        assert!(daemon_probe_environment_name_is_allowed(
            std::ffi::OsStr::new(LLVM_PROFILE_FILE_ENV)
        ));
        assert!(daemon_probe_environment_name_is_allowed(
            std::ffi::OsStr::new(LLVM_PROFILE_RUNTIME_ENV)
        ));
        assert!(!daemon_probe_environment_name_is_allowed(
            std::ffi::OsStr::new("LLVM_PROFILE_FILE_LOOKALIKE")
        ));
        assert!(!daemon_probe_environment_name_is_allowed(
            std::ffi::OsStr::new("__LLVM_PROFILE_RT_INIT_ONCE_LOOKALIKE")
        ));
    }

    #[test]
    fn spawned_daemon_command_clears_ambient_environment_and_keeps_launch_allowlist() {
        let mut command = Command::new(env::current_exe().expect("current test executable"));
        command.env(DAEMON_ENV_SENTINEL, "must-not-reach-daemon");
        configure_daemon_environment(
            &mut command,
            DAEMON_ENV_PROBE_ADDR.parse().expect("probe address"),
            DAEMON_ENV_PROBE_TOKEN,
        );
        command
            .env(DAEMON_ENV_PROBE_ENV, "1")
            .arg("--exact")
            .arg("tests::daemon_environment_probe")
            .arg("--nocapture");

        let output = command.output().expect("run daemon environment probe");
        assert!(
            output.status.success(),
            "daemon environment probe failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn spawned_daemon_command_preserves_explicit_llvm_profile_file() {
        let mut command = Command::new(env::current_exe().expect("current test executable"));
        command.env(LLVM_PROFILE_FILE_ENV, EXPLICIT_DAEMON_LLVM_PROFILE_FILE);

        configure_daemon_environment(
            &mut command,
            DAEMON_ENV_PROBE_ADDR.parse().expect("probe address"),
            DAEMON_ENV_PROBE_TOKEN,
        );

        let configured_profile_file = command.get_envs().find_map(|(name, value)| {
            (name == std::ffi::OsStr::new(LLVM_PROFILE_FILE_ENV))
                .then_some(value)
                .flatten()
        });
        assert_eq!(
            configured_profile_file,
            Some(std::ffi::OsStr::new(EXPLICIT_DAEMON_LLVM_PROFILE_FILE))
        );
    }

    #[test]
    fn spawned_daemon_command_defaults_llvm_profile_file_to_target_directory() {
        let mut command = Command::new(env::current_exe().expect("current test executable"));
        command.env_remove(LLVM_PROFILE_FILE_ENV);

        configure_daemon_environment(
            &mut command,
            DAEMON_ENV_PROBE_ADDR.parse().expect("probe address"),
            DAEMON_ENV_PROBE_TOKEN,
        );

        let configured_profile_file = command.get_envs().find_map(|(name, value)| {
            (name == std::ffi::OsStr::new(LLVM_PROFILE_FILE_ENV))
                .then_some(value)
                .flatten()
        });
        assert_eq!(
            configured_profile_file,
            Some(std::ffi::OsStr::new(DEFAULT_DAEMON_LLVM_PROFILE_FILE))
        );
    }

    #[test]
    fn daemon_environment_probe() {
        if env::var_os(DAEMON_ENV_PROBE_ENV).is_none() {
            return;
        }

        assert!(
            env::var_os(DAEMON_ENV_SENTINEL).is_none(),
            "daemon inherited the ambient sentinel"
        );
        assert!(
            env::var_os(ADDR_ENV).as_deref() == Some(std::ffi::OsStr::new(DAEMON_ENV_PROBE_ADDR)),
            "daemon address was not preserved"
        );
        assert!(
            env::var_os(TOKEN_ENV).as_deref() == Some(std::ffi::OsStr::new(DAEMON_ENV_PROBE_TOKEN)),
            "daemon token was not preserved"
        );

        let unexpected = env::vars_os()
            .map(|(name, _)| name)
            .filter(|name| !daemon_probe_environment_name_is_allowed(name))
            .map(|name| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            unexpected.is_empty(),
            "daemon inherited unexpected environment names: {unexpected:?}"
        );
    }

    fn daemon_probe_environment_name_is_allowed(name: &std::ffi::OsStr) -> bool {
        let name = name.to_string_lossy();
        [ADDR_ENV, TOKEN_ENV, DAEMON_ENV_PROBE_ENV]
            .iter()
            .any(|allowed| environment_name_matches(&name, allowed))
            || environment_name_matches(&name, LLVM_PROFILE_FILE_ENV)
            || environment_name_matches(&name, LLVM_PROFILE_RUNTIME_ENV)
            || (cfg!(windows)
                && ["SystemRoot", "WINDIR"]
                    .iter()
                    .any(|allowed| environment_name_matches(&name, allowed)))
    }

    fn environment_name_matches(actual: &str, expected: &str) -> bool {
        if cfg!(windows) {
            actual.eq_ignore_ascii_case(expected)
        } else {
            actual == expected
        }
    }

    #[test]
    fn help_describes_external_protocol_without_internal_mode() {
        let help = help_text();

        assert!(help.contains("--full-re-render"));
        assert!(help.contains("--protocol"));
        assert!(help.contains("text_delta"));
        assert!(help.contains("text_complete"));
        assert!(help.contains("disconnect"));
        assert!(help.contains(TOKEN_ENV));
        assert!(!help.to_ascii_lowercase().contains("daemon"));
        assert!(!help.contains("__agentview"));
    }

    #[test]
    fn parses_public_commands() {
        assert_eq!(parse_cli(Vec::<String>::new()).unwrap(), CliCommand::Help);
        assert_eq!(
            parse_cli(["observe".to_owned()]).unwrap(),
            CliCommand::Observe {
                full_re_render: false
            }
        );
        assert_eq!(
            parse_cli(["observe".to_owned(), "--full-re-render".to_owned()]).unwrap(),
            CliCommand::Observe {
                full_re_render: true
            }
        );
        assert_eq!(
            parse_cli(["act".to_owned(), "--protocol".to_owned()]).unwrap(),
            CliCommand::ActProtocol
        );
        assert_eq!(
            parse_cli(["act".to_owned(), "world".to_owned()]).unwrap(),
            CliCommand::ActText {
                text: "world".to_owned()
            }
        );
    }

    #[test]
    fn parses_internal_commands_without_listing_them() {
        assert_eq!(
            parse_cli([INTERNAL_DAEMON_ARG.to_owned()]).unwrap(),
            CliCommand::InternalDaemon
        );
        assert_eq!(
            parse_cli([INTERNAL_SHUTDOWN_ARG.to_owned()]).unwrap(),
            CliCommand::InternalShutdown
        );
    }
}
