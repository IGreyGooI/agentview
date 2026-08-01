//! Example-only chess AgentView support.
//!
//! This is deliberately not part of the `agentview` library API. It is a demo
//! VM that exercises observe/act/hook against a Stockfish-compatible UCI engine.

use std::process::Stdio;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentview::{
    component::{ExternalActionRoute, ExternalReplyContract, ExternalReplyContractId},
    prelude::*,
};
use chess::{Board, BoardStatus, ChessMove, Color, MoveGen, Piece, Square, ALL_FILES, ALL_RANKS};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

const CHESS_SYSTEM_TASK: &str = "You are choosing legal chess moves from the rendered board.";
const CHESS_REASONING_PRIVATE: &str = "Think privately about candidate moves before acting.";
const CHESS_REASONING_NO_COT: &str =
    "Do not print chain-of-thought; return only the XML move after deciding.";

/// Typed semantic result shared by the CLI and provider bindings.
///
/// The action is syntactically canonical UCI only. The host must still check
/// turn phase, source revision, authorization, and board legality in its
/// commit transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChessAction {
    pub uci: String,
}

impl ChessAction {
    pub fn from_uci(uci: impl AsRef<str>) -> Result<Self, ChessDiagnostic> {
        let uci = uci.as_ref().trim();
        let chess_move = ChessMove::from_str(uci).map_err(|error| ChessDiagnostic::InvalidUci {
            uci: uci.to_owned(),
            message: error.to_string(),
        })?;
        Ok(Self {
            uci: chess_move.to_string(),
        })
    }

    pub fn uci(&self) -> &str {
        &self.uci
    }
}

/// Typed, non-terminal diagnostics emitted by the shared chess reply
/// contract. They describe syntax/contract failures only, never a domain
/// authorization decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum ChessDiagnostic {
    #[error("expected a text XML chess move reply")]
    ExpectedTextReply,

    #[error("expected exactly one `<move uci=\"...\" />` reply")]
    InvalidXmlEnvelope,

    #[error("expected `<move>`, got `<{tag}>")]
    UnexpectedTag { tag: String },

    #[error("move is missing a `uci` attribute")]
    MissingUci,

    #[error("move has unexpected attribute `{attribute}`")]
    UnexpectedAttribute { attribute: String },

    #[error("move must not contain text content")]
    UnexpectedContent,

    #[error("invalid UCI move `{uci}`: {message}")]
    InvalidUci { uci: String, message: String },

    #[error("no move was selected")]
    NoMoveSelected,

    #[error("invalid chess CLI command: {message}")]
    InvalidCliCommand { message: String },
}

/// The single reply contract for the chess examples.
///
/// Its POM projection, external text decoder, and provider streaming reducer
/// all use the same tag, attribute, action, and diagnostic definitions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChessMoveContract;

#[allow(dead_code)]
impl ChessMoveContract {
    pub const TAG: &'static str = "move";
    pub const UCI_ATTRIBUTE: &'static str = "uci";
    pub const UCI_PLACEHOLDER: &'static str = "...";

    pub fn encode_reply(&self, action: &ChessAction) -> String {
        format!(
            "<{} {}=\"{}\" />",
            Self::TAG,
            Self::UCI_ATTRIBUTE,
            action.uci()
        )
    }

