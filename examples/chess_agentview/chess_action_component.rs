use std::{convert::Infallible, str::FromStr, sync::Mutex};

use agentview::component::prelude::*;
use async_trait::async_trait;
use chess::ChessMove;
use quick_xml::{events::Event, reader::Reader, XmlVersion};

use super::{
    application_state::{ChessEvent, ChessReduction, ChessState, ModelAttemptKey},
    chess_action::{ChessAction, ChessActionKind, InvalidActionReason},
    chess_application::reduce_signal,
    uci::{parse_strict_uci_move, StrictUciMoveError},
};

const ACTION_CONTRACT_VERSION: &str = "v1";
const ACTION_CONTRACT_ID: &str = "chess.action";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StrictUciMove(ChessMove);

impl FromStr for StrictUciMove {
    type Err = StrictUciMoveError;

    fn from_str(candidate: &str) -> Result<Self, Self::Err> {
        parse_strict_uci_move(candidate).map(Self)
    }
}

struct ChessActionChannels;

impl StreamingToolChannels for ChessActionChannels {
    type Output = ChessAction;
    type Live = NoStreamingValue;
    type Commit = NoStreamingValue;
    type Diagnostic = InvalidActionReason;
}

#[derive(Default)]
struct ChessActionAttempt {
    thought_completed: bool,
    action: Option<ChessAction>,
}

struct ChessPublicationReceipt(Mutex<ChessPublicationState>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChessPublicationState {
    Pending,
    Published,
    NotPublished,
}

impl ChessPublicationReceipt {
    fn pending() -> Self {
        Self(Mutex::new(ChessPublicationState::Pending))
    }

    fn record_published(&self) {
        let mut state = self.0.lock().unwrap();
        if *state == ChessPublicationState::Pending {
            *state = ChessPublicationState::Published;
        }
    }

    fn record_not_published(&self) {
        let mut state = self.0.lock().unwrap();
        if *state == ChessPublicationState::Pending {
            *state = ChessPublicationState::NotPublished;
        }
    }

    fn state(&self) -> ChessPublicationState {
        *self.0.lock().unwrap()
    }
}

#[derive(Debug, thiserror::Error)]
enum ChessActionPublicationError {
    #[error("the accepted Chess contract did not contain exactly one action")]
    InvalidAcceptedAttempt,
    #[error("the Chess state did not consume the model action for this attempt")]
    StaleAttempt,
}

struct ChessActionPublisher {
    state: Signal<ChessState>,
    attempt: ModelAttemptKey,
}

struct ChessActionPublication {
    action: ChessAction,
    receipt: ChessPublicationReceipt,
}

impl ChessActionPublication {
    fn new(action: ChessAction) -> Self {
        Self {
            action,
            receipt: ChessPublicationReceipt::pending(),
        }
    }
}

impl ChessActionPublisher {
    fn publish_operation(
        &mut self,
        operation: &ChessActionPublication,
    ) -> StreamingPublishOutcome<(), ChessActionPublicationError> {
        match operation.receipt.state() {
            ChessPublicationState::Published => return StreamingPublishOutcome::Published(()),
            ChessPublicationState::NotPublished => {
                return StreamingPublishOutcome::NotPublished(
                    ChessActionPublicationError::StaleAttempt,
                );
            }
            ChessPublicationState::Pending => {}
        }

        match apply_model_action(&self.state, self.attempt, Ok(operation.action)) {
            ChessReduction::Ignored => {
                operation.receipt.record_not_published();
                StreamingPublishOutcome::NotPublished(ChessActionPublicationError::StaleAttempt)
            }
            ChessReduction::Applied => {
                // The reducer synchronously consumed the action and advanced application state.
                // There is no actor or engine acknowledgement between this boundary and confirmation.
                operation.receipt.record_published();
                StreamingPublishOutcome::Published(())
            }
        }
    }
}

#[async_trait]
impl StreamingToolAttemptPublisher<ChessActionChannels> for ChessActionPublisher {
    type Published = ();
    type PublicationOperation = ChessActionPublication;
    type Error = ChessActionPublicationError;

    fn prepare(
        &mut self,
        _: &StreamingPublishContext,
        attempt: AcceptedStreamingToolAttempt<ChessActionChannels>,
    ) -> Result<Self::PublicationOperation, Self::Error> {
        let mut action = None;
        for entry in attempt.entries {
            match entry.value {
                StagedStreamingToolValue::Output(candidate) => {
                    if action.replace(candidate).is_some() {
                        return Err(ChessActionPublicationError::InvalidAcceptedAttempt);
                    }
                }
                StagedStreamingToolValue::Commit(never) => match never {},
            }
        }
        let action = action.ok_or(ChessActionPublicationError::InvalidAcceptedAttempt)?;
        Ok(ChessActionPublication::new(action))
    }

    async fn publish(
        &mut self,
        _: &StreamingPublishContext,
        operation: &Self::PublicationOperation,
    ) -> StreamingPublishOutcome<Self::Published, Self::Error> {
        self.publish_operation(operation)
    }

    async fn resolve(
        &mut self,
        _: &StreamingPublishRecoveryContext,
        operation: &Self::PublicationOperation,
    ) -> Result<StreamingPublishResolution<Self::Published>, Self::Error> {
        Ok(match operation.receipt.state() {
            ChessPublicationState::Published => StreamingPublishResolution::Published(()),
            ChessPublicationState::NotPublished => StreamingPublishResolution::NotPublished,
            ChessPublicationState::Pending => StreamingPublishResolution::StillIndeterminate,
        })
    }
}

fn apply_model_action(
    state: &Signal<ChessState>,
    attempt: ModelAttemptKey,
    result: Result<ChessAction, InvalidActionReason>,
) -> ChessReduction {
    reduce_signal(state, ChessEvent::ModelAction { attempt, result })
}

fn thought_contract() -> XmlElementContract<(), String> {
    XmlToolElement::text("thought")
        .occurs(XmlCardinality::exactly(1))
        .decode(|_| Ok(()), |_, text| Ok(text.to_owned()))
}

fn configure_action<Head: Send + Sync + 'static>(
    handlers: XmlElementHandlers<ChessActionAttempt, ChessActionChannels, Head, ChessAction>,
) -> XmlElementHandlers<ChessActionAttempt, ChessActionChannels, Head, ChessAction, ReadyCompletion>
{
    handlers.on_complete_validated(
        |state, _| {
            if state.thought_completed {
                XmlOccurrenceValidity::Valid
            } else {
                XmlOccurrenceValidity::Invalid(XmlOccurrenceRejection::diagnostic(
                    InvalidActionReason::ThoughtAfterAction,
                ))
            }
        },
        |state, event| {
            state.action = Some(event.value);
            StreamingToolUpdate::none()
        },
    )
}

