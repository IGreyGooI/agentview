//! Main request-only prompts preserve durable history across real loopback HTTP.

use super::*;
use crate::transcript::{CanonicalInputItem, InstructionAuthority};

async fn complete_main(application: &mut Application<AsyncOpenAiResponsesProvider>) {
    let reaction = application.react(false).unwrap();
    assert_eq!(reaction.kind(), ReactionKind::Main);
    assert_eq!(reaction.await.unwrap(), ReactionOutcome::Completed);
}

fn request_input(request: &Value) -> &[Value] {
    request["input"].as_array().unwrap()
}

fn assistant_texts(request: &Value) -> Vec<&str> {
    request_input(request)
        .iter()
        .filter(|item| item["role"] == "assistant")
        .flat_map(|item| item["content"].as_array().unwrap())
        .map(|part| part["text"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn main_ephemeral_application_keeps_the_parent_and_uploads_durable_outputs_once() {
    let (base, requests, shutdown, server) = spawn_ark_response_sequence_server(
        [
            ("stored_a", "reply from A"),
            ("unstored_e1", "durable reply from E1"),
            ("unstored_e2", "durable reply from E2"),
            ("stored_b", "reply from B"),
            ("stored_c", "reply from C"),
        ]
        .map(|(id, text)| {
            (
                StatusCode::OK,
                "text/event-stream",
                ark_application_text_sse(id, text),
            )
        })
        .into(),
    )
    .await;
    let signal = Arc::new(Mutex::new(None));
    let root_signal = signal.clone();
    let mut application = Application::mount(
        move || ark_ephemeral_application(root_signal.clone()),
        ark_provider(&base),
    )
    .unwrap();

    complete_main(&mut application).await;
    let input = signal.lock().unwrap().clone().unwrap();
    for footer in ["current request footer one", "current request footer two"] {
        input
            .update(|input| input.observation = Some(footer.into()))
            .unwrap();
        complete_main(&mut application).await;
    }
    for main in ["ordinary turn B", "ordinary turn C"] {
        input
            .set(ArkApplicationInput {
                main: main.into(),
                observation: None,
            })
            .unwrap();
        complete_main(&mut application).await;
    }

    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[0]["store"], true);
        let footer_one = request_input(&requests[1]).last().unwrap();
        let footer_two = request_input(&requests[2]).last().unwrap();
        assert_eq!(footer_one["role"], "developer");
        assert_eq!(footer_two["role"], "developer");
        assert_ne!(footer_one, footer_two);
        for request in &requests[1..3] {
            assert_eq!(request["store"], false);
            assert_eq!(request["previous_response_id"], "stored_a");
            assert!(request.get("caching").is_none());
            assert!(request.get("tools").is_none());
        }
        assert_eq!(request_input(&requests[1]).len(), 1);
        assert_eq!(assistant_texts(&requests[1]), Vec::<&str>::new());
        assert_eq!(request_input(&requests[2]).len(), 2);
        assert_eq!(assistant_texts(&requests[2]), ["durable reply from E1"]);
        assert!(!request_input(&requests[2]).contains(footer_one));

        let b = &requests[3];
        assert_eq!(b["store"], true);
        assert_eq!(b["caching"]["type"], "enabled");
        assert_eq!(b["previous_response_id"], "stored_a");
        assert_eq!(request_input(b).len(), 3);
        assert_eq!(request_input(b).last().unwrap()["role"], "user");
        assert_eq!(
            assistant_texts(b),
            ["durable reply from E1", "durable reply from E2"]
        );
        let c = &requests[4];
        assert_eq!(c["store"], true);
        assert_eq!(c["previous_response_id"], "stored_b");
        assert_eq!(request_input(c).len(), 1);
        assert_eq!(request_input(c)[0]["role"], "user");
        for request in &requests[3..] {
            assert!(!request_input(request).contains(footer_one));
            assert!(!request_input(request).contains(footer_two));
        }
    }
    application.shutdown().await.unwrap();
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn main_ephemeral_application_without_a_parent_replays_only_durable_history() {
    let (base, requests, shutdown, server) = spawn_ark_response_sequence_server(
        [
            ("unstored_first", "durable bootstrap reply"),
            ("stored_b", "reply from B"),
            ("stored_c", "reply from C"),
        ]
        .map(|(id, text)| {
            (
                StatusCode::OK,
                "text/event-stream",
                ark_application_text_sse(id, text),
            )
        })
        .into(),
    )
    .await;
    let signal = Arc::new(Mutex::new(None));
    let root_signal = signal.clone();
    let mut application = Application::mount(
        move || ark_ephemeral_application(root_signal.clone()),
        ark_provider(&base),
    )
    .unwrap();
    assert!(application.prepare().await.unwrap().is_continue());
    let input = signal.lock().unwrap().clone().unwrap();
    input
        .update(|input| input.observation = Some("bootstrap request footer".into()))
        .unwrap();
    complete_main(&mut application).await;
    for main in ["ordinary turn B", "ordinary turn C"] {
        input
            .set(ArkApplicationInput {
                main: main.into(),
                observation: None,
            })
            .unwrap();
        complete_main(&mut application).await;
    }

    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["store"], false);
        assert!(requests[0].get("previous_response_id").is_none());
        assert!(requests[0].get("caching").is_none());
        let first_input = request_input(&requests[0]);
        assert_eq!(first_input.len(), 2);
        let durable_input = &first_input[0];
        let footer = &first_input[1];
        assert_eq!(durable_input["role"], "user");
        assert_eq!(footer["role"], "developer");

        let b = &requests[1];
        assert_eq!(b["store"], true);
        assert_eq!(b["caching"]["type"], "enabled");
        assert!(b.get("previous_response_id").is_none());
        assert_eq!(request_input(b)[0]["role"], "system");
        assert_eq!(
            request_input(b)
                .iter()
                .filter(|item| *item == durable_input)
                .count(),
            1
        );
        assert_eq!(assistant_texts(b), ["durable bootstrap reply"]);
        assert!(!request_input(b).contains(footer));
        assert_eq!(requests[2]["store"], true);
        assert_eq!(requests[2]["previous_response_id"], "stored_b");
        assert_eq!(request_input(&requests[2]).len(), 1);
        assert!(!request_input(&requests[2]).contains(footer));
    }
    application.shutdown().await.unwrap();
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

fn ephemeral_frame(
    declaration: &TargetDeclaration,
    sequence: u64,
    replay: Vec<CanonicalInputItem>,
    projection: Vec<CanonicalInputItem>,
    ephemeral: Vec<CanonicalInputItem>,
) -> Frame {
    let epoch = declaration.continuity().epoch();
    let revision = FrameRevision::new(
        NonZeroU128::new(700).unwrap(),
        declaration.identity(),
        epoch,
        NonZeroU64::new(sequence).unwrap(),
    );
    let basis = match declaration.continuity() {
        TargetContinuity::FullRequired { .. } => FrameBasis::Full,
        TargetContinuity::Accepted { revision, .. } => FrameBasis::DeltaFrom(*revision),
    };
    Frame::from_compiled(
        revision,
        declaration.identity(),
        epoch,
        declaration.continuity().clone(),
        declaration.profile().clone(),
        basis,
        FrameSubmission::from_compiled(
            replay,
            Vec::new(),
            ProjectionSubmission::new(projection),
            ToolCatalog::new(Vec::new()).unwrap(),
            Vec::new(),
        )
        .with_ephemeral(ephemeral),
    )
    .unwrap()
}

fn system_input(text: &str) -> CanonicalInputItem {
    CanonicalInputItem::instruction(
        InstructionAuthority::System,
        crate::pom_resolution::resolve_system_document(crate::pom::Document::from_raw_text(text)),
    )
}

fn footer_input(text: &str) -> CanonicalInputItem {
    CanonicalInputItem::instruction(
        InstructionAuthority::Developer,
        crate::pom_resolution::resolve_system_document(crate::pom::Document::from_raw_text(text)),
    )
}

fn sealed_output(facts: &[ProviderFact]) -> CanonicalInputItem {
    facts
        .iter()
        .find_map(|fact| match fact {
            ProviderFact::TextSealed { text, phase, .. } => {
                Some(CanonicalInputItem::assistant_text(text.clone(), *phase))
            }
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn main_ephemeral_faults_never_promote_unfinished_responses_or_reuse_incompatible_prefixes() {
    for fail_storing_response in [false, true] {
        for interrupt_stream in [false, true] {
            let mut replies = vec![(
                StatusCode::OK,
                "text/event-stream",
                ark_application_text_sse("stored_a", "reply from A"),
            )];
            if fail_storing_response {
                replies.push((
                    StatusCode::OK,
                    "text/event-stream",
                    ark_application_text_sse("unstored_e", "durable ephemeral reply"),
                ));
            }
            replies.push(if interrupt_stream {
                (
                    StatusCode::OK,
                    "text/event-stream",
                    ark_application_text_sse("unfinished_response", "interrupted reply"),
                )
            } else {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "application/json",
                    json!({"error":{"code":"ServiceUnavailable", "message":"fixture failure"}})
                        .to_string(),
                )
            });
            for (id, text) in [
                ("recovered_parent", "recovery reply"),
                ("next_parent", "next reply"),
            ] {
                replies.push((
                    StatusCode::OK,
                    "text/event-stream",
                    ark_application_text_sse(id, text),
                ));
            }
            let (base, requests, shutdown, server) =
                spawn_ark_response_sequence_server(replies).await;
            let mut provider = ark_provider(&base);
            let tools = ToolCatalog::new(Vec::new()).unwrap();
            let initial = provider.declare().unwrap();
            let initial_input = ark_user_input("initial durable input");
            let first = ark_frame(
                &initial,
                1,
                Vec::new(),
                Vec::new(),
                vec![system_input("initial policy"), initial_input.clone()],
                tools.clone(),
            );
            let facts: Vec<_> = provider
                .submit(first)
                .await
                .unwrap()
                .map(Result::unwrap)
                .collect()
                .await;
            let initial_output = sealed_output(&facts);
            let mut history = vec![initial_input, initial_output.clone()];
            let continuation_input = ark_user_input("durable continuation input");
            history.push(continuation_input.clone());
            let accepted = provider.declare().unwrap();
            let ephemeral = ephemeral_frame(
                &accepted,
                2,
                vec![initial_output],
                vec![continuation_input],
                vec![footer_input("request-only footer")],
            );
            let failing = if fail_storing_response {
                let facts: Vec<_> = provider
                    .submit(ephemeral)
                    .await
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
                    .await;
                let output = sealed_output(&facts);
                history.push(output.clone());
                let storing_input = ark_user_input("next durable input");
                history.push(storing_input.clone());
                ark_frame(
                    &provider.declare().unwrap(),
                    3,
                    vec![output],
                    Vec::new(),
                    vec![storing_input],
                    tools.clone(),
                )
            } else {
                ephemeral
            };
            let mut stream = provider.submit(failing).await.unwrap();
            if interrupt_stream {
                let ProviderFact::TextDelta { delta, phase, .. } =
                    stream.next().await.unwrap().unwrap()
                else {
                    panic!("fixture must publish a partial reply before cancellation");
                };
                history.push(CanonicalInputItem::interrupted_assistant_text(delta, phase));
            } else {
                let fault = stream.next().await.unwrap().unwrap_err();
                assert_eq!(fault.kind(), ReactionPortFaultKind::Retryable);
                assert!(stream.next().await.is_none());
            }
            drop(stream);

            let recovery = provider.declare().unwrap();
            assert!(matches!(
                recovery.continuity(),
                TargetContinuity::FullRequired { .. }
            ));
            let full = ark_frame(
                &recovery,
                4,
                history,
                Vec::new(),
                vec![
                    system_input("replacement policy"),
                    ark_user_input("recover"),
                ],
                tools.clone(),
            );
            let facts: Vec<_> = provider
                .submit(full)
                .await
                .unwrap()
                .map(Result::unwrap)
                .collect()
                .await;
            let next = ark_frame(
                &provider.declare().unwrap(),
                5,
                vec![sealed_output(&facts)],
                Vec::new(),
                vec![ark_user_input("continue after recovery")],
                tools,
            );
            let facts: Vec<_> = provider
                .submit(next)
                .await
                .unwrap()
                .map(Result::unwrap)
                .collect()
                .await;
            assert!(matches!(
                facts.last(),
                Some(ProviderFact::ReactionCompleted { .. })
            ));

            {
                let requests = requests.lock().unwrap();
                let recovery_index = if fail_storing_response { 3 } else { 2 };
                assert_eq!(requests.len(), recovery_index + 2);
                assert_eq!(requests[1]["store"], false);
                assert_eq!(requests[1]["previous_response_id"], "stored_a");
                let footer = request_input(&requests[1]).last().unwrap();
                assert_eq!(footer["role"], "developer");
                if fail_storing_response {
                    assert_eq!(requests[2]["store"], true);
                    assert_eq!(requests[2]["previous_response_id"], "stored_a");
                    assert_eq!(assistant_texts(&requests[2]), ["durable ephemeral reply"]);
                }
                let recovered = &requests[recovery_index];
                assert_eq!(recovered["store"], true);
                assert!(recovered.get("previous_response_id").is_none());
                let old_system = &request_input(&requests[0])[0];
                assert_eq!(request_input(recovered)[0]["role"], "system");
                assert!(!request_input(recovered).contains(old_system));
                assert!(!request_input(recovered).contains(footer));
                let mut expected = vec!["reply from A"];
                if fail_storing_response {
                    expected.push("durable ephemeral reply");
                }
                if interrupt_stream {
                    expected.push("interrupted reply");
                }
                assert_eq!(assistant_texts(recovered), expected);
                let following = &requests[recovery_index + 1];
                assert_eq!(following["store"], true);
                assert_eq!(following["previous_response_id"], "recovered_parent");
                assert_eq!(request_input(following).len(), 1);
            }
            provider.shutdown().await;
            shutdown.send(()).unwrap();
            server.await.unwrap();
        }
    }
}
