//! Provider-neutral contracts and results for a single structured judgment.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

static NEXT_DECISION_BATCH: AtomicU64 = AtomicU64::new(1);

/// Runtime correlation identity for one submitted batch of questions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct DecisionBatchId(String);

impl DecisionBatchId {
    pub fn new(value: impl Into<String>) -> Result<Self, DecisionDeclarationError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(DecisionDeclarationError::EmptyBatchId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DecisionBatchId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DecisionBatchId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Explicit question contract shared by native Decisions and Responses models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecisionQuestion {
    Noul {
        #[serde(default)]
        instructions: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<DecisionNoulCriteria>,
    },
    Choice {
        #[serde(default)]
        instructions: Value,
        criteria: BTreeMap<String, Value>,
    },
    Score {
        #[serde(default)]
        instructions: Value,
        criteria: Vec<Value>,
    },
    Box {
        instructions: Value,
    },
}

/// Optional guidance for the true and false outcomes of a `noul` question.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionNoulCriteria {
    #[serde(
        rename = "true",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_value"
    )]
    pub true_criterion: Option<Value>,
    #[serde(
        rename = "false",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_value"
    )]
    pub false_criterion: Option<Value>,
}

fn deserialize_present_value<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

impl DecisionQuestion {
    pub fn noul(instructions: impl Into<Value>) -> Self {
        Self::Noul {
            instructions: instructions.into(),
            criteria: None,
        }
    }

    pub fn choice(instructions: impl Into<Value>, criteria: BTreeMap<String, Value>) -> Self {
        Self::Choice {
            instructions: instructions.into(),
            criteria,
        }
    }

    pub fn score(instructions: impl Into<Value>, criteria: Vec<Value>) -> Self {
        Self::Score {
            instructions: instructions.into(),
            criteria,
        }
    }

    pub fn bounding_box(instructions: impl Into<Value>) -> Self {
        Self::Box {
            instructions: instructions.into(),
        }
    }

    fn is_valid(&self) -> bool {
        match self {
            Self::Noul {
                instructions,
                criteria,
            } => {
                guidance_is_valid(instructions, true)
                    && criteria.as_ref().is_none_or(|criteria| {
                        [&criteria.true_criterion, &criteria.false_criterion]
                            .into_iter()
                            .all(|value| {
                                value
                                    .as_ref()
                                    .is_none_or(|value| guidance_is_valid(value, true))
                            })
                    })
            }
            Self::Choice {
                instructions,
                criteria,
            } => {
                guidance_is_valid(instructions, true)
                    && !criteria.is_empty()
                    && criteria.iter().all(|(key, value)| {
                        !key.trim().is_empty() && guidance_is_valid(value, true)
                    })
            }
            Self::Score {
                instructions,
                criteria,
            } => {
                guidance_is_valid(instructions, true)
                    && !criteria.is_empty()
                    && criteria.iter().all(|value| guidance_is_valid(value, false))
            }
            Self::Box { instructions } => guidance_is_valid(instructions, false),
        }
    }
}

fn guidance_is_valid(value: &Value, allow_null: bool) -> bool {
    matches!(value, Value::String(_) | Value::Object(_) | Value::Array(_))
        || (allow_null && value.is_null())
}

fn validate_questions(
    questions: &BTreeMap<String, DecisionQuestion>,
) -> Result<(), DecisionDeclarationError> {
    if questions.is_empty()
        || questions
            .iter()
            .any(|(key, question)| key.trim().is_empty() || !question.is_valid())
    {
        return Err(DecisionDeclarationError::InvalidQuestions);
    }
    Ok(())
}

/// Answers retain probabilities and refusals exactly as judgments, with no
/// application threshold, boolean conversion, or generated explanation.
pub type DecisionAnswers = BTreeMap<String, DecisionAnswer>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecisionAnswer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        probabilities: Option<BTreeMap<String, f64>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
    },
    Score {
        score: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        probabilities: Option<BTreeMap<String, f64>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        legend: Option<BTreeMap<String, Value>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
    },
    Box {
        #[serde(rename = "box")]
        bounds: DecisionBox,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
    },
    Refusal,
}

