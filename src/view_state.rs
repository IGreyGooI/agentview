//! Generic full and incremental view state envelopes.

use serde::{Deserialize, Serialize};

use crate::view_awake::ViewEpoch;
use crate::StorageString;

/// Identifier for the active user document inside a view snapshot.
pub type ViewTurnId = StorageString;

/// Full view state captured at a view epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewSnapshot<View, UserDocument = ()> {
    pub view_epoch: ViewEpoch,
    pub turn_id: ViewTurnId,
    pub view: View,
    pub user_document: UserDocument,
}

impl<View, UserDocument> ViewSnapshot<View, UserDocument> {
    pub fn new(
        view_epoch: ViewEpoch,
        turn_id: impl Into<ViewTurnId>,
        view: View,
        user_document: UserDocument,
    ) -> Self {
        Self {
            view_epoch,
            turn_id: turn_id.into(),
            view,
            user_document,
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
pub struct ViewUpdate<View, UserDocument = ()> {
    pub base_epoch: ViewEpoch,
    pub view_epoch: ViewEpoch,
    pub body: ViewUpdateBody<View, UserDocument>,
}

impl<View, UserDocument> ViewUpdate<View, UserDocument> {
    pub fn full(base_epoch: ViewEpoch, snapshot: ViewSnapshot<View, UserDocument>) -> Self {
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

    pub fn snapshot(&self) -> Option<&ViewSnapshot<View, UserDocument>> {
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
pub enum ViewUpdateBody<View, UserDocument = ()> {
    Full(ViewSnapshot<View, UserDocument>),
    Partial(ViewPatch),
}
