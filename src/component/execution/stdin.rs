//! A retained application for stdin/stdout frontends, without a model provider.

use std::{future::Future, ops::ControlFlow, time::Duration};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use tokio::task::{JoinError, JoinHandle};

use crate::{
    component::authoring::{ApplicationExitHandle, Component, ExitReason},
    pom_renderer::{render_pom_document, PomRenderError},
    transcript::CanonicalInputItem,
};

use super::{
    Application, ApplicationFault, CommandCall, CommandInput, CommandInputError, CommandOutcome,
    CommandSender, ExternalProviderPort, ReactionPortFault, RenderedProjection,
};

#[cfg(unix)]
mod cli;
#[cfg(unix)]
mod daemon;
#[cfg(unix)]
mod daemon_history;
#[cfg(unix)]
mod delivery;
#[cfg(unix)]
mod history;
#[cfg(unix)]
mod input;

#[cfg(unix)]
use delivery::StdinDelivery;

/// A callback result alongside its view. The programmatic application methods
/// return complete snapshots; the standard CLI lowers reader-specific Frames.
///
/// `ok` says the action was dispatched successfully, not whether the business
/// operation was accepted. Business outcomes belong to the callback's `result`.
/// An observation has no result and preserves the view's previous feedback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StdinResponse {
    pub ok: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_result"
    )]
    pub result: Option<Value>,
    pub view: String,
}

/// The application declares complete state; each frontend lowers its own Frame.
#[derive(Debug)]
struct StdinProjectionResponse {
    ok: bool,
    result: Option<Value>,
    projection: RenderedProjection,
}

impl StdinProjectionResponse {
    fn into_response(self) -> Result<StdinResponse, StdinApplicationError> {
        Ok(StdinResponse {
            ok: self.ok,
            result: self.result,
            view: render(&self.projection)?,
        })
    }
}