/// A box uses fractions of the image, with top <= bottom and left <= right.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionBox {
    pub top: f64,
    pub bottom: f64,
    pub left: f64,
    pub right: f64,
}

fn probability_is_valid(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn score_index_is_valid(key: &str, criteria: &[Value]) -> bool {
    key.parse::<usize>()
        .is_ok_and(|index| index < criteria.len() && index.to_string() == key)
}

fn answer_matches_question(question: &DecisionQuestion, answer: &DecisionAnswer) -> bool {
    match (question, answer) {
        (_, DecisionAnswer::Refusal) => true,
        (DecisionQuestion::Noul { .. }, DecisionAnswer::Noul { noul }) => {
            probability_is_valid(*noul)
        }
        (
            DecisionQuestion::Choice { criteria, .. },
            DecisionAnswer::Choice {
                choice,
                probabilities,
                confidence,
            },
        ) => {
            criteria.contains_key(choice)
                && confidence.is_none_or(probability_is_valid)
                && probabilities.as_ref().is_none_or(|probabilities| {
                    probabilities.iter().all(|(key, value)| {
                        criteria.contains_key(key) && probability_is_valid(*value)
                    })
                })
        }
        (
            DecisionQuestion::Score { criteria, .. },
            DecisionAnswer::Score {
                score,
                probabilities,
                legend,
                confidence,
            },
        ) => {
            score.is_finite()
                && (0.0..=(criteria.len() - 1) as f64).contains(score)
                && confidence.is_none_or(probability_is_valid)
                && probabilities.as_ref().is_none_or(|probabilities| {
                    probabilities.iter().all(|(key, value)| {
                        score_index_is_valid(key, criteria) && probability_is_valid(*value)
                    })
                })
                && legend.as_ref().is_none_or(|legend| {
                    legend.iter().all(|(key, value)| {
                        score_index_is_valid(key, criteria) && guidance_is_valid(value, false)
                    })
                })
        }
        (DecisionQuestion::Box { .. }, DecisionAnswer::Box { bounds, confidence }) => {
            [bounds.top, bounds.bottom, bounds.left, bounds.right]
                .into_iter()
                .all(probability_is_valid)
                && bounds.top <= bounds.bottom
                && bounds.left <= bounds.right
                && confidence.is_none_or(probability_is_valid)
        }
        _ => false,
    }
}

/// Typed probability returned by a `noul` question. Applications choose thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoulAnswer {
    pub noul: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChoiceAnswer {
    pub choice: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probabilities: Option<BTreeMap<String, f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

/// A continuous, probability-weighted position on the zero-based criteria scale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreAnswer {
    pub score: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probabilities: Option<BTreeMap<String, f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legend: Option<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

/// One question's result; application/reaction/frame provenance is in EventContext.
#[derive(Debug, Clone, PartialEq)]
pub struct QuestionOutcome<T> {
    pub result: Result<T, DecisionFailure>,
    pub source: DecisionSource,
}

/// Serializable batch contract. State is the Frame's effective observation;
/// the mounted callbacks stay outside this declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionDeclaration {
    batch_id: DecisionBatchId,
    questions: BTreeMap<String, DecisionQuestion>,
}

impl DecisionDeclaration {
    pub fn new(
        questions: BTreeMap<String, DecisionQuestion>,
    ) -> Result<Self, DecisionDeclarationError> {
        validate_questions(&questions)?;
        let identity = NEXT_DECISION_BATCH
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .expect("decision batch identity space exhausted");
        Ok(Self {
            batch_id: DecisionBatchId(format!("decision-batch-{identity}")),
            questions,
        })
    }

    pub fn batch_id(&self) -> &DecisionBatchId {
        &self.batch_id
    }
    pub fn questions(&self) -> &BTreeMap<String, DecisionQuestion> {
        &self.questions
    }
    pub fn validate(&self) -> Result<(), DecisionDeclarationError> {
        validate_questions(&self.questions)
    }

    pub(crate) fn merge(&mut self, other: Self) -> Result<(), String> {
        if let Some(name) = other
            .questions
            .keys()
            .find(|name| self.questions.contains_key(*name))
        {
            return Err(name.clone());
        }
        self.questions.extend(other.questions);
        Ok(())
    }

    pub(crate) fn admit(
        &self,
        decision: &ProviderDecision,
    ) -> Result<DecisionOutput, DecisionProtocolError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Envelope {
            answers: ParsedAnswers,
        }
        #[derive(Deserialize)]
        struct NativeEnvelope {
            #[serde(rename = "model")]
            _model: String,
            answers: ParsedAnswers,
            #[serde(rename = "usage")]
            _usage: NativeUsage,
        }
        #[derive(Deserialize)]
        struct NativeUsage {
            #[serde(rename = "input_tokens")]
            _input_tokens: u64,
            #[serde(rename = "output_tokens")]
            _output_tokens: Option<u64>,
        }
        let parsed = match &decision.payload {
            ProviderDecisionPayload::Refusal(_) => {
                return Ok(DecisionOutput {
                    batch_id: self.batch_id.clone(),
                    outcomes: self
                        .questions
                        .keys()
                        .map(|name| (name.clone(), Err(DecisionFailure::Refused)))
                        .collect(),
                    source: decision.source,
                });
            }
            ProviderDecisionPayload::JsonEnvelope(raw) => {
                serde_json::from_str::<Envelope>(raw).map(|envelope| envelope.answers)
            }
            ProviderDecisionPayload::NativeAnswers(raw) => {
                serde_json::from_str::<NativeEnvelope>(raw).map(|envelope| envelope.answers)
            }
        }
        .map_err(|error| {
            if error.is_data() {
                DecisionProtocolError::InvalidEnvelope
            } else {
                DecisionProtocolError::InvalidJson
            }
        })?;
        let mut answers = parsed.0;
        let mut outcomes = BTreeMap::new();
        for (name, question) in &self.questions {
            let outcome = match answers.remove(name) {
                None => Err(DecisionFailure::MissingResult),
                Some(None) => Err(DecisionFailure::InvalidAnswer),
                Some(Some(value)) => match serde_json::from_value::<DecisionAnswer>(value) {
                    Ok(DecisionAnswer::Refusal) => Err(DecisionFailure::Refused),
                    Ok(answer) if answer_matches_question(question, &answer) => Ok(answer),
                    _ => Err(DecisionFailure::InvalidAnswer),
                },
            };
            outcomes.insert(name.clone(), outcome);
        }
        outcomes.extend(
            answers
                .into_keys()
                .map(|name| (name, Err(DecisionFailure::UnknownQuestion))),
        );
        Ok(DecisionOutput {
            batch_id: self.batch_id.clone(),
            outcomes,
            source: decision.source,
        })
    }
}