    /// Decode a parsed provider element. This is deliberately pure so a
    /// streaming reducer can emit a preview immediately and leave all host
    /// mutation to its effect/commit boundary.
    pub fn decode_element(
        &self,
        element: &agentview::stream_parser::XmlElement,
    ) -> Result<ChessAction, ChessDiagnostic> {
        self.decode_parts(
            &element.tag_name,
            &element.content,
            element
                .attributes
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
    }

    /// Decode the XML reply delivered by an external controller.
    ///
    /// This accepts the same empty `<move>` element as the streaming parser:
    /// either self-closing or an explicit empty close tag. It intentionally
    /// rejects prose, multiple elements, and non-contract attributes.
    pub fn decode_reply(&self, reply: &str) -> Result<ChessAction, ChessDiagnostic> {
        let reply = reply.trim();
        let after_open = reply
            .strip_prefix('<')
            .ok_or(ChessDiagnostic::InvalidXmlEnvelope)?;
        let after_tag = after_open
            .strip_prefix(Self::TAG)
            .ok_or(ChessDiagnostic::InvalidXmlEnvelope)?;
        if !matches!(
            after_tag.as_bytes().first(),
            Some(b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>')
        ) {
            return Err(ChessDiagnostic::InvalidXmlEnvelope);
        }

        let opening_end = find_xml_tag_end(after_tag).ok_or(ChessDiagnostic::InvalidXmlEnvelope)?;
        let opening = &after_tag[..opening_end];
        let tail = &after_tag[opening_end + 1..];
        let opening = opening.trim_end();
        let (attributes, self_closing) = match opening.strip_suffix('/') {
            Some(attributes) => (attributes, true),
            None => (opening, false),
        };

        if self_closing {
            if !tail.trim().is_empty() {
                return Err(ChessDiagnostic::InvalidXmlEnvelope);
            }
        } else if tail.trim() != format!("</{}>", Self::TAG) {
            return Err(ChessDiagnostic::InvalidXmlEnvelope);
        }

        self.decode_parts(
            Self::TAG,
            "",
            parse_xml_attributes(attributes)?
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
    }

    fn decode_parts<'a>(
        &self,
        tag: &str,
        content: &str,
        attributes: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> Result<ChessAction, ChessDiagnostic> {
        if tag != Self::TAG {
            return Err(ChessDiagnostic::UnexpectedTag {
                tag: tag.to_owned(),
            });
        }
        if !content.trim().is_empty() {
            return Err(ChessDiagnostic::UnexpectedContent);
        }

        let attributes = attributes.collect::<Vec<_>>();
        let Some((name, uci)) = attributes.first().copied() else {
            return Err(ChessDiagnostic::MissingUci);
        };
        if name != Self::UCI_ATTRIBUTE {
            return Err(ChessDiagnostic::UnexpectedAttribute {
                attribute: name.to_owned(),
            });
        }
        if attributes.len() != 1 {
            return Err(ChessDiagnostic::UnexpectedAttribute {
                attribute: attributes[1].0.to_owned(),
            });
        }

        ChessAction::from_uci(uci)
    }
}

impl AgentView for ChessMoveContract {
    type Root = XmlNode;

    fn build_root(&self) -> Result<Self::Root, PomError> {
        let mut node = XmlNode::new(XmlName::try_from(Self::TAG)?);
        node.push_attribute(
            XmlName::try_from(Self::UCI_ATTRIBUTE)?,
            Self::UCI_PLACEHOLDER,
        )?;
        Ok(node)
    }
}

/// System-only typed POM contribution for the shared semantic reply contract.
#[derive(Debug, Clone, PartialEq, Eq, AgentView)]
#[agent_view(document)]
#[allow(dead_code)]
pub struct ChessReplyContractDocument {
    #[view(xml)]
    reply_contract: ChessMoveContract,
}

impl Default for ChessReplyContractDocument {
    fn default() -> Self {
        Self {
            reply_contract: ChessMoveContract,
        }
    }
}

/// Binds the shared XML semantics to the external-controller reply ingress.
#[derive(Clone)]
#[allow(dead_code)]
pub struct ChessReplyContract {
    semantic: ChessMoveContract,
    route: ExternalActionRoute,
    id: ExternalReplyContractId,
}

#[allow(dead_code)]
impl ChessReplyContract {
    pub fn new() -> Self {
        Self {
            semantic: ChessMoveContract,
            route: ExternalActionRoute::new("chess.player_move").unwrap(),
            id: ExternalReplyContractId::new("chess.player_move.reply/v2").unwrap(),
        }
    }
}

impl Default for ChessReplyContract {
    fn default() -> Self {
        Self::new()
    }
}

impl ExternalReplyContract for ChessReplyContract {
    type Action = ChessAction;
    type Diagnostic = ChessDiagnostic;
    type System = ChessReplyContractDocument;

    fn system(&self) -> Self::System {
        ChessReplyContractDocument::default()
    }

    fn route(&self) -> &ExternalActionRoute {
        &self.route
    }

    fn contract_id(&self) -> &ExternalReplyContractId {
        &self.id
    }

    fn decode(&self, reply: &ControlReply) -> Result<Self::Action, Self::Diagnostic> {
        match reply {
            ControlReply::Text(reply) => self.semantic.decode_reply(reply),
            ControlReply::Structured(_) => Err(ChessDiagnostic::ExpectedTextReply),
        }
    }
}

/// Host-side CLI envelope for the chess skill.
///
/// This is not a prompt grammar. It validates a user-facing command and
/// translates it to the shared semantic action before an external controller
/// sees the canonical `<move uci="..." />` reply.
#[allow(dead_code)]
pub struct ChessCliTransport;

#[allow(dead_code)]
impl ChessCliTransport {
    pub fn decode(command: &str) -> Result<ChessAction, ChessDiagnostic> {
        let tokens = command.split_whitespace().collect::<Vec<_>>();
        if tokens.get(..3) != Some(["agentview", "chess", "act"].as_slice()) {
            return Err(Self::invalid(
                "expected `agentview chess act` command prefix",
            ));
        }

        let mut cursor = 3;
        let piece = Self::command_value(&tokens, &mut cursor, "--piece")?;
        let from = Self::command_value(&tokens, &mut cursor, "--from")?;
        let to = Self::command_value(&tokens, &mut cursor, "--to")?;
        let promotion = if tokens.get(cursor) == Some(&"--promotion") {
            Some(Self::command_value(&tokens, &mut cursor, "--promotion")?)
        } else {
            None
        };
        let uci = Self::command_value(&tokens, &mut cursor, "--uci")?;
        if cursor != tokens.len() {
            return Err(Self::invalid(format!(
                "unexpected argument `{}` after `--uci <uci>`",
                tokens[cursor]
            )));
        }
        if !matches!(piece, "P" | "N" | "B" | "R" | "Q" | "K") {
            return Err(Self::invalid(format!(
                "expected `--piece` to be one of P, N, B, R, Q, or K; got `{piece}`"
            )));
        }

        let action = ChessAction::from_uci(uci)?;
        let canonical_from = &action.uci[..2];
        let canonical_to = &action.uci[2..4];
        if from != canonical_from {
            return Err(Self::invalid(format!(
                "`--from {from}` does not match UCI source `{canonical_from}`"
            )));
        }
        if to != canonical_to {
            return Err(Self::invalid(format!(
                "`--to {to}` does not match UCI target `{canonical_to}`"
            )));
        }

        match (promotion, action.uci.as_bytes().get(4).copied()) {
            (None, None) => {}
            (Some(promotion), Some(uci_promotion)) if promotion.as_bytes() == [uci_promotion] => {}
            (None, Some(uci_promotion)) => {
                return Err(Self::invalid(format!(
                    "promotion UCI `{}` requires `--promotion {}`",
                    action.uci,
                    char::from(uci_promotion)
                )));
            }
            (Some(promotion), None) => {
                return Err(Self::invalid(format!(
                    "`--promotion {promotion}` requires a promotion UCI move"
                )));
            }
            (Some(promotion), Some(uci_promotion)) => {
                return Err(Self::invalid(format!(
                    "`--promotion {promotion}` does not match UCI promotion `{}`",
                    char::from(uci_promotion)
                )));
            }
        }

        Ok(action)
    }

    fn command_value<'a>(
        tokens: &'a [&'a str],
        cursor: &mut usize,
        expected_flag: &str,
    ) -> Result<&'a str, ChessDiagnostic> {
        let flag = tokens
            .get(*cursor)
            .ok_or_else(|| Self::invalid(format!("expected `{expected_flag} <value>`")))?;
        if *flag != expected_flag {
            return Err(Self::invalid(format!(
                "expected `{expected_flag}`, got `{flag}`"
            )));
        }
        *cursor += 1;
        let value = tokens
            .get(*cursor)
            .ok_or_else(|| Self::invalid(format!("missing value after `{expected_flag}`")))?;
        if value.is_empty() || value.starts_with("--") {
            return Err(Self::invalid(format!(
                "expected a value after `{expected_flag}`"
            )));
        }
        *cursor += 1;
        Ok(value)
    }

