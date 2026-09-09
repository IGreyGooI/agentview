//! A live support-drafting agent that prepares inbox and account context before each model turn.
//!
//! Set `OPENAI_API_KEY`, then run `cargo run --example support_preparation`.

use std::{path::PathBuf, sync::Arc};

use agentview::component::{execution::Application, prelude::*};
use anyhow::{ensure, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};

#[path = "support/live_provider.rs"]
mod live_provider;

#[derive(Clone, Deserialize)]
struct Ticket {
    id: String,
    account_id: String,
    request: String,
}

#[derive(Clone, Deserialize)]
struct Account {
    id: String,
    plan: String,
    export_retention_days: u32,
}

#[derive(Clone, Deserialize)]
struct SupportPolicy {
    expired_export: String,
    escalation: String,
}

#[derive(Debug, Serialize)]
struct ReplyDraft {
    ticket_id: String,
    text: String,
}

#[derive(Clone)]
struct SupportInbox {
    tickets: Arc<Mutex<mpsc::Receiver<Ticket>>>,
    drafts: Arc<Mutex<Vec<ReplyDraft>>>,
    data_dir: PathBuf,
}

async fn read_json<T: DeserializeOwned>(path: PathBuf) -> Result<T> {
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("decode {}", path.display()))
}

#[component]
fn account_context(account_id: String, data_dir: PathBuf) -> Component {
    let account = use_signal(|| None::<Account>);
    let prepared = account.clone();
    use_preparation(move || async move {
        if prepared.with(Option::is_none)? {
            let accounts: Vec<Account> = read_json(data_dir.join("accounts.json")).await?;
            let account = accounts
                .into_iter()
                .find(|account| account.id == account_id)
                .with_context(|| format!("unknown account {account_id}"))?;
            prepared.set(Some(account))?;
        }
        Ok::<(), anyhow::Error>(())
    });

    match account.with(Clone::clone).expect("mounted account") {
        Some(Account {
            id,
            plan,
            export_retention_days,
        }) => view! {
            #[developer]
            account_context {
                account_id { "{id}" }
                plan { "{plan}" }
                export_retention_days { "{export_retention_days}" }
            }
        },
        None => view! {},
    }
}

#[component]
fn support_policy(data_dir: PathBuf) -> Component {
    let policy = use_signal(|| None::<SupportPolicy>);
    let prepared = policy.clone();
    use_preparation(move || async move {
        if prepared.with(Option::is_none)? {
            let policy = read_json(data_dir.join("policy.json")).await?;
            prepared.set(Some(policy))?;
        }
        Ok::<(), anyhow::Error>(())
    });

    match policy.with(Clone::clone).expect("mounted support policy") {
        Some(SupportPolicy {
            expired_export,
            escalation,
        }) => view! {
            #[developer]
            support_policy {
                expired_export { "{expired_export}" }
                escalation { "{escalation}" }
            }
        },
        None => view! {},
    }
}

#[component]
fn support_agent(inbox: SupportInbox) -> Component {
    let ticket = use_signal(|| None::<Ticket>);
    let answer = use_signal(|| None::<String>);
    let exit = use_application_exit();

    let current = ticket.clone();
    let next_answer = answer.clone();
    let incoming = inbox.tickets.clone();
    use_preparation(move || async move {
        if next_answer.with(Option::is_some)? {
            next_answer.set(None)?;
        }
        if current.with(Option::is_none)? {
            let next = incoming.lock().await.recv().await;
            match next {
                Some(ticket) => current.set(Some(ticket))?,
                None => exit.request(ExitReason::Completed)?,
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    let received = answer.clone();
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let received = received.clone();
        async move {
            if let TextTurnEvent::TextComplete(text) = event {
                received.set(Some(text))?;
            }
            Ok::<(), SignalAccessError>(())
        }
    });

    let completed = ticket.clone();
    let drafts = inbox.drafts.clone();
    use_reaction_completion(move || async move {
        let text = answer
            .with(Clone::clone)?
            .context("model returned no reply")?;
        ensure!(!text.trim().is_empty(), "model returned an empty reply");
        let ticket = completed
            .with(Clone::clone)?
            .context("no active support ticket")?;
        let mut drafts = drafts.lock().await;
        drafts.push(ReplyDraft {
            ticket_id: ticket.id,
            text,
        });
        completed.set(None)?;
        Ok::<(), anyhow::Error>(())
    });

    let active = ticket
        .with(Clone::clone)
        .expect("mounted support ticket")
        .map(
            |Ticket {
                 id,
                 account_id,
                 request,
             }| {
                view! {
                    customer_request {
                        ticket_id { "{id}" }
                        request { "{request}" }
                    }
                    account_context(account_id, inbox.data_dir.clone())
                }
            },
        )
        .unwrap_or_else(|| view! {});
    view! {
        #[system_once]
        support_agent {
            "Draft a concise response to the current ticket using its account and support policy. Do not send it to the customer. Do not reuse another ticket's account context."
        }
        support_policy(inbox.data_dir)
        { active }
    }
}

fn support_data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/data/support")
}

#[tokio::main]
async fn main() -> Result<()> {
    let data_dir = support_data_dir();
    let tickets: Vec<Ticket> = read_json(data_dir.join("tickets.json")).await?;
    let (sender, receiver) = mpsc::channel(tickets.len().max(1));
    for ticket in tickets {
        sender.send(ticket).await?;
    }
    drop(sender);

    let drafts = Arc::new(Mutex::new(Vec::new()));
    let inbox = SupportInbox {
        tickets: Arc::new(Mutex::new(receiver)),
        drafts: Arc::clone(&drafts),
        data_dir,
    };
    let provider = live_provider::from_env("support-preparation")?;
    let mut application = Application::mount(move || support_agent(inbox.clone()), provider)?;
    let result = application.run().await;
    let shutdown = application.shutdown().await;
    result.map_err(|operation| match &shutdown {
        Ok(()) => anyhow::Error::from(operation),
        Err(shutdown) => anyhow::Error::from(operation)
            .context(format!("support agent shutdown also failed: {shutdown}")),
    })?;
    shutdown?;
    println!("{}", serde_json::to_string_pretty(&*drafts.lock().await)?);
    Ok(())
}

#[cfg(test)]
#[path = "../tests/examples/support_preparation.rs"]
mod tests;