/// A repeated question name invalidates only that question, leaving independent
/// answers available. A non-object answers envelope remains a protocol failure.
struct ParsedAnswers(BTreeMap<String, Option<Value>>);

impl<'de> Deserialize<'de> for ParsedAnswers {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AnswersVisitor;
        impl<'de> serde::de::Visitor<'de> for AnswersVisitor {
            type Value = ParsedAnswers;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object of named answers")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut answers = BTreeMap::new();
                while let Some((name, value)) = map.next_entry::<String, Value>()? {
                    match answers.entry(name) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(Some(value));
                        }
                        std::collections::btree_map::Entry::Occupied(mut entry) => {
                            entry.insert(None);
                        }
                    }
                }
                Ok(ParsedAnswers(answers))
            }
        }
        deserializer.deserialize_map(AnswersVisitor)
    }
}

impl<'de> Deserialize<'de> for DecisionDeclaration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Declaration {
            batch_id: DecisionBatchId,
            questions_json: String,
        }
        let value = Declaration::deserialize(deserializer)?;
        let questions =
            serde_json::from_str(&value.questions_json).map_err(serde::de::Error::custom)?;
        let declaration = Self {
            batch_id: value.batch_id,
            questions,
        };
        declaration.validate().map_err(serde::de::Error::custom)?;
        Ok(declaration)
    }
}

