//! Generic full and incremental view state envelopes.

use serde::{Deserialize, Serialize};

use crate::view_awake::ViewEpoch;
use crate::StorageString;

/// Identifier for the active turn prompt inside a view snapshot.
pub type ViewTurnId = StorageString;

/// Full view state captured at a view epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewSnapshot<View, TurnPrompt = ()> {
    pub view_epoch: ViewEpoch,
    pub turn_id: ViewTurnId,
    pub view: View,
    pub turn_prompt: TurnPrompt,
}

impl<View, TurnPrompt> ViewSnapshot<View, TurnPrompt> {
    pub fn new(
        view_epoch: ViewEpoch,
        turn_id: impl Into<ViewTurnId>,
        view: View,
        turn_prompt: TurnPrompt,
    ) -> Self {
        Self {
            view_epoch,
            turn_id: turn_id.into(),
            view,
            turn_prompt,
        }
    }
}

/// Patch payload for a partial view update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewPatch {
    pub body: serde_json::Value,
}

impl ViewPatch {
    pub fn json(body: serde_json::Value) -> Self {
        Self { body }
    }
}

/// Full-or-partial transition between view epochs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewUpdate<View, TurnPrompt = ()> {
    pub base_epoch: ViewEpoch,
    pub view_epoch: ViewEpoch,
    pub body: ViewUpdateBody<View, TurnPrompt>,
}

impl<View, TurnPrompt> ViewUpdate<View, TurnPrompt> {
    pub fn full(base_epoch: ViewEpoch, snapshot: ViewSnapshot<View, TurnPrompt>) -> Self {
        Self {
            base_epoch,
            view_epoch: snapshot.view_epoch,
            body: ViewUpdateBody::Full(snapshot),
        }
    }

    pub fn partial(base_epoch: ViewEpoch, view_epoch: ViewEpoch, patch: ViewPatch) -> Self {
        Self {
            base_epoch,
            view_epoch,
            body: ViewUpdateBody::Partial(patch),
        }
    }

    pub fn snapshot(&self) -> Option<&ViewSnapshot<View, TurnPrompt>> {
        match &self.body {
            ViewUpdateBody::Full(snapshot) => Some(snapshot),
            ViewUpdateBody::Partial(_) => None,
        }
    }

    pub fn patch(&self) -> Option<&ViewPatch> {
        match &self.body {
            ViewUpdateBody::Partial(patch) => Some(patch),
            ViewUpdateBody::Full(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ViewUpdateBody<View, TurnPrompt = ()> {
    Full(ViewSnapshot<View, TurnPrompt>),
    Partial(ViewPatch),
}
