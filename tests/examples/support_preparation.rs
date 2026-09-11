#[allow(dead_code)]
#[path = "../support/scripted_provider.rs"]
mod scripted_provider;

use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    task::Poll,
    time::Duration,
};

use agentview::component::execution::{
    Application, ApplicationFaultReason, ReactionAdmissionReason,
};
use futures::poll;
use scripted_provider::{ScriptedCapture, ScriptedProvider, ScriptedReaction};
use tokio::sync::{mpsc, Mutex};

use super::*;

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

struct DataDir(PathBuf);

impl DataDir {
    async fn without_policy() -> Self {
        let path = std::env::temp_dir().join(format!(
            "agentview-support-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::create_dir(&path).await.unwrap();
        tokio::fs::copy(
            support_data_dir().join("accounts.json"),
            path.join("accounts.json"),
        )
        .await
        .unwrap();
        Self(path)
    }
}

impl Drop for DataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mount(
    data_dir: PathBuf,
    replies: impl IntoIterator<Item = ScriptedReaction>,
) -> (
    Application<ScriptedProvider>,
    mpsc::Sender<Ticket>,
    SupportInbox,
    ScriptedCapture,
) {
    let (sender, receiver) = mpsc::channel(4);
    let inbox = SupportInbox {
        tickets: Arc::new(Mutex::new(receiver)),
        drafts: Arc::new(Mutex::new(Vec::new())),
        data_dir,
    };
    let root = inbox.clone();
    let (provider, capture) = ScriptedProvider::new(replies).unwrap();
    let application = Application::mount(move || support_agent(root.clone()), provider).unwrap();
    (application, sender, inbox, capture)
}

async fn tickets() -> Vec<Ticket> {
    read_json(support_data_dir().join("tickets.json"))
        .await
        .unwrap()
}

#[tokio::test]
async fn every_ticket_waits_for_its_account_and_policy_before_handoff() {
    let (mut application, sender, inbox, capture) = mount(
        support_data_dir(),
        [
            ScriptedReaction::text(["Regenerate the expired export."]),
            ScriptedReaction::text(["Escalate the missing export."]),
        ],
    );
    for ticket in tickets().await {
        sender.send(ticket).await.unwrap();
    }
    drop(sender);

    assert_eq!(application.run().await.unwrap(), ExitReason::Completed);
    application.shutdown().await.unwrap();

    let frames = capture.frames();
    assert_eq!(frames.len(), 2);
    assert!(frames[0]
        .text
        .contains("<account_id>acct-1042</account_id>"));
    assert!(frames[0].text.contains("<customer_request>"));
    assert!(frames[0].text.contains("<ticket_id>ticket-101</ticket_id>"));
    assert!(frames[0]
        .text
        .contains("<export_retention_days>30</export_retention_days>"));
    assert!(frames[0].text.contains(
        "## Export support policy\n\n- Expired exports: Expired downloads cannot be restored. Regenerate the export from the original report.\n- Escalation: If an export is unavailable within the account's retention window, route it to the export support queue."
    ));
    assert!(!frames[0].text.contains("<support_policy>"));
    assert!(frames[0].text.contains(
        "## Support response\n\nDraft a concise response to the current ticket using its account and support policy."
    ));
    assert!(frames[1]
        .text
        .contains("<account_id>acct-2050</account_id>"));
    assert!(frames[1]
        .text
        .contains("<export_retention_days>7</export_retention_days>"));
    assert!(!frames[1]
        .text
        .contains("<account_id>acct-1042</account_id>"));
    let drafts = inbox.drafts.lock().await;
    assert_eq!(
        drafts
            .iter()
            .map(|draft| draft.ticket_id.as_str())
            .collect::<Vec<_>>(),
        ["ticket-101", "ticket-102"]
    );
}

#[tokio::test]
async fn idle_inbox_blocks_and_cancelled_wait_can_resume() {
    let (mut application, sender, inbox, capture) = mount(
        support_data_dir(),
        [ScriptedReaction::text(["Draft reply."])],
    );
    let mut run = Box::pin(application.run());
    assert!(matches!(poll!(run.as_mut()), Poll::Pending));
    assert_eq!(capture.submission_count(), 0);
    drop(run);

    sender.send(tickets().await.remove(0)).await.unwrap();
    drop(sender);
    assert_eq!(application.run().await.unwrap(), ExitReason::Completed);
    application.shutdown().await.unwrap();
    assert_eq!(capture.submission_count(), 1);
    assert_eq!(inbox.drafts.lock().await.len(), 1);
}

#[tokio::test]
async fn failed_context_load_keeps_the_ticket_for_retry_without_model_io() {
    let data = DataDir::without_policy().await;
    let (mut application, sender, inbox, capture) =
        mount(data.0.clone(), [ScriptedReaction::text(["Draft reply."])]);
    sender.send(tickets().await.remove(0)).await.unwrap();
    drop(sender);

    let error = application.run().await.unwrap_err();
    assert_eq!(
        error.stage(),
        agentview::component::execution::ApplicationFaultStage::Preparation
    );
    assert_eq!(capture.submission_count(), 0);
    assert!(inbox.drafts.lock().await.is_empty());

    tokio::fs::copy(
        support_data_dir().join("policy.json"),
        data.0.join("policy.json"),
    )
    .await
    .unwrap();
    assert_eq!(application.run().await.unwrap(), ExitReason::Completed);
    application.shutdown().await.unwrap();
    assert_eq!(capture.submission_count(), 1);
    assert_eq!(inbox.drafts.lock().await[0].ticket_id, "ticket-101");
}

#[tokio::test]
async fn host_can_exit_while_the_inbox_is_still_open() {
    let (mut application, _sender, inbox, capture) = mount(support_data_dir(), []);
    let exit = application.exit_handle();
    let mut run = Box::pin(application.run());
    assert!(matches!(poll!(run.as_mut()), Poll::Pending));
    exit.request(ExitReason::Requested).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), run)
            .await
            .unwrap()
            .unwrap(),
        ExitReason::Requested
    );
    application.shutdown().await.unwrap();
    assert_eq!(capture.submission_count(), 0);
    assert!(inbox.drafts.lock().await.is_empty());
}

