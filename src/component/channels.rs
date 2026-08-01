//! Typed result and effect lanes shared by provided components.

/// Harness-level channel contract used to normalize local provided components.
pub trait TurnChannels: Send + Sync + 'static {
    type Output: Send + 'static;
    type Live: Send + 'static;
    /// Post-publication effects are delivered by an async retry owner. They
    /// must be shareable while that owner retains the published batch.
    type Commit: Send + Sync + 'static;
    type Diagnostic: Send + 'static;
}

/// Uninhabited type for a channel that a harness does not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Never {}

impl Never {
    /// Convert an impossible lane value into any target type.
    pub fn absurd<T>(self) -> T {
        match self {}
    }
}

/// Empty harness channel contract for a POM-only mounted feature.
///
/// Use this when a harness has no provided component output, Live effect,
/// Commit effect, or diagnostic lane. It avoids making a prompt-only author
/// repeat the four `Never` associated types solely to construct a
/// [`MountedFeature`](super::MountedFeature).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct NoTurnChannels;

impl TurnChannels for NoTurnChannels {
    type Output = Never;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = Never;
}

/// One typed value emitted by a reducer into a harness-owned lane.
///
/// This type classifies values only. The host runtime still decides when each
/// lane is interpreted and how live or commit effects are cancelled/retried.
pub enum TurnEmission<C>
where
    C: TurnChannels,
{
    Output(C::Output),
    Live(C::Live),
    Commit(C::Commit),
}

impl<C> std::fmt::Debug for TurnEmission<C>
where
    C: TurnChannels,
    C::Output: std::fmt::Debug,
    C::Live: std::fmt::Debug,
    C::Commit: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Output(output) => formatter.debug_tuple("Output").field(output).finish(),
            Self::Live(effect) => formatter.debug_tuple("Live").field(effect).finish(),
            Self::Commit(effect) => formatter.debug_tuple("Commit").field(effect).finish(),
        }
    }
}

impl<C> Clone for TurnEmission<C>
where
    C: TurnChannels,
    C::Output: Clone,
    C::Live: Clone,
    C::Commit: Clone,
{
    fn clone(&self) -> Self {
        match self {
            Self::Output(output) => Self::Output(output.clone()),
            Self::Live(effect) => Self::Live(effect.clone()),
            Self::Commit(effect) => Self::Commit(effect.clone()),
        }
    }
}

impl<C> PartialEq for TurnEmission<C>
where
    C: TurnChannels,
    C::Output: PartialEq,
    C::Live: PartialEq,
    C::Commit: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Output(left), Self::Output(right)) => left == right,
            (Self::Live(left), Self::Live(right)) => left == right,
            (Self::Commit(left), Self::Commit(right)) => left == right,
            _ => false,
        }
    }
}

impl<C> Eq for TurnEmission<C>
where
    C: TurnChannels,
    C::Output: Eq,
    C::Live: Eq,
    C::Commit: Eq,
{
}

#[cfg(test)]
mod tests {
    use super::{Never, NoTurnChannels, TurnChannels};

    fn assert_prompt_only<C>()
    where
        C: TurnChannels<Output = Never, Live = Never, Commit = Never, Diagnostic = Never>,
    {
    }

    #[test]
    fn no_turn_channels_has_only_impossible_lanes() {
        assert_prompt_only::<NoTurnChannels>();
    }
}