impl Serialize for DecisionDeclaration {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Frame JCS numbers use IEEE-754; retain arbitrary JSON guidance and
        // criterion integers exactly inside the typed canonical declaration.
        #[derive(Serialize)]
        struct Declaration<'a> {
            batch_id: &'a DecisionBatchId,
            questions_json: String,
        }
        Declaration {
            batch_id: &self.batch_id,
            questions_json: serde_json::to_string(&self.questions)
                .map_err(serde::ser::Error::custom)?,
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecisionDeclarationError {
    #[error("decision batch identity must be nonempty")]
    EmptyBatchId,
    #[error("decision names must be unique within the submitted Frame")]
    DuplicateNames,
    #[error("decision questions must have nonempty names and valid typed criteria")]
    InvalidQuestions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum DecisionProtocolError {
    #[error("decision response is not valid JSON")]
    InvalidJson,
    #[error("decision response has an invalid answers envelope")]
    InvalidEnvelope,
}

/// Normal per-question failures, distinct from reaction protocol/transport errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum DecisionFailure {
    #[error("the reaction completed without an answer to this question")]
    MissingResult,
    #[error("the answer does not satisfy this question's primitive and criteria")]
    InvalidAnswer,
    #[error("the model refused this question")]
    Refused,
    #[error("the answer names no declared question")]
    UnknownQuestion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    Model,
    Cache,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecisionOutput {
    pub batch_id: DecisionBatchId,
    pub outcomes: BTreeMap<String, Result<DecisionAnswer, DecisionFailure>>,
    pub source: DecisionSource,
}

/// A sealed, actual provider response awaiting shared per-question admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDecision {
    batch_id: DecisionBatchId,
    payload: ProviderDecisionPayload,
    source: DecisionSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "content", rename_all = "snake_case")]
enum ProviderDecisionPayload {
    JsonEnvelope(String),
    NativeAnswers(String),
    Refusal(String),
}