// A callback may legitimately return JSON null. An absent field denotes an
// observation; a present null is still a result that stdout must deliver.
fn deserialize_result<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[derive(Debug, thiserror::Error)]
pub enum StdinApplicationError {
    #[error("stdin applications require a running Tokio runtime")]
    NoRuntime,
    #[error("declare use_wait_for_command() in the root Component")]
    MissingWait,
    #[error(transparent)]
    Application(#[from] ApplicationFault),
    #[error(transparent)]
    Port(#[from] ReactionPortFault),
    #[error(transparent)]
    Input(#[from] CommandInputError),
    #[error(transparent)]
    Render(#[from] PomRenderError),
    #[error("stdin views cannot contain provider tool or extension items")]
    UnsupportedViewItem,
    #[error("stdin application driver was cancelled")]
    DriverCancelled,
    #[error("{operation}; application cleanup also failed: {cleanup}")]
    Cleanup {
        operation: Box<StdinApplicationError>,
        cleanup: ApplicationFault,
    },
}

/// Process frontend delivery policy. Preparation and callbacks remain serialized
/// by the retained application's ordinary command queue.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdinApplicationOptions {
    /// Observe prepared application state while stdin is idle. The first view is
    /// always delivered immediately; unchanged idle observations produce no
    /// stdout. A positive interval is required when this option is enabled.
    ///
    /// Refreshing does not invoke an Action or clear its feedback. Applications
    /// that read external state should do so in their ordinary preparation hook.
    pub idle_refresh_interval: Option<Duration>,
}

/// Mount a Component tree whose root explicitly waits for command input.
///
/// [`Self::run`] owns the complete CLI, stdin/stdout and daemon lifecycle.
/// Custom transports can instead use [`Self::mount`] to drive preparation and
/// render responses automatically. Component props need no inbox or provider.
/// Call [`Self::shutdown`] before acknowledging daemon shutdown. Dropping this
/// handle requests cleanup; cancelling the shutdown waiter does not cancel it.
pub struct StdinApplication {
    sender: CommandSender,
    exit: ApplicationExitHandle,
    driver: Option<JoinHandle<Result<(), StdinApplicationError>>>,
}

impl StdinApplication {
    /// Run the process frontend, including stdin/stdout and a persistent daemon.
    ///
    /// Call this from a Tokio `main` and return its exit code. The CLI accepts
    /// `start`, `status`, `stop`, `restart`, `help`, and an optional `--socket PATH`.
    /// Without a subcommand it immediately shows the prepared initial view, then
    /// collects JSON action objects into ordered frames.
    /// A final newline followed by 100 ms without input submits a frame;
    /// EOF immediately submits the remaining frame, even without a newline.
    /// Objects may span lines. Connecting starts a missing daemon as needed;
    /// each action frame returns one final view with every result.
    /// The first response is complete; later responses on the same frontend
    /// connection use native Frame diff to omit unchanged state and schemas.
    /// A new frontend connection starts with its own complete view.
    /// A long-lived caller sends a complete batch, waits for its view, then
    /// sends the next batch. Tool or OS write calls are not framing boundaries.
    /// Empty stdin receives only the initial view, with no duplicate at EOF.
    /// Use [`Self::run_with_options`] to observe external state while input is idle.
    /// EOF leaves the daemon and its application state running.
    /// Socket transfers time out after ten seconds. Queue and callback execution
    /// may take up to five minutes before the daemon starts its response.
    /// Lifecycle requests wait behind accepted actions; an unfinished async
    /// action can make `stop` or `restart` time out without stopping the daemon.
    ///
    /// The root must declare `use_wait_for_command()`. Only the daemon mounts it;
    /// frontend invocations connect to that retained application. The default
    /// Unix socket is scoped by executable name under `$XDG_RUNTIME_DIR` or
    /// `$HOME/.cache`. Other platforms return an unsupported-platform diagnostic.
    pub async fn run(
        root: impl Fn() -> Component + Send + Sync + 'static,
    ) -> std::process::ExitCode {
        Self::run_with_options(root, StdinApplicationOptions::default()).await
    }

    /// Run the standard process frontend with an optional idle observation
    /// interval. A refresh waits for accepted action batches to finish, preserves
    /// buffered stdin and command feedback, and uses the same native Frame cursor
    /// as action responses. No request is retried after submission.
    pub async fn run_with_options(
        root: impl Fn() -> Component + Send + Sync + 'static,
        options: StdinApplicationOptions,
    ) -> std::process::ExitCode {
        #[cfg(unix)]
        {
            cli::run(root, options).await
        }
        #[cfg(not(unix))]
        {
            let _ = (root, options);
            eprintln!("agentview: the stdin daemon runner requires Unix domain sockets on a Unix platform");
            std::process::ExitCode::FAILURE
        }
    }

    pub fn mount(
        root: impl Fn() -> Component + Send + Sync + 'static,
    ) -> Result<Self, StdinApplicationError> {
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| StdinApplicationError::NoRuntime)?;
        let (sender, input) = CommandInput::channel(16);
        let (port, _control) = ExternalProviderPort::new()?;
        let application = Application::mount_with_commands(root, port, input)?;
        if !application.has_command_wait() {
            return Err(StdinApplicationError::MissingWait);
        }
        let exit = application.exit_handle();
        let driver = runtime.spawn(drive(application));
        Ok(Self {
            sender,
            exit,
            driver: Some(driver),
        })
    }

    /// Execute a typed mounted Action and return its direct result and new view.
    /// Dropping the caller's future does not retract an accepted invocation.
    pub async fn submit(
        &mut self,
        call: CommandCall,
    ) -> Result<StdinResponse, StdinApplicationError> {
        self.submit_projection(call).await?.into_response()
    }

    async fn submit_projection(
        &mut self,
        call: CommandCall,
    ) -> Result<StdinProjectionResponse, StdinApplicationError> {
        let sender = self.sender.clone();
        let mut response = self.receive(sender.submit_frame(vec![call])).await?;
        let (_, outcome) = response.outcomes.pop().expect("one submitted action");
        let (ok, result) = match outcome {
            CommandOutcome::Output(value) => (true, value),
            CommandOutcome::Rejected { code, message } => {
                (false, json!({"code": code, "message": message}))
            }
        };
        Ok(StdinProjectionResponse {
            ok,
            result: Some(result),
            projection: response.projection,
        })
    }

    /// Execute one complete frame in order and render its final view once.
    /// Ordinary action rejections are retained alongside successful outcomes;
    /// dropping the waiter leaves the accepted frame running to completion.
    /// An empty frame observes current state without clearing its feedback.
    pub async fn submit_frame(
        &mut self,
        calls: Vec<CommandCall>,
    ) -> Result<StdinResponse, StdinApplicationError> {
        self.submit_frame_projection(calls).await?.into_response()
    }

    async fn submit_frame_projection(
        &mut self,
        calls: Vec<CommandCall>,
    ) -> Result<StdinProjectionResponse, StdinApplicationError> {
        if calls.is_empty() {
            return self.observe_projection().await;
        }
        let sender = self.sender.clone();
        let response = self.receive(sender.submit_frame(calls)).await?;
        let mut ok = true;
        let results = response
            .outcomes
            .into_iter()
            .map(|(call, outcome)| {
                let (accepted, result) = match outcome {
                    CommandOutcome::Output(value) => (true, value),
                    CommandOutcome::Rejected { code, message } => {
                        (false, json!({"code": code, "message": message}))
                    }
                };
                ok &= accepted;
                json!({"action": call.name(), "ok": accepted, "result": result})
            })
            .collect();
        Ok(StdinProjectionResponse {
            ok,
            result: Some(Value::Array(results)),
            projection: response.projection,
        })
    }

    /// Observe prepared state without invoking a callback or clearing feedback.
    pub async fn observe(&mut self) -> Result<StdinResponse, StdinApplicationError> {
        self.observe_projection().await?.into_response()
    }

    async fn observe_projection(
        &mut self,
    ) -> Result<StdinProjectionResponse, StdinApplicationError> {
        let sender = self.sender.clone();
        let projection = self.receive(sender.observe()).await?;
        Ok(StdinProjectionResponse {
            ok: true,
            result: None,
            projection,
        })
    }

    /// Return an ordinary input error through the root-declared feedback view.
    pub async fn feedback(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<StdinResponse, StdinApplicationError> {
        self.feedback_projection(code, message)
            .await?
            .into_response()
    }

    async fn feedback_projection(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<StdinProjectionResponse, StdinApplicationError> {
        let sender = self.sender.clone();
        let projection = self
            .receive(sender.feedback(code.into(), message.into()))
            .await?;
        Ok(StdinProjectionResponse {
            ok: false,
            result: None,
            projection,
        })
    }

    /// Finish Application cleanup before the transport acknowledges shutdown.
    pub fn shutdown(
        mut self,
    ) -> impl Future<Output = Result<(), StdinApplicationError>> + Send + 'static {
        let _ = self.exit.request(ExitReason::Requested);
        let driver = self.driver.take();
        async move {
            match driver {
                Some(driver) => joined(driver.await),
                None => Ok(()),
            }
        }
    }

    async fn receive<T>(
        &mut self,
        request: impl Future<Output = Result<T, CommandInputError>>,
    ) -> Result<T, StdinApplicationError> {
        let driver = self.driver.as_mut().ok_or(CommandInputError::Closed)?;
        let response = tokio::select! {
            biased;
            // A completed invocation belongs to this request. A fault in the
            // following preparation must not replace its result or diagnostic.
            response = request => response,
            result = driver => {
                self.driver = None;
                joined(result)?;
                return Err(CommandInputError::Closed.into());
            }
        };
        if matches!(response, Err(CommandInputError::Closed)) {
            // Channel closure can precede publication of a JoinError. Join to
            // preserve the original panic payload and actual terminal fault.
            let result = self.driver.as_mut().expect("active driver").await;
            self.driver = None;
            joined(result)?;
        }
        Ok(response?)
    }
}

impl Drop for StdinApplication {
    fn drop(&mut self) {
        let _ = self.exit.request(ExitReason::Requested);
        // Dropping a JoinHandle detaches the cleanup owner; it never aborts it.
    }
}

fn joined(
    result: Result<Result<(), StdinApplicationError>, JoinError>,
) -> Result<(), StdinApplicationError> {
    match result {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(_) => Err(StdinApplicationError::DriverCancelled),
    }
}

async fn drive(
    mut application: Application<ExternalProviderPort>,
) -> Result<(), StdinApplicationError> {
    let operation = async {
        loop {
            if !application.has_command_wait() {
                return Err(StdinApplicationError::MissingWait);
            }
            if let ControlFlow::Break(_) = application.wait_for_command_request().await? {
                return Ok(());
            }
            if let ControlFlow::Break(_) = application.prepare().await? {
                return Ok(());
            }
        }
    }
    .await;
    let cleanup = application.shutdown().await;
    match (operation, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(operation), Err(cleanup)) => Err(StdinApplicationError::Cleanup {
            operation: Box::new(operation),
            cleanup,
        }),
    }
}

fn render(projection: &RenderedProjection) -> Result<String, StdinApplicationError> {
    render_items(projection.nodes().iter().flat_map(|node| node.items()))
}

fn render_items<'a>(
    items: impl IntoIterator<Item = &'a CanonicalInputItem>,
) -> Result<String, StdinApplicationError> {
    let mut sections = Vec::new();
    for item in items {
        match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => sections.push(render_pom_document(pom)?),
            _ => return Err(StdinApplicationError::UnsupportedViewItem),
        }
    }
    Ok(if sections.is_empty() {
        String::new()
    } else {
        format!("{}\n", sections.join("\n\n"))
    })
}