    fn invalid(message: impl Into<String>) -> ChessDiagnostic {
        ChessDiagnostic::InvalidCliCommand {
            message: message.into(),
        }
    }
}

#[allow(dead_code)]
fn find_xml_tag_end(input: &str) -> Option<usize> {
    let mut quote = None;
    for (index, character) in input.char_indices() {
        match (quote, character) {
            (Some(active), character) if character == active => quote = None,
            (Some(_), _) => {}
            (None, '\"' | '\'') => quote = Some(character),
            (None, '>') => return Some(index),
            (None, _) => {}
        }
    }
    None
}

#[allow(dead_code)]
fn parse_xml_attributes(input: &str) -> Result<Vec<(String, String)>, ChessDiagnostic> {
    let mut input = input.trim();
    let mut attributes = Vec::new();
    while !input.is_empty() {
        let name_length = input
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
            .count();
        if name_length == 0 {
            return Err(ChessDiagnostic::InvalidXmlEnvelope);
        }
        let name = &input[..name_length];
        input = input[name_length..].trim_start();
        input = input
            .strip_prefix('=')
            .ok_or(ChessDiagnostic::InvalidXmlEnvelope)?
            .trim_start();
        let quote = input
            .chars()
            .next()
            .filter(|quote| matches!(quote, '\"' | '\''))
            .ok_or(ChessDiagnostic::InvalidXmlEnvelope)?;
        input = &input[quote.len_utf8()..];
        let end = input
            .find(quote)
            .ok_or(ChessDiagnostic::InvalidXmlEnvelope)?;
        let value = &input[..end];
        input = input[end + quote.len_utf8()..].trim_start();
        if attributes.iter().any(|(existing, _)| existing == name) {
            return Err(ChessDiagnostic::InvalidXmlEnvelope);
        }
        attributes.push((name.to_owned(), value.to_owned()));
    }
    Ok(attributes)
}

/// Shared chess game source used by the example applications.
#[derive(Debug, Clone)]
pub struct ChessGameSource {
    inner: Arc<Mutex<ChessGameState>>,
}