impl ProviderDecision {
    pub fn json_envelope(batch_id: DecisionBatchId, raw_json: impl Into<String>) -> Self {
        Self {
            batch_id,
            payload: ProviderDecisionPayload::JsonEnvelope(raw_json.into()),
            source: DecisionSource::Model,
        }
    }
    pub fn native_answers(batch_id: DecisionBatchId, raw_response_json: impl Into<String>) -> Self {
        Self {
            batch_id,
            payload: ProviderDecisionPayload::NativeAnswers(raw_response_json.into()),
            source: DecisionSource::Model,
        }
    }
    pub fn refused(batch_id: DecisionBatchId, refusal: impl Into<String>) -> Self {
        Self {
            batch_id,
            payload: ProviderDecisionPayload::Refusal(refusal.into()),
            source: DecisionSource::Model,
        }
    }
    pub fn batch_id(&self) -> &DecisionBatchId {
        &self.batch_id
    }
    pub fn is_enveloped(&self) -> bool {
        matches!(self.payload, ProviderDecisionPayload::JsonEnvelope(_))
    }
    pub fn is_native_answers(&self) -> bool {
        matches!(self.payload, ProviderDecisionPayload::NativeAnswers(_))
    }
    pub fn with_source(mut self, source: DecisionSource) -> Self {
        self.source = source;
        self
    }
    pub fn source(&self) -> DecisionSource {
        self.source
    }
    pub fn raw_json(&self) -> Option<&str> {
        match &self.payload {
            ProviderDecisionPayload::JsonEnvelope(raw)
            | ProviderDecisionPayload::NativeAnswers(raw) => Some(raw),
            ProviderDecisionPayload::Refusal(_) => None,
        }
    }
    pub fn refusal(&self) -> Option<&str> {
        match &self.payload {
            ProviderDecisionPayload::Refusal(refusal) => Some(refusal),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn declaration() -> DecisionDeclaration {
        DecisionDeclaration::new(BTreeMap::from([
            ("bug".into(), DecisionQuestion::noul("Is this a defect?")),
            (
                "team".into(),
                DecisionQuestion::choice(
                    "Who owns it?",
                    BTreeMap::from([
                        ("payments".into(), json!("Billing")),
                        ("account".into(), json!("Login")),
                    ]),
                ),
            ),
            (
                "urgency".into(),
                DecisionQuestion::score(
                    "How urgent?",
                    vec![json!("Low"), json!("Medium"), json!("High")],
                ),
            ),
            (
                "target".into(),
                DecisionQuestion::bounding_box("Locate the button."),
            ),
        ]))
        .unwrap()
    }

    fn answers() -> Value {
        json!({
            "bug":{"type":"noul","noul":0.96},
            "team":{"type":"choice","choice":"payments","confidence":0.75,"probabilities":{"payments":0.84,"account":0.16}},
            "urgency":{"type":"score","score":1.99,"confidence":0.99,"probabilities":{"0":0,"1":0.01,"2":0.99},"legend":{"0":"Low","1":"Medium","2":"High"}},
            "target":{"type":"box","box":{"top":0.1,"bottom":0.7,"left":0.2,"right":0.8},"confidence":0.9}
        })
    }

    fn native(answers: Value) -> Value {
        json!({"id":"native-id","model":"typesafe/jev","provider":"TypeSafe","answers":answers,
            "usage":{"input_tokens":476,"output_tokens":70,"cost":0.000019992,"input_tokens_details":{"cached_tokens":100}}})
    }

    #[test]
    fn question_catalogs_validate_guidance_names_and_typed_criteria() {
        assert_eq!(
            DecisionBatchId::new(" ").unwrap_err(),
            DecisionDeclarationError::EmptyBatchId
        );
        for questions in [
            BTreeMap::new(),
            BTreeMap::from([(" ".into(), DecisionQuestion::noul("Evidence"))]),
            BTreeMap::from([(
                "q".into(),
                DecisionQuestion::choice("Choose.", BTreeMap::new()),
            )]),
            BTreeMap::from([("q".into(), DecisionQuestion::score("Rate.", vec![]))]),
            BTreeMap::from([(
                "q".into(),
                DecisionQuestion::score("Rate.", vec![Value::Null]),
            )]),
            BTreeMap::from([("q".into(), DecisionQuestion::bounding_box(Value::Null))]),
            BTreeMap::from([("q".into(), DecisionQuestion::noul(json!(true)))]),
        ] {
            assert_eq!(
                DecisionDeclaration::new(questions).unwrap_err(),
                DecisionDeclarationError::InvalidQuestions
            );
        }
        assert_ne!(declaration().batch_id(), declaration().batch_id());
    }

    #[test]
    fn canonical_catalogs_and_actual_native_bodies_preserve_large_integer_guidance() {
        let declaration = DecisionDeclaration::new(BTreeMap::from([(
            "q".into(),
            DecisionQuestion::Noul {
                instructions: json!({"boundary":9007199254740993u64}),
                criteria: Some(DecisionNoulCriteria {
                    true_criterion: Some(json!({"limit":u64::MAX})),
                    false_criterion: Some(Value::Null),
                }),
            },
        )]))
        .unwrap();
        let bytes = serde_json_canonicalizer::to_vec(&declaration).unwrap();
        let restored: DecisionDeclaration = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored, declaration);
        let body = json!({"model":"model","answers":{"q":{"type":"noul","noul":0.98}},
            "usage":{"input_tokens":1,"output_tokens":1}, "metadata":{"limit":u64::MAX}});
        let decision =
            ProviderDecision::native_answers(declaration.batch_id().clone(), body.to_string());
        let bytes = serde_json_canonicalizer::to_vec(&decision).unwrap();
        let restored: ProviderDecision = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored, decision);
        assert_eq!(
            serde_json::from_str::<Value>(restored.raw_json().unwrap()).unwrap(),
            body
        );
    }

    #[test]
    fn both_protocols_admit_the_same_question_answers_without_probability_conversion() {
        let declaration = declaration();
        let answers = answers();
        let id = declaration.batch_id();
        let expected: DecisionAnswers = serde_json::from_value(answers.clone()).unwrap();
        for decision in [
            ProviderDecision::native_answers(id.clone(), native(answers.clone()).to_string()),
            ProviderDecision::json_envelope(id.clone(), json!({"answers":answers}).to_string()),
        ] {
            let admitted = declaration.admit(&decision).unwrap();
            assert_eq!(admitted.batch_id, *id);
            assert_eq!(admitted.source, DecisionSource::Model);
            for (name, answer) in &expected {
                assert_eq!(admitted.outcomes[name], Ok(answer.clone()));
            }
            assert_eq!(
                admitted.outcomes["bug"],
                Ok(DecisionAnswer::Noul { noul: 0.96 })
            );
        }
    }