#[tokio::test]
async fn empty_model_reply_does_not_complete_the_ticket() {
    let (mut application, sender, inbox, capture) =
        mount(support_data_dir(), [ScriptedReaction::text([" "])]);
    sender.send(tickets().await.remove(0)).await.unwrap();
    drop(sender);
    assert!(application.run().await.is_err());
    application.shutdown().await.unwrap();
    assert_eq!(capture.submission_count(), 1);
    assert!(inbox.drafts.lock().await.is_empty());
}

#[tokio::test]
async fn retry_does_not_reuse_text_completed_before_a_later_admission_fault() {
    let (mut application, sender, inbox, capture) = mount(
        support_data_dir(),
        [
            ScriptedReaction::text(["stale reply"]).duplicate_completion(),
            ScriptedReaction::empty(),
            ScriptedReaction::text(["fresh reply"]),
        ],
    );
    sender.send(tickets().await.remove(0)).await.unwrap();
    drop(sender);

    let first = application.run().await.unwrap_err();
    assert_eq!(
        first.reason(),
        ApplicationFaultReason::Admission(ReactionAdmissionReason::DuplicateCompletion)
    );
    assert_eq!(capture.submission_count(), 1);
    assert!(inbox.drafts.lock().await.is_empty());

    let second = application.run().await.unwrap_err();
    assert_eq!(second.reason(), ApplicationFaultReason::EventHandler);
    assert_eq!(capture.submission_count(), 2);
    assert!(inbox.drafts.lock().await.is_empty());

    assert_eq!(application.run().await.unwrap(), ExitReason::Completed);
    application.shutdown().await.unwrap();
    assert_eq!(capture.submission_count(), 3);
    let drafts = inbox.drafts.lock().await;
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].ticket_id, "ticket-101");
    assert_eq!(drafts[0].text, "fresh reply");
}