fn choose_move_contract() -> XmlElementContract<StrictUciMove, ChessAction> {
    XmlToolElement::self_closing("choose_move")
        .required_attribute::<StrictUciMove>("uci", "...")
        .decode(
            |mut attributes| attributes.take_required::<StrictUciMove>("uci"),
            |head, _| Ok(ChessAction::ChooseMove(head.0)),
        )
}

fn resign_contract() -> XmlElementContract<(), ChessAction> {
    XmlToolElement::self_closing("resign").decode(
        |_| Ok::<(), XmlDecodeViolation>(()),
        |_, _| Ok::<ChessAction, XmlDecodeViolation>(ChessAction::Resign),
    )
}

fn decide_action(
    state: ChessActionAttempt,
    summary: XmlAttemptSummary<'_, InvalidActionReason>,
) -> StreamingToolDecision<ChessActionChannels> {
    if let Some(reason) = thought_problem(&state, &summary) {
        return StreamingToolDecision::Reject(StreamingToolRejection::diagnostic(reason));
    }
    let result = match registered_action_count(&summary) {
        0 => Err(InvalidActionReason::MissingAction),
        1 if state.action.is_some() && summary.diagnostics.is_empty() => {
            Ok(state.action.expect("one decoded Chess action"))
        }
        1 => Err(invalid_uci_reason(&summary).unwrap_or(InvalidActionReason::InvalidXml)),
        _ => Err(InvalidActionReason::MultipleActions),
    };
    match result {
        Ok(action) => StreamingToolDecision::Accept(StreamingToolUpdate::output(action)),
        Err(reason) => StreamingToolDecision::Reject(StreamingToolRejection::diagnostic(reason)),
    }
}

fn registered_action_count(summary: &XmlAttemptSummary<'_, InvalidActionReason>) -> usize {
    summary
        .elements
        .iter()
        .filter(|element| matches!(element.name, "choose_move" | "resign"))
        .map(|element| element.seen)
        .sum()
}