    #[test]
    fn native_input_only_usage_admits_answers_without_rewriting_missing_counts() {
        let declaration = declaration();
        let mut body = native(answers());
        body["usage"] = json!({"input_tokens":81});
        let decision =
            ProviderDecision::native_answers(declaration.batch_id().clone(), body.to_string());
        let admitted = declaration.admit(&decision).unwrap();
        assert_eq!(
            admitted.outcomes["bug"],
            Ok(DecisionAnswer::Noul { noul: 0.96 })
        );
        assert_eq!(
            serde_json::from_str::<Value>(decision.raw_json().unwrap()).unwrap(),
            body
        );
        for output in [json!(-1), json!("0")] {
            body["usage"]["output_tokens"] = output;
            let invalid =
                ProviderDecision::native_answers(declaration.batch_id().clone(), body.to_string());
            assert_eq!(
                declaration.admit(&invalid).unwrap_err(),
                DecisionProtocolError::InvalidEnvelope
            );
        }
    }

    #[test]
    fn refusal_missing_invalid_and_unknown_answers_preserve_independent_valid_answers() {
        let declaration = declaration();
        let body = json!({"answers":{
            "bug":{"type":"noul","noul":0.96},
            "team":{"type":"refusal"},
            "urgency":{"type":"score","score":9},
            "undeclared":{"type":"noul","noul":0.9}
        }});
        let result = declaration
            .admit(&ProviderDecision::json_envelope(
                declaration.batch_id().clone(),
                body.to_string(),
            ))
            .unwrap();
        assert_eq!(
            result.outcomes["bug"],
            Ok(DecisionAnswer::Noul { noul: 0.96 })
        );
        assert_eq!(result.outcomes["team"], Err(DecisionFailure::Refused));
        assert_eq!(
            result.outcomes["urgency"],
            Err(DecisionFailure::InvalidAnswer)
        );
        assert_eq!(
            result.outcomes["target"],
            Err(DecisionFailure::MissingResult)
        );
        assert_eq!(
            result.outcomes["undeclared"],
            Err(DecisionFailure::UnknownQuestion)
        );
        let refused = declaration
            .admit(&ProviderDecision::refused(
                declaration.batch_id().clone(),
                "Actual provider refusal",
            ))
            .unwrap();
        assert_eq!(refused.outcomes.len(), 4);
        assert!(refused
            .outcomes
            .values()
            .all(|result| *result == Err(DecisionFailure::Refused)));
    }

    #[test]
    fn repeated_answer_names_invalidate_only_the_ambiguous_question() {
        let declaration = declaration();
        let raw = r#"{"answers":{"bug":{"type":"noul","noul":0.1},"bug":{"type":"noul","noul":0.9},"team":{"type":"choice","choice":"payments"}}}"#;
        let result = declaration
            .admit(&ProviderDecision::json_envelope(
                declaration.batch_id().clone(),
                raw,
            ))
            .unwrap();
        assert_eq!(result.outcomes["bug"], Err(DecisionFailure::InvalidAnswer));
        assert!(matches!(
            result.outcomes["team"],
            Ok(DecisionAnswer::Choice { .. })
        ));
        assert_eq!(
            result.outcomes["urgency"],
            Err(DecisionFailure::MissingResult)
        );
    }

