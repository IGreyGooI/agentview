use std::sync::{Arc, Mutex};

use super::*;
use crate::component::{prelude::*, ComponentHost};

#[component]
fn refresh_owner(captured: Arc<Mutex<Option<FrameRefresh>>>) -> Component {
    let refresh = use_frame_refresh();
    *captured.lock().unwrap() = Some(refresh);
    Component::text("Current state")
}

#[tokio::test]
async fn mounted_hook_dispatches_each_refresh_intent_with_its_payload() {
    let captured = Arc::new(Mutex::new(None));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let received = requests.clone();
    let (_active, active_response) = watch::channel(Some("response-3".to_owned()));
    let dispatcher = FrameRefreshDispatcher::new(active_response, move |request| {
        received.lock().unwrap().push(request);
        Ok(())
    });
    let mut host = ComponentHost::new_root(refresh_owner, captured.clone());
    host.set_frame_refresh_dispatcher(Some(dispatcher));
    host.render().unwrap();
    let refresh = captured.lock().unwrap().clone().unwrap();

    refresh.request().await.unwrap();
    refresh
        .request_and_respond("completion-7", "Report the completed task")
        .await
        .unwrap();
    let response = refresh.active_response_id().unwrap().unwrap();
    refresh
        .interrupt_and_respond(response, "Use the updated context")
        .await
        .unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(matches!(&requests[0], FrameRefreshRequest::Silent));
    assert!(matches!(
        &requests[1],
        FrameRefreshRequest::Respond { correlation, instruction }
            if correlation == "completion-7" && instruction == "Report the completed task"
    ));
    assert!(matches!(
        &requests[2],
        FrameRefreshRequest::Replace { response_id, instruction }
            if response_id == "response-3" && instruction == "Use the updated context"
    ));
}

#[tokio::test]
async fn retired_hook_cannot_dispatch_into_the_remounted_owner() {
    let captured = Arc::new(Mutex::new(None));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let received = requests.clone();
    let (_active, active_response) = watch::channel(Some("response-3".to_owned()));
    let dispatcher = FrameRefreshDispatcher::new(active_response, move |request| {
        received.lock().unwrap().push(request);
        Ok(())
    });
    let mut host = ComponentHost::new_root(refresh_owner, captured.clone());
    host.set_frame_refresh_dispatcher(Some(dispatcher));
    host.render().unwrap();
    let retired = captured.lock().unwrap().clone().unwrap();
    host.remount(captured.clone()).unwrap();
    host.render().unwrap();
    let current = captured.lock().unwrap().clone().unwrap();

    assert!(matches!(
        retired.request().await,
        Err(FrameRefreshError::StaleMount)
    ));
    assert!(matches!(
        retired.request_and_respond("old-task", "Old result").await,
        Err(FrameRefreshError::StaleMount)
    ));
    assert!(matches!(
        retired
            .interrupt_and_respond("old-response", "Old replacement")
            .await,
        Err(FrameRefreshError::StaleMount)
    ));
    assert!(matches!(
        retired.active_response_id(),
        Err(FrameRefreshError::StaleMount)
    ));
    assert!(requests.lock().unwrap().is_empty());

    current.request().await.unwrap();
    assert!(matches!(
        requests.lock().unwrap().as_slice(),
        [FrameRefreshRequest::Silent]
    ));
}

#[tokio::test]
async fn mounted_hook_reports_an_application_without_refresh_support() {
    let captured = Arc::new(Mutex::new(None));
    let mut host = ComponentHost::new_root(refresh_owner, captured.clone());
    host.render().unwrap();
    let refresh = captured.lock().unwrap().clone().unwrap();

    assert!(matches!(
        refresh.request().await,
        Err(FrameRefreshError::Unsupported)
    ));
}