impl ChessGameSource {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ChessGameState::new())),
        }
    }

    pub fn snapshot(&self) -> ChessGameState {
        self.inner.lock().unwrap().clone()
    }

    /// Validate one player move against the current authoritative board
    /// without mutating it. External hosts use this during reply preparation
    /// before they persist a commit candidate.
    pub fn validate_legal_uci(&self, uci: &str) -> anyhow::Result<()> {
        let chess_move = ChessMove::from_str(uci)
            .map_err(|error| anyhow::anyhow!("invalid UCI move `{uci}`: {error}"))?;
        if !self.inner.lock().unwrap().board.legal(chess_move) {
            anyhow::bail!("illegal chess move `{uci}` for current board");
        }
        Ok(())
    }

    /// Apply a legal UCI move as an example-domain action.
    ///
    /// The mounted prompt example uses this to create its next captured board
    /// snapshot. A real host receives a typed agent result, validates it at
    /// its own action boundary, and performs the same kind of domain update.
    #[allow(dead_code)] // Used by the separately compiled mounted example.
    pub fn apply_legal_uci(&self, uci: &str) -> anyhow::Result<()> {
        self.validate_legal_uci(uci)?;
        let chess_move = ChessMove::from_str(uci)
            .map_err(|error| anyhow::anyhow!("invalid UCI move `{uci}`: {error}"))?;
        self.with_state(|state| {
            if !state.board.legal(chess_move) {
                anyhow::bail!("illegal chess move `{uci}` for current board");
            }
            state.board = state.board.make_move_new(chess_move);
            state.move_history.push(uci.to_owned());
            state.engine_pending = state.board.status() == BoardStatus::Ongoing;
            state.last_error = None;
            Ok(())
        })
    }

    /// Apply one already-selected engine move and retain engine-specific view
    /// state. This is used by mounted external-control examples whose host
    /// publishes its own durable wake rather than the legacy `ViewAwake`.
    #[allow(dead_code)]
    pub fn apply_engine_uci(&self, uci: &str) -> anyhow::Result<()> {
        let chess_move = ChessMove::from_str(uci)
            .map_err(|error| anyhow::anyhow!("invalid UCI move `{uci}`: {error}"))?;
        self.with_state(|state| {
            if !state.board.legal(chess_move) {
                anyhow::bail!("illegal engine move `{uci}` for current board");
            }
            state.board = state.board.make_move_new(chess_move);
            state.move_history.push(uci.to_owned());
            state.engine_pending = false;
            state.last_engine_move = Some(uci.to_owned());
            state.last_error = None;
            Ok(())
        })
    }

    /// Record an engine failure without choosing a wake mechanism.
    ///
    /// Legacy `AgentViewApp` wakes through `ViewAwakeHandle`; mounted hosts
    /// publish their own durable wake cursor. The chess domain owns the state
    /// transition, while each runtime owns notification and persistence.
    #[allow(dead_code)]
    pub fn record_engine_failure(&self, message: impl Into<String>) {
        self.with_state(|state| {
            state.engine_pending = false;
            state.last_error = Some(message.into());
        });
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut ChessGameState) -> R) -> R {
        let mut state = self.inner.lock().unwrap();
        f(&mut state)
    }
}

impl Default for ChessGameSource {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct ChessGameState {
    board: Board,
    move_history: Vec<String>,
    engine_pending: bool,
    last_engine_move: Option<String>,
    last_error: Option<String>,
}

impl ChessGameState {
    pub fn new() -> Self {
        Self {
            board: Board::default(),
            move_history: Vec::new(),
            engine_pending: false,
            last_engine_move: None,
            last_error: None,
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn move_history(&self) -> &[String] {
        &self.move_history
    }

    fn legal_uci_moves(&self) -> Vec<String> {
        legal_uci_moves(&self.board)
    }
}

impl Default for ChessGameState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChessSide {
    White,
    Black,
}

impl ChessSide {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::White => "white",
            Self::Black => "black",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChessPieceKind {
    Pawn,
    Knight,
    Bishop,
    Rook,
    Queen,
    King,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChessPieceView {
    pub side: ChessSide,
    pub kind: ChessPieceKind,
    pub symbol: char,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChessSquareView {
    pub square: String,
    pub file: char,
    pub rank: u8,
    pub piece: Option<ChessPieceView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "square")]
struct ChessSquarePromptView {
    id: String,
    file: char,
    rank: u8,

    #[view(text)]
    piece: char,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "board_state")]
struct ChessBoardStatePromptView {
    #[view(element)]
    board_ascii: String,

    #[view(element)]
    fen: String,

    #[view(element)]
    side_to_move: String,

    #[view(element)]
    status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "move")]
struct ChessMovePromptView {
    #[view(text)]
    uci: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "engine")]
struct ChessEnginePromptView {
    #[view(element)]
    pending: bool,

    #[view(flatten)]
    last_move: Option<ChessLastMovePromptView>,

    #[view(flatten)]
    last_error: Option<ChessLastErrorPromptView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "last_move")]
struct ChessLastMovePromptView {
    #[view(text)]
    uci: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "last_error")]
struct ChessLastErrorPromptView {
    #[view(text)]
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, AgentView)]
#[agent_view(markdown = "paragraph")]
struct ChessReasoningPolicyItemPromptView {
    #[view(text)]
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, AgentView)]
#[agent_view(document)]
pub struct ChessSystemPromptView {
    #[view(heading = 1)]
    title: String,

    #[view(paragraph)]
    task: String,

    #[view(heading = 2)]
    reasoning_policy_title: String,

    #[view(unordered_list)]
    reasoning_policy: Vec<ChessReasoningPolicyItemPromptView>,

    #[view(xml)]
    reply_contract: ChessMoveContract,
}

/// The stable chess policy POM without the reply contract.
///
/// External reply components append the shared semantic contract after this
/// contribution. CLI syntax stays outside this POM as a host transport
/// envelope, so it cannot drift into a second model-facing grammar.
#[derive(Debug, Clone, PartialEq, Eq, AgentView)]
#[agent_view(document)]
#[allow(dead_code)]
pub struct ChessSystemPolicyPromptView {
    #[view(heading = 1)]
    title: String,

    #[view(paragraph)]
    task: String,