    #[test]
    fn invalid_types_labels_ranges_and_distributions_are_per_question_failures() {
        let declaration = declaration();
        for (name, invalid) in [
            ("bug", json!({"type":"noul","noul":-0.1})),
            ("bug", json!({"type":"noul","noul":1.1})),
            ("bug", json!({"type":"noul","noul":true})),
            ("bug", json!({"type":"choice","choice":"payments"})),
            ("team", json!({"type":"choice","choice":"unknown"})),
            (
                "team",
                json!({"type":"choice","choice":"payments","probabilities":{"unknown":0.5}}),
            ),
            (
                "team",
                json!({"type":"choice","choice":"payments","probabilities":{"payments":1.1}}),
            ),
            (
                "team",
                json!({"type":"choice","choice":"payments","confidence":-0.1}),
            ),
            ("urgency", json!({"type":"score","score":-0.01})),
            ("urgency", json!({"type":"score","score":2.01})),
            (
                "urgency",
                json!({"type":"score","score":1,"probabilities":{"3":0.5}}),
            ),
            (
                "urgency",
                json!({"type":"score","score":1,"probabilities":{"01":0.5}}),
            ),
            (
                "urgency",
                json!({"type":"score","score":1,"legend":{"3":"Outside scale"}}),
            ),
            (
                "target",
                json!({"type":"box","box":{"top":0.8,"bottom":0.7,"left":0.2,"right":0.8}}),
            ),
            (
                "target",
                json!({"type":"box","box":{"top":0.1,"bottom":0.7,"left":0.9,"right":0.8}}),
            ),
            (
                "target",
                json!({"type":"box","box":{"top":0.1,"bottom":0.7,"left":0.2,"right":1.1}}),
            ),
        ] {
            let mut body = answers();
            body[name] = invalid;
            let result = declaration
                .admit(&ProviderDecision::json_envelope(
                    declaration.batch_id().clone(),
                    json!({"answers":body}).to_string(),
                ))
                .unwrap();
            assert_eq!(result.outcomes[name], Err(DecisionFailure::InvalidAnswer));
            assert_eq!(
                result
                    .outcomes
                    .values()
                    .filter(|result| result.is_ok())
                    .count(),
                3
            );
        }
        let body = json!({"answers":{"team":{"type":"choice","choice":"payments","probabilities":{"payments":0.8}},"urgency":{"type":"score","score":1.99}}});
        let result = declaration
            .admit(&ProviderDecision::json_envelope(
                declaration.batch_id().clone(),
                body.to_string(),
            ))
            .unwrap();
        assert!(result.outcomes["team"].is_ok());
        assert!(result.outcomes["urgency"].is_ok());
    }

    #[test]
    fn malformed_packages_are_protocol_failures_and_do_not_fabricate_question_outcomes() {
        let declaration = declaration();
        for raw in [
            "invalid",
            r#"{"answers":{"bug":{"type":"noul","noul":1e400}}}"#,
        ] {
            assert_eq!(
                declaration
                    .admit(&ProviderDecision::json_envelope(
                        declaration.batch_id().clone(),
                        raw
                    ))
                    .unwrap_err(),
                DecisionProtocolError::InvalidJson
            );
        }
        for raw in [
            r#"{}"#,
            r#"{"answers":null}"#,
            r#"{"answers":[]}"#,
            r#"{"result":{}}"#,
            r#"{"answers":{},"answers":{}}"#,
        ] {
            assert_eq!(
                declaration
                    .admit(&ProviderDecision::json_envelope(
                        declaration.batch_id().clone(),
                        raw
                    ))
                    .unwrap_err(),
                DecisionProtocolError::InvalidEnvelope
            );
        }
        assert_eq!(
            declaration
                .admit(&ProviderDecision::native_answers(
                    declaration.batch_id().clone(),
                    r#"{"answers":{}}"#
                ))
                .unwrap_err(),
            DecisionProtocolError::InvalidEnvelope
        );
    }

    #[test]
    fn canonical_answer_facts_require_a_preceding_matching_batch_and_are_unique_per_submission() {
        use crate::transcript::{
            CanonicalInputItem, CanonicalTranscript, CanonicalTranscriptError,
        };
        let declaration = declaration();
        let request = CanonicalInputItem::DecisionRequest {
            declaration: declaration.clone(),
        };
        let output = CanonicalInputItem::DecisionOutput {
            decision: ProviderDecision::json_envelope(
                declaration.batch_id().clone(),
                json!({"answers":answers()}).to_string(),
            ),
        };
        assert_eq!(
            CanonicalTranscript::new()
                .appended(output.clone())
                .unwrap_err(),
            CanonicalTranscriptError::UnknownDecisionRequest
        );
        let transcript = CanonicalTranscript::new()
            .appended(request.clone())
            .unwrap()
            .appended(output.clone())
            .unwrap();
        assert_eq!(
            transcript.appended(output.clone()).unwrap_err(),
            CanonicalTranscriptError::DuplicateDecisionOutput
        );
        let transcript = transcript
            .appended(request)
            .unwrap()
            .appended(output)
            .unwrap();
        let restored: CanonicalTranscript =
            serde_json::from_value(serde_json::to_value(&transcript).unwrap()).unwrap();
        assert_eq!(restored, transcript);
    }
}
