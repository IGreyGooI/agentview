//! Thin semantic roles over the frame-native queued reaction exchange.
//!
//! These crate-private adapters exist so Skill and Plugin lifecycle tests use
//! real [`ReactionPort`](super::reaction::ReactionPort) handoff semantics. The
//! shared `Application` still owns canonical history, diff state, and cursors.

#![allow(dead_code)]

use async_trait::async_trait;

use super::{
    external::{
        ExternalAct, ExternalControl, ExternalControlFault, ExternalIngressGeneration,
        ExternalObservation, ExternalProviderPort,
    },
    reaction::{
        Frame, ProviderFactStream, ReactionPort, ReactionPortFault, SubmitFault, TargetDeclaration,
    },
};

pub(crate) struct SkillPort {
    queued: ExternalProviderPort,
}

#[derive(Clone)]
pub(crate) struct SkillControl {
    queued: ExternalControl,
}

impl SkillPort {
    pub(crate) fn new() -> Result<(Self, SkillControl), ReactionPortFault> {
        let (queued, control) = ExternalProviderPort::new()?;
        Ok((Self { queued }, SkillControl { queued: control }))
    }
}

impl SkillControl {
    pub(crate) async fn next_observation(
        &self,
    ) -> Result<ExternalObservation, ExternalControlFault> {
        self.queued.next_observation().await
    }

    pub(crate) async fn act(
        &self,
        generation: ExternalIngressGeneration,
        act: ExternalAct,
    ) -> Result<(), ExternalControlFault> {
        self.queued.act(generation, act).await
    }

    pub(crate) async fn complete(
        &self,
        generation: ExternalIngressGeneration,
    ) -> Result<(), ExternalControlFault> {
        self.queued.complete(generation).await
    }
}

#[async_trait]
impl ReactionPort for SkillPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.queued.declare()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        self.queued.submit(frame).await
    }
}

pub(crate) struct PluginPort {
    queued: ExternalProviderPort,
}

#[derive(Clone)]
pub(crate) struct PluginControl {
    queued: ExternalControl,
}

impl PluginPort {
    pub(crate) fn new() -> Result<(Self, PluginControl), ReactionPortFault> {
        let (queued, control) = ExternalProviderPort::new()?;
        Ok((Self { queued }, PluginControl { queued: control }))
    }
}

impl PluginControl {
    pub(crate) async fn next_observation(
        &self,
    ) -> Result<ExternalObservation, ExternalControlFault> {
        self.queued.next_observation().await
    }

    pub(crate) async fn act(
        &self,
        generation: ExternalIngressGeneration,
        act: ExternalAct,
    ) -> Result<(), ExternalControlFault> {
        self.queued.act(generation, act).await
    }

    pub(crate) async fn complete(
        &self,
        generation: ExternalIngressGeneration,
    ) -> Result<(), ExternalControlFault> {
        self.queued.complete(generation).await
    }
}

#[async_trait]
impl ReactionPort for PluginPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.queued.declare()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        self.queued.submit(frame).await
    }
}

#[cfg(test)]
mod tests;