    #[view(heading = 2)]
    reasoning_policy_title: String,

    #[view(unordered_list)]
    reasoning_policy: Vec<ChessReasoningPolicyItemPromptView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChessRankView {
    pub rank: u8,
    pub squares: Vec<ChessSquareView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChessBoardView {
    pub ranks: Vec<ChessRankView>,
}

impl ChessBoardView {
    pub fn ascii_diagram(&self) -> String {
        let mut lines = Vec::with_capacity(self.ranks.len() + 1);
        for rank in &self.ranks {
            let mut line = rank.rank.to_string();
            for square in &rank.squares {
                line.push(' ');
                line.push(
                    square
                        .piece
                        .as_ref()
                        .map(|piece| piece.symbol)
                        .unwrap_or('.'),
                );
            }
            lines.push(line);
        }
        lines.push("  a b c d e f g h".to_owned());
        lines.join("\n")
    }
}

impl AgentViewCollect<ChessSquareView> for ChessSquarePromptView {
    fn collect(square: &ChessSquareView) -> Self {
        Self {
            id: square.square.clone(),
            file: square.file,
            rank: square.rank,
            piece: square
                .piece
                .as_ref()
                .map(|piece| piece.symbol)
                .unwrap_or('.'),
        }
    }
}

struct ChessBoardStateSource<'a> {
    board: &'a ChessBoardView,
    fen: &'a str,
    side_to_move: ChessSide,
    status: &'a str,
}

impl AgentViewCollect<ChessBoardStateSource<'_>> for ChessBoardStatePromptView {
    fn collect(source: &ChessBoardStateSource<'_>) -> Self {
        Self {
            board_ascii: source.board.ascii_diagram(),
            fen: source.fen.to_owned(),
            side_to_move: source.side_to_move.as_str().to_owned(),
            status: source.status.to_owned(),
        }
    }
}

#[cfg(test)]
fn render_chess_board_state_xml(view: &ChessView) -> String {
    render_pom_document(&resolve_system_document(Document::from_xml(
        view.board_state.build_root().unwrap(),
    )))
    .unwrap()
}

#[cfg(test)]
#[allow(dead_code)]
pub fn render_board_state_xml_for_test(view: &ChessView) -> String {
    render_chess_board_state_xml(view)
}

fn chess_move_prompt_view(uci: &str) -> ChessMovePromptView {
    ChessMovePromptView {
        uci: uci.to_owned(),
    }
}

fn chess_move_prompt_views(moves: &[String]) -> Vec<ChessMovePromptView> {
    moves
        .iter()
        .map(|uci| chess_move_prompt_view(uci))
        .collect()
}

fn chess_last_move_prompt_view(uci: &str) -> ChessLastMovePromptView {
    ChessLastMovePromptView {
        uci: uci.to_owned(),
    }
}

fn chess_last_error_prompt_view(message: &str) -> ChessLastErrorPromptView {
    ChessLastErrorPromptView {
        message: message.to_owned(),
    }
}

impl AgentViewCollect<ChessGameState> for ChessView {
    fn collect(state: &ChessGameState) -> Self {
        let board = board_view(&state.board);
        let fen = state.board.to_string();
        let side_to_move = side_view(state.board.side_to_move());
        let legal_uci_moves = state.legal_uci_moves();
        let status = board_status_name(state.board.status()).to_owned();

        Self {
            board_state: ChessBoardStatePromptView::collect(&ChessBoardStateSource {
                board: &board,
                fen: &fen,
                side_to_move,
                status: &status,
            }),
            board_squares: board
                .ranks
                .iter()
                .flat_map(|rank| rank.squares.iter())
                .map(ChessSquarePromptView::collect)
                .collect(),
            legal_moves: chess_move_prompt_views(&legal_uci_moves),
            move_history_view: chess_move_prompt_views(&state.move_history),
            engine: ChessEnginePromptView {
                pending: state.engine_pending,
                last_move: state
                    .last_engine_move
                    .as_deref()
                    .map(chess_last_move_prompt_view),
                last_error: state
                    .last_error
                    .as_deref()
                    .map(chess_last_error_prompt_view),
            },
        }
    }
}

fn chess_system_reasoning_policy_prompt_view() -> Vec<ChessReasoningPolicyItemPromptView> {
    [CHESS_REASONING_PRIVATE, CHESS_REASONING_NO_COT]
        .into_iter()
        .map(|text| ChessReasoningPolicyItemPromptView {
            text: text.to_owned(),
        })
        .collect()
}

impl Default for ChessSystemPromptView {
    fn default() -> Self {
        Self {
            title: "Chess move agent".to_owned(),
            task: CHESS_SYSTEM_TASK.to_owned(),
            reasoning_policy_title: "Reasoning policy".to_owned(),
            reasoning_policy: chess_system_reasoning_policy_prompt_view(),
            reply_contract: ChessMoveContract,
        }
    }
}

impl Default for ChessSystemPolicyPromptView {
    fn default() -> Self {
        Self {
            title: "Chess move agent".to_owned(),
            task: CHESS_SYSTEM_TASK.to_owned(),
            reasoning_policy_title: "Reasoning policy".to_owned(),
            reasoning_policy: chess_system_reasoning_policy_prompt_view(),
        }
    }
}

