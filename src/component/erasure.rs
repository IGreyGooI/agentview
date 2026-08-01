//! Runtime type descriptors and the private mounted-epoch erasure boundary.

use std::{any::TypeId, fmt};

use super::TurnChannels;

/// Process-local identity and diagnostic name for one Rust type.
///
/// `TypeId` is suitable for checking an in-process adapter boundary. Neither
/// it nor `type_name` is a durable identifier across builds or processes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeSlot {
    type_id: TypeId,
    type_name: &'static str,
}

impl TypeSlot {
    pub fn of<T>() -> Self
    where
        T: ?Sized + 'static,
    {
        Self {
            type_id: TypeId::of::<T>(),
            type_name: std::any::type_name::<T>(),
        }
    }

    pub fn type_id(self) -> TypeId {
        self.type_id
    }

    pub fn type_name(self) -> &'static str {
        self.type_name
    }

    pub fn is<T>(self) -> bool
    where
        T: ?Sized + 'static,
    {
        self.type_id == TypeId::of::<T>()
    }
}

impl fmt::Debug for TypeSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TypeSlot")
            .field("type_id", &self.type_id)
            .field("type_name", &self.type_name)
            .finish()
    }
}

/// Named position inside a root turn-channel contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelTypeField {
    RootChannels,
    Output,
    Live,
    Commit,
    Diagnostic,
    TurnProps,
}

impl fmt::Display for ChannelTypeField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RootChannels => "root channels",
            Self::Output => "output",
            Self::Live => "live",
            Self::Commit => "commit",
            Self::Diagnostic => "diagnostic",
            Self::TurnProps => "turn props",
        })
    }
}

/// Complete process-local descriptor for one mounted root contract.
///
/// Local component channels have already been mapped to this root before the
/// descriptor is created. Lane identity remains explicit after erasure, so a
/// `Live` payload can never be interpreted as a `Commit` payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChannelTypeInfo {
    root_channels: TypeSlot,
    output: TypeSlot,
    live: TypeSlot,
    commit: TypeSlot,
    diagnostic: TypeSlot,
    turn_props: TypeSlot,
}

impl ChannelTypeInfo {
    pub fn of<C, TurnProps>() -> Self
    where
        C: TurnChannels,
        TurnProps: ?Sized + 'static,
    {
        Self {
            root_channels: TypeSlot::of::<C>(),
            output: TypeSlot::of::<C::Output>(),
            live: TypeSlot::of::<C::Live>(),
            commit: TypeSlot::of::<C::Commit>(),
            diagnostic: TypeSlot::of::<C::Diagnostic>(),
            turn_props: TypeSlot::of::<TurnProps>(),
        }
    }

    pub fn root_channels(&self) -> TypeSlot {
        self.root_channels
    }

    pub fn output(&self) -> TypeSlot {
        self.output
    }

    pub fn live(&self) -> TypeSlot {
        self.live
    }

    pub fn commit(&self) -> TypeSlot {
        self.commit
    }

    pub fn diagnostic(&self) -> TypeSlot {
        self.diagnostic
    }

    pub fn turn_props(&self) -> TypeSlot {
        self.turn_props
    }

    pub fn ensure<C, TurnProps>(&self) -> Result<(), ChannelTypeMismatch>
    where
        C: TurnChannels,
        TurnProps: ?Sized + 'static,
    {
        let requested = Self::of::<C, TurnProps>();
        for (field, mounted, requested) in [
            (
                ChannelTypeField::RootChannels,
                self.root_channels,
                requested.root_channels,
            ),
            (ChannelTypeField::Output, self.output, requested.output),
            (ChannelTypeField::Live, self.live, requested.live),
            (ChannelTypeField::Commit, self.commit, requested.commit),
            (
                ChannelTypeField::Diagnostic,
                self.diagnostic,
                requested.diagnostic,
            ),
            (
                ChannelTypeField::TurnProps,
                self.turn_props,
                requested.turn_props,
            ),
        ] {
            if mounted != requested {
                return Err(ChannelTypeMismatch {
                    field,
                    mounted,
                    requested,
                });
            }
        }
        Ok(())
    }
}

/// A typed handle requested a different contract than the mounted adapter.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "mounted {field} type is `{mounted}` but the typed handle requested `{requested}`",
    mounted = .mounted.type_name(),
    requested = .requested.type_name(),
)]
pub struct ChannelTypeMismatch {
    field: ChannelTypeField,
    mounted: TypeSlot,
    requested: TypeSlot,
}

impl ChannelTypeMismatch {
    pub fn field(&self) -> ChannelTypeField {
        self.field
    }

    pub fn mounted(&self) -> TypeSlot {
        self.mounted
    }

    pub fn requested(&self) -> TypeSlot {
        self.requested
    }
}

pub(super) struct ErasedMountedEpoch {
    channel_types: ChannelTypeInfo,
    state: Box<dyn std::any::Any + Send + Sync>,
}

impl ErasedMountedEpoch {
    pub(super) fn new<C, TurnProps, State>(state: State) -> Self
    where
        C: TurnChannels,
        TurnProps: ?Sized + 'static,
        State: Send + Sync + 'static,
    {
        Self {
            channel_types: ChannelTypeInfo::of::<C, TurnProps>(),
            state: Box::new(state),
        }
    }

    pub(super) fn channel_types(&self) -> &ChannelTypeInfo {
        &self.channel_types
    }

    pub(super) fn state<State>(&self) -> &State
    where
        State: Send + Sync + 'static,
    {
        self.state
            .downcast_ref::<State>()
            .expect("typed mounted-epoch handle must match its private erased adapter")
    }
}
