//! Runtime-injected provenance for Component events.
//!
//! These identities are process-local tracing metadata. They confer no dispatch,
//! authorization, or business concurrency capability.

use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

use super::{
    authoring::{StreamingToolAttemptId, XmlOccurrenceId},
    execution::{FrameRevision, ProviderOutputKey},
};

static NEXT_APPLICATION: AtomicU64 = AtomicU64::new(1);
static NEXT_REACTION: AtomicU64 = AtomicU64::new(1);
static NEXT_EVENT: AtomicU64 = AtomicU64::new(1);

macro_rules! identity {
    ($name:ident, $counter:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name(u64);

        impl $name {
            pub(crate) fn fresh() -> Self {
                let value = $counter
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                    .unwrap_or_else(|_| {
                        panic!(concat!(stringify!($name), " identity space exhausted"))
                    });
                Self(value)
            }

            /// The opaque process-local numeric identity.
            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

identity!(
    ApplicationId,
    NEXT_APPLICATION,
    "Identity of an independent Application or standalone ComponentHost."
);
identity!(
    ReactionId,
    NEXT_REACTION,
    "Identity of one react operation, including a pre-handoff continuity retry."
);
identity!(
    EventId,
    NEXT_EVENT,
    "Identity of one runtime-dispatched event; multicast listeners share this identity."
);

/// The fixed history ownership of one admitted reaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReactionKind {
    /// Advances the Application's primary model history.
    Main,
    /// Owns an isolated model history while sharing the mounted business view.
    Temporary,
}

impl ReactionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Temporary => "temporary",
        }
    }
}

/// Structured origin retained alongside business callback arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventSource {
    /// Ordinary provider output. Legacy adapters may not have an output key.
    Provider { output: Option<ProviderOutputKey> },
    /// An admitted native tool action. Call IDs are scoped to their reaction.
    NativeTool {
        call_id: String,
        output: Option<ProviderOutputKey>,
        ordinal: Option<u64>,
    },
    /// An XML parser event, with identities local to its contract attempt.
    Xml {
        attempt: StreamingToolAttemptId,
        occurrence: Option<XmlOccurrenceId>,
        sequence: u64,
    },
    /// An event from the legacy shared-route XML parser. Its parent identifies
    /// the provider event (or stream completion) that produced it.
    ParsedXml,
    /// Normal completion of a provider reaction, after its actions have settled.
    ReactionCompleted,
    /// An external application command, outside any model reaction.
    Command { name: String },
    /// A standalone host event without model protocol metadata.
    Host,
}

/// Immutable provenance supplied by the runtime, outside the model's input schema.
///
/// Move or clone this value into asynchronous work and store it with business
/// outcomes to preserve their original source. A new render does not rewrite it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventContext {
    scope: EventScope,
    event_id: EventId,
    source: EventSource,
    caused_by: Option<EventId>,
}

impl EventContext {
    pub const fn application_id(&self) -> ApplicationId {
        self.scope.application_id
    }
    pub const fn reaction_id(&self) -> Option<ReactionId> {
        self.scope.reaction_id
    }
    pub const fn reaction_kind(&self) -> Option<ReactionKind> {
        self.scope.reaction_kind
    }
    pub(crate) const fn reaction_lineage(&self) -> Option<ReactionId> {
        self.scope.reaction_lineage
    }
    pub const fn frame_revision(&self) -> Option<FrameRevision> {
        self.scope.frame_revision
    }
    pub const fn event_id(&self) -> EventId {
        self.event_id
    }
    pub fn source(&self) -> &EventSource {
        &self.source
    }
    pub const fn caused_by(&self) -> Option<EventId> {
        self.caused_by
    }

