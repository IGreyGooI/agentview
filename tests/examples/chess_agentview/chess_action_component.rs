use std::sync::Arc;

use super::super::application_state::ChessFeedback;
use super::*;
use agentview::component::ComponentHost;

#[derive(Clone)]
struct PublisherTestProps {
    exposed: Arc<Mutex<Option<Signal<ChessState>>>>,
}

#[component]
fn publisher_test_root(props: PublisherTestProps) -> Component {
    let state = use_signal(|| ChessState::new(1));
    *props.exposed.lock().unwrap() = Some(state);
    view! {}
}

fn mounted_publisher_state() -> (ComponentHost<PublisherTestProps>, Signal<ChessState>) {
    let props = PublisherTestProps {
        exposed: Arc::new(Mutex::new(None)),
    };
    let mut host = ComponentHost::new_root(publisher_test_root, props.clone());
    host.render().expect("publisher test root renders");
    let state = props
        .exposed
        .lock()
        .unwrap()
        .clone()
        .expect("publisher test root exposes its Signal");
    (host, state)
}

#[test]
fn strict_move_decoder_rejects_noncanonical_uci() {
    assert!("e2e4".parse::<StrictUciMove>().is_ok());
    assert!("E2E4".parse::<StrictUciMove>().is_err());
}

#[test]
fn rejected_uci_extraction_decodes_xml_attribute_escapes() {
    assert_eq!(
        extract_uci_attribute(r#"<choose_move uci="e2e&amp;4" />"#),
        Some("e2e&4".to_owned())
    );
}

#[test]
fn publishing_the_same_operation_twice_reuses_its_journaled_result() {
    let (_host, state) = mounted_publisher_state();
    assert_eq!(
        reduce_signal(&state, ChessEvent::Start),
        ChessReduction::Applied
    );
    let attempt = state
        .with(|state| state.current_attempt())
        .unwrap()
        .expect("model attempt is active");
    let mut publisher = ChessActionPublisher {
        state: state.clone(),
        attempt,
    };
    let operation = ChessActionPublication::new(ChessAction::Resign);

    assert!(matches!(
        publisher.publish_operation(&operation),
        StreamingPublishOutcome::Published(())
    ));
    let after_first_publish = state.with(Clone::clone).unwrap();

    assert!(matches!(
        publisher.publish_operation(&operation),
        StreamingPublishOutcome::Published(())
    ));
    assert_eq!(state.with(Clone::clone).unwrap(), after_first_publish);
}

#[test]
fn publishing_an_illegal_current_action_records_one_rejection() {
    let (_host, state) = mounted_publisher_state();
    assert_eq!(
        reduce_signal(&state, ChessEvent::Start),
        ChessReduction::Applied
    );
    let attempt = state
        .with(|state| state.current_attempt())
        .unwrap()
        .expect("model attempt is active");
    let action = ChessAction::ChooseMove(
        parse_strict_uci_move("e2e5").expect("move has syntactically valid UCI"),
    );
    let mut publisher = ChessActionPublisher {
        state: state.clone(),
        attempt,
    };
    let operation = ChessActionPublication::new(action);

    assert!(matches!(
        publisher.publish_operation(&operation),
        StreamingPublishOutcome::Published(())
    ));
    assert_eq!(operation.receipt.state(), ChessPublicationState::Published);
    assert_eq!(
        state.with(|state| state.feedback()).unwrap(),
        ChessFeedback::Rejected(InvalidActionReason::IllegalMove(action))
    );
    assert_eq!(state.with(|state| state.retry_attempts()).unwrap(), 1);
    assert!(state
        .with(|state| state.committed_moves().is_empty())
        .unwrap());
    let after_first_publish = state.with(Clone::clone).unwrap();

    assert!(matches!(
        publisher.publish_operation(&operation),
        StreamingPublishOutcome::Published(())
    ));
    assert_eq!(state.with(Clone::clone).unwrap(), after_first_publish);
}

#[test]
fn stale_attempt_is_not_published() {
    let (_host, state) = mounted_publisher_state();
    assert_eq!(
        reduce_signal(&state, ChessEvent::Start),
        ChessReduction::Applied
    );
    let current = state
        .with(|state| state.current_attempt())
        .unwrap()
        .expect("model attempt is active");
    let stale = ModelAttemptKey {
        attempt_index: current.attempt_index + 1,
        ..current
    };
    let mut publisher = ChessActionPublisher {
        state: state.clone(),
        attempt: stale,
    };
    let operation = ChessActionPublication::new(ChessAction::Resign);

    assert!(matches!(
        publisher.publish_operation(&operation),
        StreamingPublishOutcome::NotPublished(ChessActionPublicationError::StaleAttempt)
    ));
    assert_eq!(
        operation.receipt.state(),
        ChessPublicationState::NotPublished
    );
    assert_eq!(
        state.with(|state| state.current_attempt()).unwrap(),
        Some(current)
    );
}