#[cfg(test)]
#[test]
fn chess_system_prompt_view_produces_the_exact_pom_document() {
    fn assert_document_agent_view<T: AgentView<Root = Document>>() {}

    assert_document_agent_view::<ChessSystemPromptView>();

    let prompt = ChessSystemPromptView::default();
    let document = prompt.build_root().unwrap();
    let rendered = render_pom_document(&resolve_system_document(document)).unwrap();

    assert_eq!(
        rendered,
        concat!(
            "# Chess move agent\n\n",
            "You are choosing legal chess moves from the rendered board.\n\n",
            "## Reasoning policy\n\n",
            "- Think privately about candidate moves before acting.\n",
            "- Do not print chain-of-thought; return only the XML move after deciding.\n\n",
            "<move uci=\"...\" />"
        )
    );
}

/// Full chess VM snapshot for an outside chat/CLI/daemon/skill caller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "prompt_board")]
pub struct ChessView {
    #[view(diff(replace))]
    board_state: ChessBoardStatePromptView,

    #[view(diff(key = "id"))]
    board_squares: Vec<ChessSquarePromptView>,

    #[view(diff(set))]
    legal_moves: Vec<ChessMovePromptView>,

    #[view(name = "move_history", diff(seq))]
    move_history_view: Vec<ChessMovePromptView>,

    #[view(diff(replace))]
    engine: ChessEnginePromptView,
}

impl ChessView {
    #[allow(dead_code)]
    pub fn side_to_move(&self) -> &str {
        &self.board_state.side_to_move
    }

    #[allow(dead_code)]
    pub fn legal_uci_moves(&self) -> Vec<&str> {
        self.legal_moves
            .iter()
            .map(|move_view| move_view.uci.as_str())
            .collect()
    }

    #[allow(dead_code)]
    pub fn move_history(&self) -> Vec<&str> {
        self.move_history_view
            .iter()
            .map(|move_view| move_view.uci.as_str())
            .collect()
    }

    #[allow(dead_code)]
    pub fn engine_pending(&self) -> bool {
        self.engine.pending
    }

    #[allow(dead_code)]
    pub fn last_engine_move(&self) -> Option<&str> {
        self.engine
            .last_move
            .as_ref()
            .map(|move_view| move_view.uci.as_str())
    }

    #[allow(dead_code)]
    pub fn last_error(&self) -> Option<&str> {
        self.engine
            .last_error
            .as_ref()
            .map(|error| error.message.as_str())
    }

