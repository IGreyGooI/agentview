//! Named, typed questions over the ordinary Frame observation.
#![allow(
    clippy::disallowed_types,
    reason = "short synchronous staging; no guard crosses an await"
)]

use std::{
    collections::BTreeMap,
    convert::Infallible,
    fmt,
    future::Future,
    sync::{Arc, Mutex},
};

use super::{
    declaration::{Component, ComponentNode},
    handler::AsyncHandler,
};
use crate::component::prelude::*;
use crate::component::{
    execution::{
        ChoiceAnswer, DecisionAnswer, DecisionFailure, DecisionNoulCriteria, DecisionQuestion,
        DecisionSource, NoulAnswer, QuestionOutcome, ScoreAnswer,
    },
    EventContext,
};

mod sealed {
    pub trait Sealed {}
}

/// A supported question determines its answer type, independently of a Provider.
pub trait DecisionQuestionSpec: sealed::Sealed + Clone + Send + Sync + 'static {
    type Answer: Send + 'static;

    #[doc(hidden)]
    fn declaration(&self) -> DecisionQuestion;
    #[doc(hidden)]
    fn decode(answer: DecisionAnswer) -> Result<Self::Answer, DecisionFailure>;
}

#[derive(Debug, Clone)]
pub struct Noul {
    instructions: serde_json::Value,
    criteria: Option<DecisionNoulCriteria>,
}

impl Noul {
    pub fn new(instructions: impl Into<serde_json::Value>) -> Self {
        Self {
            instructions: instructions.into(),
            criteria: None,
        }
    }

    pub fn criteria(
        mut self,
        when_true: impl Into<serde_json::Value>,
        when_false: impl Into<serde_json::Value>,
    ) -> Self {
        self.criteria = Some(DecisionNoulCriteria {
            true_criterion: Some(when_true.into()),
            false_criterion: Some(when_false.into()),
        });
        self
    }
}