fn thought_problem(
    state: &ChessActionAttempt,
    summary: &XmlAttemptSummary<'_, InvalidActionReason>,
) -> Option<InvalidActionReason> {
    let Some(thought) = summary
        .elements
        .iter()
        .find(|element| element.name == "thought")
    else {
        return Some(InvalidActionReason::MissingThought);
    };
    match thought.seen {
        0 => return Some(InvalidActionReason::MissingThought),
        1 => {}
        _ => return Some(InvalidActionReason::MultipleThoughts),
    }
    if thought.completed != 1 || !state.thought_completed {
        return Some(InvalidActionReason::InvalidThought);
    }
    summary
        .diagnostics
        .iter()
        .find_map(|record| match &record.diagnostic {
            StreamingToolDiagnostic::Domain(InvalidActionReason::ThoughtAfterAction) => {
                Some(InvalidActionReason::ThoughtAfterAction)
            }
            _ => None,
        })
}

fn invalid_uci_reason(
    summary: &XmlAttemptSummary<'_, InvalidActionReason>,
) -> Option<InvalidActionReason> {
    summary.diagnostics.iter().find_map(|record| {
        if !matches!(
            record.diagnostic,
            StreamingToolDiagnostic::Decode(XmlDecodeViolation::Model {
                code: "invalid_attribute",
                ..
            })
        ) {
            return None;
        }
        let StreamingToolDiagnosticOrigin::Parser {
            span: Some(span), ..
        } = &record.origin
        else {
            return None;
        };
        let opening = span.slice(summary.raw_output)?;
        extract_uci_attribute(opening).map(|submitted| {
            InvalidActionReason::invalid_uci(ChessActionKind::ChooseMove, &submitted)
        })
    })
}

fn extract_uci_attribute(opening: &str) -> Option<String> {
    let mut reader = Reader::from_str(opening);
    let event = reader.read_event().ok()?;
    let element = match event {
        Event::Empty(element) | Event::Start(element) => element,
        _ => return None,
    };
    if element.name().as_ref() != b"choose_move" {
        return None;
    }
    element
        .attributes()
        .with_checks(false)
        .find_map(|attribute| {
            let attribute = attribute.ok()?;
            (attribute.key.as_ref() == b"uci").then_some(attribute)
        })?
        .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
        .ok()
        .map(|value| value.into_owned())
}

fn rejection_reason(
    report: &RejectedStreamingToolAttempt<InvalidActionReason>,
) -> InvalidActionReason {
    report
        .diagnostics
        .iter()
        .rev()
        .find_map(|record| match &record.diagnostic {
            StreamingToolDiagnostic::Domain(reason) => Some(reason.clone()),
            StreamingToolDiagnostic::Contract(_) | StreamingToolDiagnostic::Decode(_) => None,
        })
        .unwrap_or(InvalidActionReason::InvalidXml)
}

#[component]
pub(crate) fn chess_action_component(
    attempt: ModelAttemptKey,
    state: Signal<ChessState>,
) -> Component {
    let publisher_state = state.clone();
    let rejection_state = state;
    XmlStreamingToolCall::new::<ChessActionChannels>(ACTION_CONTRACT_ID)
        .version(ACTION_CONTRACT_VERSION)
        .state_with(|_| Ok::<_, Infallible>(ChessActionAttempt::default()))
        .element(thought_contract(), |handlers| {
            handlers.on_complete_validated(
                |_, event| {
                    if event.value.trim().is_empty() {
                        XmlOccurrenceValidity::Invalid(XmlOccurrenceRejection::diagnostic(
                            InvalidActionReason::InvalidThought,
                        ))
                    } else {
                        XmlOccurrenceValidity::Valid
                    }
                },
                |state, _| {
                    state.thought_completed = true;
                    StreamingToolUpdate::none()
                },
            )
        })
        .element(choose_move_contract(), configure_action)
        .element(resign_contract(), configure_action)
        .finish(decide_action)
        .without_live()
        .publish_with(move |_| {
            Ok::<_, Infallible>(ChessActionPublisher {
                state: publisher_state.clone(),
                attempt,
            })
        })
        .on_rejected(move |report| {
            let state = rejection_state.clone();
            let reason = rejection_reason(&report);
            async move {
                let _ = apply_model_action(&state, attempt, Err(reason));
                Ok::<_, Infallible>(StreamingToolRejectionAction::Complete)
            }
        })
        .build()
}

#[cfg(test)]
#[path = "../../tests/examples/chess_agentview/chess_action_component.rs"]
mod tests;