    #[cfg(test)]
    fn square_for_test(
        &self,
        rank_index: usize,
        square_index: usize,
    ) -> Option<&ChessSquarePromptView> {
        let index = rank_index
            .checked_mul(ALL_FILES.len())?
            .checked_add(square_index)?;
        self.board_squares.get(index)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn rank_for_test(&self, rank_index: usize) -> Option<u8> {
        self.square_for_test(rank_index, 0)
            .map(|square| square.rank)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn square_id_for_test(&self, rank_index: usize, square_index: usize) -> Option<&str> {
        self.square_for_test(rank_index, square_index)
            .map(|square| square.id.as_str())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn piece_symbol_for_test(&self, rank_index: usize, square_index: usize) -> Option<char> {
        self.square_for_test(rank_index, square_index)
            .map(|square| square.piece)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn render_square_xml_for_test(
        &self,
        rank_index: usize,
        square_index: usize,
    ) -> Option<String> {
        let square = self.square_for_test(rank_index, square_index)?;
        render_pom_document(&resolve_system_document(Document::from_xml(
            square.build_root().ok()?,
        )))
        .ok()
    }
}

/// Per-turn task data for the next chess move.
///
/// Stable policy and reply grammar live in the System document. This User POM
/// carries only data that can change with the turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "chess_task")]
pub struct ChessTaskView {
    #[view(element)]
    pub instruction: String,

    #[view(element)]
    pub active_turn_id: String,
}

impl ChessTaskView {
    pub fn new(instruction: impl Into<String>, active_turn_id: impl Into<String>) -> Self {
        Self {
            instruction: instruction.into(),
            active_turn_id: active_turn_id.into(),
        }
    }
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(document)]
struct ChessUserDocumentView {
    #[view(name = "agent_context", diff)]
    context: ChessView,

    #[view(xml)]
    task: ChessTaskView,
}

/// Build the per-turn chess User POM without selecting a runtime owner.
///
/// Both the legacy external-control example and the mounted lifecycle golden
/// example use this pure authoring function.
pub fn chess_user_document(
    context: ChessView,
    instruction: impl Into<String>,
    active_turn_id: impl Into<String>,
) -> Result<Document, PomError> {
    ChessUserDocumentView {
        context,
        task: ChessTaskView::new(instruction, active_turn_id),
    }
    .build_root()
}

#[derive(Debug, Clone, Default)]
pub struct ChessViewModel;

#[async_trait::async_trait]
impl AgentViewModel<Turn, ()> for ChessViewModel {
    type Source = ChessGameSource;
    type View = ChessView;
    type ContextState = ();

    async fn build_system_document(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        _source: &Self::Source,
    ) -> anyhow::Result<Document> {
        let prompt = ChessSystemPromptView::default();
        Ok(prompt.build_root()?)
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View {
        ChessView::collect(&source.snapshot())
    }

    async fn build_user_document(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        call_id: &str,
        task: StorageString,
        current_view: &Self::View,
    ) -> anyhow::Result<Document> {
        Ok(chess_user_document(current_view.clone(), task, call_id)?)
    }

    async fn commit_turn(
        &self,
        _ctx: &mut PromptContext<Turn, Self::ContextState>,
        _request: &AgentTurnRequest<Turn>,
        _executor_commit: ExecutorCommit<Turn>,
        _sink_output: &mut (),
    ) -> anyhow::Result<TurnFlow> {
        Ok(TurnFlow::Wait)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerMove {
    pub uci: StorageString,
    chess_move: ChessMove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChessMoveOutput {
    Accepted(PlayerMove),
    Rejected { message: String },
}

/// Parses and validates a move supplied by an external caller.
pub struct ChessMoveSink {
    board: Board,
    output: Option<ChessMoveOutput>,
}

impl ChessMoveSink {
    pub fn from_source(source: &ChessGameSource) -> Self {
        Self {
            board: source.snapshot().board,
            output: None,
        }
    }

    fn parse_reply(reply: ControlReply) -> Result<ChessAction, String> {
        let uci = match reply {
            ControlReply::Text(text) => Ok(text.trim().to_owned()),
            ControlReply::Structured(value) => value
                .get("uci")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|uci| !uci.is_empty())
                .map(ToOwned::to_owned)
                .ok_or_else(|| "expected structured reply with non-empty `uci`".to_owned()),
        }?;
        ChessAction::from_uci(uci).map_err(|diagnostic| diagnostic.to_string())
    }

    fn reject(&mut self, message: impl Into<String>) {
        self.output = Some(ChessMoveOutput::Rejected {
            message: message.into(),
        });
    }
}

#[async_trait::async_trait]
impl TurnSink<ControlReply> for ChessMoveSink {
    type Output = ChessMoveOutput;

    async fn on_event(&mut self, reply: ControlReply) {
        let action = match Self::parse_reply(reply) {
            Ok(action) => action,
            Err(message) => {
                self.reject(message);
                return;
            }
        };

        let chess_move =
            ChessMove::from_str(action.uci()).expect("a ChessAction always has UCI syntax");

        if !self.board.legal(chess_move) {
            self.reject(format!(
                "illegal chess move `{}` for current board",
                action.uci()
            ));
            return;
        }

        self.output = Some(ChessMoveOutput::Accepted(PlayerMove {
            uci: action.uci.into(),
            chess_move,
        }));
    }

    async fn finish(self: Box<Self>) -> Self::Output {
        self.output.unwrap_or_else(|| ChessMoveOutput::Rejected {
            message: "no chess move reply was provided".to_owned(),
        })
    }
}

pub fn apply_player_move(
    session: &mut AgentSession<Turn, ()>,
    source: &ChessGameSource,
    output: ChessMoveOutput,
) -> anyhow::Result<()> {
    match output {
        ChessMoveOutput::Accepted(player_move) => {
            source.with_state(|state| {
                state.board = state.board.make_move_new(player_move.chess_move);
                state.move_history.push(player_move.uci.to_string());
                state.engine_pending = state.board.status() == BoardStatus::Ongoing;
                state.last_error = None;
            });
            session.push_history(Turn::user(format!("player_move = {}", player_move.uci)));
        }
        ChessMoveOutput::Rejected { message } => {
            source.with_state(|state| {
                state.engine_pending = false;
                state.last_error = Some(message.clone());
            });
            session.push_history(Turn::user(format!("rejected_player_move = {message}")));
        }
    }
    Ok(())
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct StockfishEngine {
    command: String,
    movetime: Duration,
}

#[allow(dead_code)]
impl StockfishEngine {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            movetime: Duration::from_millis(100),
        }
    }

    pub async fn best_move(&self, state: &ChessGameState) -> anyhow::Result<String> {
        let mut child = Command::new(&self.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| {
                anyhow::anyhow!("failed to start stockfish `{}`: {err}", self.command)
            })?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("stockfish stdin was unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("stockfish stdout was unavailable"))?;
        let mut lines = BufReader::new(stdout).lines();

        write_uci_line(&mut stdin, "uci").await?;
        wait_for_uci_line(&mut lines, "uciok").await?;
        write_uci_line(&mut stdin, "isready").await?;
        wait_for_uci_line(&mut lines, "readyok").await?;
        write_uci_line(&mut stdin, &format!("position fen {}", state.board)).await?;
        write_uci_line(
            &mut stdin,
            &format!("go movetime {}", self.movetime.as_millis()),
        )
        .await?;

        let best_move = wait_for_bestmove(&mut lines).await?;
        let _ = write_uci_line(&mut stdin, "quit").await;
        let _ = timeout(Duration::from_millis(100), child.wait()).await;

        Ok(best_move)
    }
}

#[allow(dead_code)]
async fn write_uci_line(stdin: &mut tokio::process::ChildStdin, line: &str) -> anyhow::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

#[allow(dead_code)]
async fn wait_for_uci_line(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    expected: &str,
) -> anyhow::Result<()> {
    timeout(Duration::from_secs(2), async {
        while let Some(line) = lines.next_line().await? {
            if line.trim() == expected {
                return anyhow::Ok(());
            }
        }
        anyhow::bail!("stockfish exited before `{expected}`")
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for stockfish `{expected}`"))?
}

#[allow(dead_code)]
async fn wait_for_bestmove(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> anyhow::Result<String> {
    timeout(Duration::from_secs(5), async {
        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("bestmove ") {
                let best_move = rest
                    .split_whitespace()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("stockfish returned empty bestmove"))?;
                if best_move == "(none)" {
                    anyhow::bail!("stockfish returned no legal move");
                }
                return Ok(best_move.to_owned());
            }
        }
        anyhow::bail!("stockfish exited before bestmove")
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for stockfish bestmove"))?
}

#[allow(dead_code)]
pub async fn apply_engine_move(
    source: &ChessGameSource,
    awake: &ViewAwakeHandle,
    engine: &StockfishEngine,
) -> anyhow::Result<String> {
    let engine_move = match engine.best_move(&source.snapshot()).await {
        Ok(engine_move) => engine_move,
        Err(err) => {
            let message = format!("stockfish failed: {err}");
            source.with_state(|state| {
                state.last_error = Some(message);
                state.engine_pending = false;
            });
            awake.awake();
            return Err(err);
        }
    };

    commit_engine_move(source, awake, "stockfish", engine_move)
}

fn commit_engine_move(
    source: &ChessGameSource,
    awake: &ViewAwakeHandle,
    engine_name: &str,
    engine_move: String,
) -> anyhow::Result<String> {
    let chess_move = match ChessMove::from_str(&engine_move) {
        Ok(chess_move) => chess_move,
        Err(err) => {
            let message = format!("{engine_name} produced invalid move `{engine_move}`: {err}");
            source.with_state(|state| {
                state.last_error = Some(message.clone());
                state.engine_pending = false;
            });
            awake.awake();
            anyhow::bail!(message);
        }
    };

    let legal = source.with_state(|state| {
        if !state.board.legal(chess_move) {
            state.last_error = Some(format!(
                "{engine_name} produced illegal move `{engine_move}`"
            ));
            state.engine_pending = false;
            return false;
        }

        state.board = state.board.make_move_new(chess_move);
        state.move_history.push(engine_move.clone());
        state.last_engine_move = Some(engine_move.clone());
        state.engine_pending = false;
        state.last_error = None;
        true
    });
    awake.awake();
    if !legal {
        anyhow::bail!("{engine_name} produced illegal move `{engine_move}`");
    }
    Ok(engine_move)
}

fn legal_uci_moves(board: &Board) -> Vec<String> {
    let mut moves = MoveGen::new_legal(board)
        .map(|chess_move| chess_move.to_string())
        .collect::<Vec<_>>();
    moves.sort();
    moves
}

fn board_status_name(status: BoardStatus) -> &'static str {
    match status {
        BoardStatus::Ongoing => "ongoing",
        BoardStatus::Stalemate => "stalemate",
        BoardStatus::Checkmate => "checkmate",
    }
}

fn side_view(color: Color) -> ChessSide {
    match color {
        Color::White => ChessSide::White,
        Color::Black => ChessSide::Black,
    }
}

fn kind_view(piece: Piece) -> ChessPieceKind {
    match piece {
        Piece::Pawn => ChessPieceKind::Pawn,
        Piece::Knight => ChessPieceKind::Knight,
        Piece::Bishop => ChessPieceKind::Bishop,
        Piece::Rook => ChessPieceKind::Rook,
        Piece::Queen => ChessPieceKind::Queen,
        Piece::King => ChessPieceKind::King,
    }
}

fn board_view(board: &Board) -> ChessBoardView {
    let ranks = ALL_RANKS
        .iter()
        .rev()
        .map(|rank| {
            let rank_number = (rank.to_index() + 1) as u8;
            let squares = ALL_FILES
                .iter()
                .map(|file| {
                    let square = Square::make_square(*rank, *file);
                    let piece = board.piece_on(square).map(|piece| {
                        let side = side_view(board.color_on(square).unwrap());
                        ChessPieceView {
                            side,
                            kind: kind_view(piece),
                            symbol: piece_char(piece, side),
                        }
                    });
                    ChessSquareView {
                        square: square.to_string(),
                        file: file_char(file.to_index()),
                        rank: rank_number,
                        piece,
                    }
                })
                .collect();
            ChessRankView {
                rank: rank_number,
                squares,
            }
        })
        .collect();
    ChessBoardView { ranks }
}

fn file_char(index: usize) -> char {
    (b'a' + index as u8) as char
}

fn piece_char(piece: Piece, side: ChessSide) -> char {
    let ch = match piece {
        Piece::Pawn => 'p',
        Piece::Knight => 'n',
        Piece::Bishop => 'b',
        Piece::Rook => 'r',
        Piece::Queen => 'q',
        Piece::King => 'k',
    };

    match side {
        ChessSide::White => ch.to_ascii_uppercase(),
        ChessSide::Black => ch,
    }
}
