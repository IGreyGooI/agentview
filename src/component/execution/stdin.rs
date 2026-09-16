//! A retained application for stdin/stdout frontends, without a model provider.

use std::{future::Future, ops::ControlFlow};

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

/// One response for stdout: a callback result alongside the complete current view.
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
    /// Without a subcommand it reads JSON actions from stdin, starts a missing
    /// daemon, and flushes each callback result and updated view to stdout.
    /// Empty stdin observes the current view; terminal input observes first.
    /// EOF leaves the daemon and its application state running.
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
        #[cfg(unix)]
        {
            cli::run(root).await
        }
        #[cfg(not(unix))]
        {
            let _ = root;
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
        let sender = self.sender.clone();
        let response = self.receive(sender.submit(call)).await?;
        let (ok, result) = match response.outcome {
            CommandOutcome::Output(value) => (true, value),
            CommandOutcome::Rejected { code, message } => {
                (false, json!({"code": code, "message": message}))
            }
        };
        Ok(StdinResponse {
            ok,
            result: Some(result),
            view: render(&response.projection)?,
        })
    }

    /// Observe prepared state without invoking a callback or clearing feedback.
    pub async fn observe(&mut self) -> Result<StdinResponse, StdinApplicationError> {
        let sender = self.sender.clone();
        let projection = self.receive(sender.observe()).await?;
        Ok(StdinResponse {
            ok: true,
            result: None,
            view: render(&projection)?,
        })
    }

    /// Return an ordinary input error through the root-declared feedback view.
    pub async fn feedback(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<StdinResponse, StdinApplicationError> {
        let sender = self.sender.clone();
        let projection = self
            .receive(sender.feedback(code.into(), message.into()))
            .await?;
        Ok(StdinResponse {
            ok: false,
            result: None,
            view: render(&projection)?,
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
    let mut sections = Vec::new();
    for item in projection.nodes().iter().flat_map(|node| node.items()) {
        match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => sections.push(render_pom_document(pom)?),
            _ => return Err(StdinApplicationError::UnsupportedViewItem),
        }
    }
    Ok(format!("{}\n", sections.join("\n\n")))
}