    pub(crate) fn child(&self, source: EventSource) -> Self {
        let mut child = self.scope.event(source);
        child.caused_by = Some(self.event_id);
        child
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EventScope {
    application_id: ApplicationId,
    reaction_id: Option<ReactionId>,
    reaction_kind: Option<ReactionKind>,
    reaction_lineage: Option<ReactionId>,
    frame_revision: Option<FrameRevision>,
}

impl EventScope {
    pub(crate) const fn frame_revision(&self) -> Option<FrameRevision> {
        self.frame_revision
    }

    pub(crate) const fn application_id(&self) -> ApplicationId {
        self.application_id
    }
    pub(crate) const fn reaction_lineage(&self) -> Option<ReactionId> {
        self.reaction_lineage
    }
    pub(crate) const fn application(application_id: ApplicationId) -> Self {
        Self {
            application_id,
            reaction_id: None,
            reaction_kind: None,
            reaction_lineage: None,
            frame_revision: None,
        }
    }

    #[cfg(feature = "legacy-provider-port")]
    pub(crate) fn unframed_reaction(application_id: ApplicationId) -> Self {
        Self {
            application_id,
            reaction_id: Some(ReactionId::fresh()),
            reaction_kind: Some(ReactionKind::Main),
            reaction_lineage: None,
            frame_revision: None,
        }
    }

    pub(crate) const fn reaction(
        application_id: ApplicationId,
        reaction_id: ReactionId,
        frame_revision: FrameRevision,
    ) -> Self {
        Self::reaction_with_kind(
            application_id,
            reaction_id,
            ReactionKind::Main,
            frame_revision,
        )
    }

    pub(crate) const fn reaction_with_kind(
        application_id: ApplicationId,
        reaction_id: ReactionId,
        reaction_kind: ReactionKind,
        frame_revision: FrameRevision,
    ) -> Self {
        Self {
            application_id,
            reaction_id: Some(reaction_id),
            reaction_kind: Some(reaction_kind),
            reaction_lineage: match reaction_kind {
                ReactionKind::Main => None,
                ReactionKind::Temporary => Some(reaction_id),
            },
            frame_revision: Some(frame_revision),
        }
    }

    /// Continue a temporary protocol context with a newly admitted reaction ID.
    pub(crate) fn with_temporary_lineage(mut self, lineage: ReactionId) -> Self {
        assert_eq!(self.reaction_kind, Some(ReactionKind::Temporary));
        self.reaction_lineage = Some(lineage);
        self
    }

    pub(crate) fn event(&self, source: EventSource) -> EventContext {
        EventContext {
            scope: *self,
            event_id: EventId::fresh(),
            source,
            caused_by: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU128, NonZeroU64};

    use super::*;
    use crate::component::execution::{TargetEpoch, TargetIdentity};

    pub(crate) fn revision() -> FrameRevision {
        FrameRevision::new(
            NonZeroU128::new(1).unwrap(),
            TargetIdentity::new(NonZeroU128::new(2).unwrap()),
            TargetEpoch::new(NonZeroU64::new(1).unwrap()),
            NonZeroU64::new(1).unwrap(),
        )
    }

    #[test]
    fn derived_events_preserve_temporary_reaction_kind_and_lineage() {
        let application = ApplicationId::fresh();
        let lineage = ReactionId::fresh();
        let reaction = ReactionId::fresh();
        let frame = revision();
        let context =
            EventScope::reaction_with_kind(application, reaction, ReactionKind::Temporary, frame)
                .with_temporary_lineage(lineage)
                .event(EventSource::Provider { output: None });
        let child = context.child(EventSource::ParsedXml);

        assert_eq!(child.application_id(), application);
        assert_eq!(child.reaction_id(), Some(reaction));
        assert_eq!(child.reaction_kind(), Some(ReactionKind::Temporary));
        assert_eq!(child.reaction_lineage(), Some(lineage));
        assert_eq!(child.frame_revision(), Some(frame));
        assert_eq!(child.caused_by(), Some(context.event_id()));
        assert_ne!(child.event_id(), context.event_id());

        let command = EventScope::application(application).event(EventSource::Command {
            name: "inspect".to_owned(),
        });
        assert_eq!(command.reaction_id(), None);
        assert_eq!(command.reaction_kind(), None);
        assert_eq!(command.reaction_lineage(), None);
        assert_eq!(command.frame_revision(), None);
    }
}
