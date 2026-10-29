//! Fork a settled model history with an explicit, owned business snapshot.
//! The real Responses port talks to a deterministic local fixture; no credentials are needed.
//! Run `cargo run --no-default-features --example application_fork`.

use std::sync::{Arc, Mutex};

use agentview::{
    component::{
        execution::{Application, ProviderIdentity},
        prelude::*,
    },
    provider::{
        async_openai::{AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig},
        codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options},
        ModelSpec,
    },
};
use anyhow::{ensure, Context as _};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch, Semaphore};

#[path = "support/fork_fixture.rs"]
mod fixture;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Progress {
    Idle,
    Running,
    Completed,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Report {
    progress: Progress,
    cheapest_cost: Option<u32>,
}

#[derive(Clone, Serialize, Deserialize)]
struct BusinessSnapshot {
    costs: [u32; 2],
    report: Report,
    accepted: bool,
    feedback: String,
}

#[derive(Clone)]
struct Props {
    snapshot: BusinessSnapshot,
    exported: Arc<Mutex<BusinessSnapshot>>,
    reports: watch::Receiver<Report>,
    publish: Option<watch::Sender<Report>>,
    finish_comparison: Arc<Semaphore>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AcceptPlan {
    expected_cost: u32,
}

#[component]
fn planning(props: Props) -> Component {
    let state = use_signal(|| props.snapshot.clone());
    let worker = props.publish.is_some();
    let prepare_state = state.clone();
    let reports = props.reports.clone();
    let exported = props.exported.clone();
    use_preparation(move || {
        let (state, reports, exported) = (prepare_state.clone(), reports.clone(), exported.clone());
        async move {
            if !worker {
                let report = reports.borrow().clone();
                let current = state.update(|state| {
                    if state.report != report {
                        state.report = report;
                        state.feedback = "The latest worker report is shown below.".into();
                    }
                    state.clone()
                })?;
                *exported.lock().unwrap() = current;
            }
            Ok::<(), anyhow::Error>(())
        }
    });
    let current = state.with(Clone::clone).expect("mounted planning state");
    let [first, second] = current.costs;
    let cheapest = current.report.cheapest_cost;
    let progress = match current.report.progress {
        Progress::Idle => "idle",
        Progress::Running => "running",
        Progress::Completed => "completed",
    };
    let accepted = current.accepted;
    let feedback = current.feedback;
    let next_action = if worker && current.report.progress == Progress::Idle {
        "Call compare_plans, then inspect its result on the next turn."
    } else if worker && current.report.progress == Progress::Running {
        "The comparison is running; wait for its completed result."
    } else if worker {
        "The comparison is complete; report cheapest_cost using the delivered result."
    } else if cheapest.is_some() && !accepted {
        "Call accept_plan with expected_cost equal to cheapest_cost."
    } else if accepted {
        "The plan is accepted; report the result."
    } else {
        "Wait for the worker's completed report before accepting a plan."
    };
    let actions = if let Some(publish) = props.publish {
        let exported = props.exported;
        let gate = props.finish_comparison;
        view! {
            NativeToolCall {
                name: "compare_plans",
                description: "Before comparison starts, compare the two displayed costs and publish progress and the cheapest cost.",
                on_call: move || {
                    let (state, publish, gate, exported) = (state.clone(), publish.clone(), gate.clone(), exported.clone());
                    async move {
                        if state.with(|state| state.report.progress != Progress::Idle)? {
                            return Ok(state.update(|state| {
                                state.feedback = "Ignored: this comparison has already started; inspect its current progress or result.".into();
                                state.clone()
                            })?);
                        }
                        let running = Report { progress: Progress::Running, cheapest_cost: None };
                        state.update(|state| {
                            state.report = running.clone();
                            state.feedback = "Comparison is running.".into();
                        })?;
                        publish.send_replace(running);
                        gate.acquire().await?.forget(); // host lets the parent observe progress first
                        let completed = state.update(|state| {
                            state.report = Report { progress: Progress::Completed, cheapest_cost: state.costs.iter().min().copied() };
                            state.feedback = "Comparison completed; cheapest cost is available.".into();
                            state.clone()
                        })?;
                        *exported.lock().unwrap() = completed.clone();
                        publish.send_replace(completed.report.clone());
                        Ok::<_, anyhow::Error>(completed)
                    }
                },
            }
        }
    } else {
        let exported = props.exported;
        view! {
            NativeToolCall {
                name: "accept_plan",
                description: "Accept only a completed comparison; expected_cost must equal the displayed cheapest_cost.",
                on_call: move |input: AcceptPlan| {
                    let result = state.update(|state| {
                        if state.report.progress == Progress::Completed && state.report.cheapest_cost == Some(input.expected_cost) {
                            state.accepted = true;
                            state.feedback = "Accepted the cheapest plan.".into();
                        } else { state.feedback = "Ignored: wait for a completed comparison and use its current cheapest_cost.".into(); }
                        state.clone()
                    })?;
                    *exported.lock().unwrap() = result.clone();
                    Ok::<_, SignalAccessError>(result)
                },
            }
        }
    };
    view! {
        #[system_once]
        "Choose the cheaper plan using the current comparison. Observe action feedback before choosing another action."
        planning_state {
            costs { first: first, second: second, }
            progress { "{progress}" }
            cheapest_cost { "{cheapest:?}" }
            accepted { "{accepted}" }
            feedback { "{feedback}" }
            next_action { "{next_action}" }
        }
        { actions }
    }
}

async fn scenario(base: &str, audit: &fixture::Fixture) -> anyhow::Result<()> {
    let report = Report {
        progress: Progress::Idle,
        cheapest_cost: None,
    };
    let snapshot = BusinessSnapshot {
        costs: [63, 42],
        report: report.clone(),
        accepted: false,
        feedback: "Ready to compare.".into(),
    };
    let exported = Arc::new(Mutex::new(snapshot.clone()));
    let (publish, mut reports) = watch::channel(report);
    let gate = Arc::new(Semaphore::new(0));
    let props = Props {
        snapshot,
        exported: exported.clone(),
        reports: reports.clone(),
        publish: None,
        finish_comparison: gate.clone(),
    };
    let encoder = CodexHttpV1Encoder::new(CodexHttpV1Options::new(
        ModelSpec::new("local-fixture", 128_000)?,
        None,
        None,
        None::<String>,
    )?);
    let provider = AsyncOpenAiResponsesProvider::try_new(
        AsyncOpenAiTransportConfig::new(base, "local-fixture")?,
        ProviderIdentity::new("fixture", "responses", 1, "application-fork")?,
        encoder,
    )?
    .without_parallel_compaction();
    let mut parent = Application::mount(move || planning(props.clone()), provider)?;
    let mut child = None;
    let operation = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        ensure!(parent.react().await?.is_continue()); // the common assistant output is now settled history
        let snapshot = exported.lock().unwrap().clone(); // owned values, no parent Signal handles
        let worker_export = Arc::new(Mutex::new(snapshot.clone()));
        let worker_props = Props {
            snapshot,
            exported: worker_export.clone(),
            reports: reports.clone(),
            publish: Some(publish),
            finish_comparison: gate.clone(),
        };
        child = Some(parent.fork(move || planning(worker_props.clone())).await?);
        let worker = child.as_mut().unwrap();
        let (parent_result, worker_result) = tokio::join!(
            async {
                drop(
                    reports
                        .wait_for(|report| report.progress == Progress::Running)
                        .await?,
                );
                ensure!(parent.react().await?.is_continue()); // progress reaches the parent model while child work is Pending
                ensure!(exported.lock().unwrap().report.progress == Progress::Running);
                println!(
                    "Parent observed: comparison running while the child reaction is Pending."
                );
                gate.add_permits(1);
                drop(
                    reports
                        .wait_for(|report| report.progress == Progress::Completed)
                        .await?,
                );
                ensure!(parent.react().await?.is_continue()); // observe the completed report, then accept its displayed cost
                ensure!(parent.react().await?.is_continue()); // deliver the actual acceptance result and updated view
                Ok::<_, anyhow::Error>(())
            },
            async {
                ensure!(worker.react().await?.is_continue());
                ensure!(worker.react().await?.is_continue()); // deliver compare_plans output and completed worker state
                Ok::<_, anyhow::Error>(())
            },
        );
        parent_result?;
        worker_result?;
        ensure!(exported.lock().unwrap().accepted);
        ensure!(
            !worker_export.lock().unwrap().accepted,
            "forked business state must be independent"
        );
        verify_deliveries(audit)?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("fork scenario timed out")
    .and_then(|result| result);
    let (parent_cleanup, child_cleanup) = tokio::join!(parent.shutdown(), async {
        if let Some(child) = child {
            child.shutdown().await?;
        }
        Ok::<_, agentview::component::execution::ApplicationFault>(())
    });
    operation?;
    parent_cleanup?;
    child_cleanup?;
    Ok(())
}

fn verify_deliveries(audit: &fixture::Fixture) -> anyhow::Result<()> {
    let requests = audit.requests.lock().unwrap();
    let worker: Vec<_> = requests
        .iter()
        .filter(|(worker, _)| *worker)
        .map(|(_, request)| request)
        .collect();
    let parent: Vec<_> = requests
        .iter()
        .filter(|(worker, _)| !worker)
        .map(|(_, request)| request)
        .collect();
    ensure!(worker.len() == 2 && parent.len() == 4);
    ensure!(
        worker[0]["input"].as_array().unwrap().iter().any(|item| {
            item["role"] == "assistant" && item["content"][0]["text"] == fixture::COMMON_HISTORY
        }),
        "the child's first request must retain the common assistant history"
    );
    for (request, accepted) in [(worker[1], false), (parent[3], true)] {
        let output = request["input"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|item| item["type"] == "function_call_output")
            .context("action output was not delivered")?;
        let result: BusinessSnapshot = serde_json::from_str(output["output"].as_str().unwrap())?;
        ensure!(result.report.cheapest_cost == Some(42) && result.accepted == accepted);
    }
    Ok(())
}

async fn run() -> anyhow::Result<()> {
    let audit = Arc::new(fixture::Fixture::default());
    let router = axum::Router::new()
        .route("/responses", axum::routing::post(fixture::respond))
        .with_state(audit.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    let operation = scenario(&base, &audit).await;
    let _ = stop.send(());
    let shutdown = server.await;
    operation?;
    shutdown??;
    println!("Verified: shared history -> independent comparison -> parent sees progress -> completed report -> accept -> feedback.");
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run().await
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn parent_observes_child_progress_and_result_before_accepting() {
        super::run().await.unwrap();
    }
}