impl sealed::Sealed for Noul {}
impl DecisionQuestionSpec for Noul {
    type Answer = NoulAnswer;
    fn declaration(&self) -> DecisionQuestion {
        DecisionQuestion::Noul {
            instructions: self.instructions.clone(),
            criteria: self.criteria.clone(),
        }
    }
    fn decode(answer: DecisionAnswer) -> Result<Self::Answer, DecisionFailure> {
        match answer {
            DecisionAnswer::Noul { noul } => Ok(NoulAnswer { noul }),
            _ => Err(DecisionFailure::InvalidAnswer),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Choice {
    instructions: serde_json::Value,
    criteria: BTreeMap<String, serde_json::Value>,
}

impl Choice {
    pub fn new(
        instructions: impl Into<serde_json::Value>,
        criteria: BTreeMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            instructions: instructions.into(),
            criteria,
        }
    }
}

impl sealed::Sealed for Choice {}
impl DecisionQuestionSpec for Choice {
    type Answer = ChoiceAnswer;
    fn declaration(&self) -> DecisionQuestion {
        DecisionQuestion::Choice {
            instructions: self.instructions.clone(),
            criteria: self.criteria.clone(),
        }
    }
    fn decode(answer: DecisionAnswer) -> Result<Self::Answer, DecisionFailure> {
        match answer {
            DecisionAnswer::Choice {
                choice,
                probabilities,
                confidence,
            } => Ok(ChoiceAnswer {
                choice,
                probabilities,
                confidence,
            }),
            _ => Err(DecisionFailure::InvalidAnswer),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Score {
    instructions: serde_json::Value,
    criteria: Vec<serde_json::Value>,
}

impl Score {
    pub fn new(
        instructions: impl Into<serde_json::Value>,
        criteria: Vec<serde_json::Value>,
    ) -> Self {
        Self {
            instructions: instructions.into(),
            criteria,
        }
    }
}

impl sealed::Sealed for Score {}
impl DecisionQuestionSpec for Score {
    type Answer = ScoreAnswer;
    fn declaration(&self) -> DecisionQuestion {
        DecisionQuestion::Score {
            instructions: self.instructions.clone(),
            criteria: self.criteria.clone(),
        }
    }
    fn decode(answer: DecisionAnswer) -> Result<Self::Answer, DecisionFailure> {
        match answer {
            DecisionAnswer::Score {
                score,
                probabilities,
                legend,
                confidence,
            } => Ok(ScoreAnswer {
                score,
                probabilities,
                legend,
                confidence,
            }),
            _ => Err(DecisionFailure::InvalidAnswer),
        }
    }
}

/// Declares one named question; construction never starts a model request.
pub struct Decision;

impl Decision {
    pub fn props() -> DecisionProps {
        DecisionProps {
            name: (),
            question: (),
            callback: (),
        }
    }

    #[allow(clippy::new_ret_no_self)]
    pub fn new<Q: DecisionQuestionSpec>(
        name: impl Into<String>,
        question: Q,
    ) -> DecisionProps<String, Q> {
        Self::props().name(name).question(question)
    }
}

/// Required props may appear in any order in `view!`.
pub struct DecisionProps<Name = (), Question = (), Callback = ()> {
    name: Name,
    question: Question,
    callback: Callback,
}

impl<Name, Question, Callback> DecisionProps<Name, Question, Callback> {
    pub fn name(self, name: impl Into<String>) -> DecisionProps<String, Question, Callback> {
        DecisionProps {
            name: name.into(),
            question: self.question,
            callback: self.callback,
        }
    }

    pub fn question<Q: DecisionQuestionSpec>(
        self,
        question: Q,
    ) -> DecisionProps<Name, Q, Callback> {
        DecisionProps {
            name: self.name,
            question,
            callback: self.callback,
        }
    }
}

impl<Name, Question> DecisionProps<Name, Question> {
    pub fn on_result<T, Handler, Return, Mode>(
        self,
        mut handler: Handler,
    ) -> DecisionProps<Name, Question, DecisionCallback<T>>
    where
        T: Send + 'static,
        Handler: FnMut(EventContext, QuestionOutcome<T>) -> Return + Send + 'static,
        Return: DecisionCallbackReturn<Mode>,
    {
        DecisionProps {
            name: self.name,
            question: self.question,
            callback: DecisionCallback {
                handler: AsyncHandler::new(move |(context, outcome)| {
                    handler(context, outcome).into_decision_future()
                }),
            },
        }
    }
}

#[doc(hidden)]
pub struct DecisionCallback<T> {
    handler: AsyncHandler<(EventContext, QuestionOutcome<T>)>,
}

impl<Q, T> DecisionProps<String, Q, DecisionCallback<T>>
where
    Q: DecisionQuestionSpec<Answer = T>,
    T: Send + 'static,
{
    pub fn build(self) -> Component {
        let mut typed_handler = self.callback.handler;
        let handler = AsyncHandler::new(
            move |(context, outcome): (EventContext, QuestionOutcome<DecisionAnswer>)| {
                let outcome = QuestionOutcome {
                    result: outcome.result.and_then(Q::decode),
                    source: outcome.source,
                };
                let started = typed_handler.start((context, outcome));
                async move {
                    started
                        .map_err(|error| error.to_string())?
                        .await
                        .map_err(|error| error.to_string())
                }
            },
        );
        decision_component(DecisionViewProps {
            name: self.name,
            question: self.question.declaration(),
            handler: Arc::new(Mutex::new(handler)),
            pending: Arc::new(Mutex::new(None)),
        })
    }
}

#[doc(hidden)]
pub trait DecisionCallbackReturn<Mode>: Send + 'static {
    type Error: fmt::Display + Send + 'static;
    type Future: Future<Output = Result<(), Self::Error>> + Send + 'static;
    fn into_decision_future(self) -> Self::Future;
}

#[doc(hidden)]
pub enum SyncDecisionCallback {}
#[doc(hidden)]
pub enum AsyncDecisionCallback {}

impl<E: fmt::Display + Send + 'static> DecisionCallbackReturn<SyncDecisionCallback>
    for Result<(), E>
{
    type Error = E;
    type Future = std::future::Ready<Self>;
    fn into_decision_future(self) -> Self::Future {
        std::future::ready(self)
    }
}

impl<F, E> DecisionCallbackReturn<AsyncDecisionCallback> for F
where
    F: Future<Output = Result<(), E>> + Send + 'static,
    E: fmt::Display + Send + 'static,
{
    type Error = E;
    type Future = F;
    fn into_decision_future(self) -> Self::Future {
        self
    }
}

pub(crate) struct DecisionData {
    pub(crate) name: String,
    pub(crate) question: DecisionQuestion,
}

type PendingOutcome = Option<(EventContext, QuestionOutcome<DecisionAnswer>)>;
type DecisionHandler = AsyncHandler<(EventContext, QuestionOutcome<DecisionAnswer>)>;

#[derive(Clone)]
struct DecisionViewProps {
    name: String,
    question: DecisionQuestion,
    handler: Arc<Mutex<DecisionHandler>>,
    pending: Arc<Mutex<PendingOutcome>>,
}

#[component]
fn decision_component(props: DecisionViewProps) -> Component {
    let pending = props.pending.clone();
    let name = props.name.clone();
    use_provider_event_handler(ProviderEvent::DECISION, move |context, mut output| {
        let outcome = QuestionOutcome {
            result: output
                .outcomes
                .remove(&name)
                .unwrap_or(Err(DecisionFailure::MissingResult)),
            source: output.source,
        };
        let pending = pending.clone();
        async move {
            *pending.lock().unwrap_or_else(|p| p.into_inner()) = Some((context, outcome));
            Ok::<(), Infallible>(())
        }
    });
    let pending = props.pending;
    let handler = props.handler;
    use_reaction_completion(move |context| {
        let (context, outcome) = pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .filter(|(origin, _)| {
                origin.application_id() == context.application_id()
                    && origin.reaction_id() == context.reaction_id()
                    && origin.frame_revision() == context.frame_revision()
            })
            .unwrap_or((
                context,
                QuestionOutcome {
                    result: Err(DecisionFailure::MissingResult),
                    source: DecisionSource::Model,
                },
            ));
        let started = handler
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .start((context, outcome));
        async move {
            started
                .map_err(|error| error.to_string())?
                .await
                .map_err(|error| error.to_string())
        }
    });
    Component::from_node(ComponentNode::Decision(Box::new(DecisionData {
        name: props.name,
        question: props.question,
    })))
}
