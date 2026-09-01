use std::sync::{Arc, RwLock};

#[allow(
    deprecated,
    reason = "this feature-gated runtime retains the legacy EventInput constructor contract"
)]
use crate::component::{
    authoring::{Component, EventInput, Signal},
    ComponentHost, ComponentHostFault, ComponentHostId, SignalAccessError,
};

#[allow(
    deprecated,
    reason = "this feature-gated runtime composes retained ApplicationHost and ProviderPort APIs"
)]
use super::{ApplicationHost, ApplicationHostFault, ProviderEvent, ProviderPort};

/// Monotonic identity of one mount in a [`ComponentReactionRuntime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComponentReactionGeneration(u64);

impl ComponentReactionGeneration {
    const INITIAL: Self = Self(1);

    fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// Immutable domain props plus the capability to publish authoritative output.
pub struct ComponentReactionProps<Props, Output> {
    value: Props,
    publisher: ComponentReactionPublisher<Output>,
}

impl<Props, Output> ComponentReactionProps<Props, Output> {
    fn new(
        value: Props,
        generation: ComponentReactionGeneration,
        outputs: Arc<ComponentReactionOutputs<Output>>,
    ) -> Self {
        Self {
            value,
            publisher: ComponentReactionPublisher {
                generation,
                outputs,
            },
        }
    }

    /// Return the immutable domain props for this mount.
    pub fn value(&self) -> &Props {
        &self.value
    }
}

impl<Props, Output> ComponentReactionProps<Props, Output>
where
    Output: Send + Sync + 'static,
{
    /// Select a mounted Signal as this reaction's authoritative typed output.
    ///
    /// Publication is pending until the complete Provider reaction and all
    /// event and EOF diagnostic handlers succeed. The runtime validates Signal
    /// provenance before committing it as current output.
    pub fn publish(&self, signal: Signal<Output>) -> Result<(), ComponentReactionOutputError> {
        self.publisher.publish(signal)
    }
}

impl<Props, Output> Clone for ComponentReactionProps<Props, Output>
where
    Props: Clone,
{
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            publisher: self.publisher.clone(),
        }
    }
}

struct ComponentReactionPublisher<Output> {
    generation: ComponentReactionGeneration,
    outputs: Arc<ComponentReactionOutputs<Output>>,
}

impl<Output> ComponentReactionPublisher<Output>
where
    Output: Send + Sync + 'static,
{
    fn publish(&self, signal: Signal<Output>) -> Result<(), ComponentReactionOutputError> {
        self.outputs.publish(self.generation, signal)
    }
}

impl<Output> Clone for ComponentReactionPublisher<Output> {
    fn clone(&self) -> Self {
        Self {
            generation: self.generation,
            outputs: Arc::clone(&self.outputs),
        }
    }
}

struct CommittedOutput<Output> {
    generation: ComponentReactionGeneration,
    selection: u64,
    signal: Signal<Output>,
}

struct ComponentReactionOutputState<Output> {
    active_generation: ComponentReactionGeneration,
    next_selection: u64,
    reaction_active: bool,
    pending: Option<Signal<Output>>,
    committed: Option<CommittedOutput<Output>>,
}

struct ComponentReactionOutputs<Output> {
    state: RwLock<ComponentReactionOutputState<Output>>,
}

impl<Output> ComponentReactionOutputs<Output>
where
    Output: Send + Sync + 'static,
{
    fn new(generation: ComponentReactionGeneration) -> Self {
        Self {
            state: RwLock::new(ComponentReactionOutputState {
                active_generation: generation,
                next_selection: 1,
                reaction_active: false,
                pending: None,
                committed: None,
            }),
        }
    }

    fn activate(&self, generation: ComponentReactionGeneration) {
        let retired_pending = {
            let mut state = self.write_state();
            state.active_generation = generation;
            state.reaction_active = false;
            state.pending.take()
        };
        drop(retired_pending);
    }

    fn begin(
        self: &Arc<Self>,
        generation: ComponentReactionGeneration,
    ) -> Result<ComponentReactionOutputTransaction<Output>, ComponentReactionOutputError> {
        let (retired_pending, retired_committed) = {
            let mut state = self.write_state();
            Self::ensure_generation(&state, generation)?;
            if state.reaction_active {
                return Err(ComponentReactionOutputError::ReactionInProgress { generation });
            }
            state.reaction_active = true;
            (state.pending.take(), state.committed.take())
        };
        drop(retired_pending);
        drop(retired_committed);
        Ok(ComponentReactionOutputTransaction {
            generation,
            outputs: Arc::clone(self),
            finished: false,
        })
    }

    fn publish(
        &self,
        generation: ComponentReactionGeneration,
        signal: Signal<Output>,
    ) -> Result<(), ComponentReactionOutputError> {
        let retired = {
            let mut state = self.write_state();
            Self::ensure_generation(&state, generation)?;
            if !state.reaction_active {
                return Err(ComponentReactionOutputError::PublicationOutsideReaction {
                    generation,
                });
            }
            state.pending.replace(signal)
        };
        drop(retired);
        Ok(())
    }

    fn close(
        &self,
        generation: ComponentReactionGeneration,
    ) -> Result<Signal<Output>, ComponentReactionOutputError> {
        let mut state = self.write_state();
        Self::ensure_generation(&state, generation)?;
        state.reaction_active = false;
        state
            .pending
            .take()
            .ok_or(ComponentReactionOutputError::Absent { generation })
    }

    fn snapshot_pending(
        &self,
        generation: ComponentReactionGeneration,
    ) -> Result<Signal<Output>, ComponentReactionOutputError> {
        let state = self.read_state();
        Self::ensure_generation(&state, generation)?;
        if !state.reaction_active {
            return Err(ComponentReactionOutputError::PublicationOutsideReaction { generation });
        }
        state
            .pending
            .clone()
            .ok_or(ComponentReactionOutputError::Absent { generation })
    }

    fn commit(
        self: &Arc<Self>,
        generation: ComponentReactionGeneration,
        signal: Signal<Output>,
    ) -> Result<ComponentReactionOutput<Output>, ComponentReactionOutputError> {
        let (selection, retired) = {
            let mut state = self.write_state();
            Self::ensure_generation(&state, generation)?;
            let selection = state.next_selection;
            state.next_selection = selection
                .checked_add(1)
                .ok_or(ComponentReactionOutputError::SelectionExhausted)?;
            let retired = state.committed.replace(CommittedOutput {
                generation,
                selection,
                signal,
            });
            (selection, retired)
        };
        drop(retired);
        Ok(ComponentReactionOutput {
            generation,
            selection,
            outputs: Arc::clone(self),
        })
    }

    fn abort(&self, generation: ComponentReactionGeneration) {
        let (retired_pending, retired_committed) = {
            let mut state = self.write_state();
            if state.active_generation == generation {
                state.reaction_active = false;
                (state.pending.take(), state.committed.take())
            } else {
                (None, None)
            }
        };
        drop(retired_pending);
        drop(retired_committed);
    }

    fn current(
        self: &Arc<Self>,
    ) -> Result<ComponentReactionOutput<Output>, ComponentReactionOutputError> {
        let state = self.read_state();
        let Some(committed) = state.committed.as_ref() else {
            return Err(ComponentReactionOutputError::Absent {
                generation: state.active_generation,
            });
        };
        if committed.generation != state.active_generation {
            return Err(ComponentReactionOutputError::StaleGeneration {
                output_generation: committed.generation,
                current_generation: state.active_generation,
            });
        }
        Ok(ComponentReactionOutput {
            generation: committed.generation,
            selection: committed.selection,
            outputs: Arc::clone(self),
        })
    }

    fn signal_for(
        &self,
        generation: ComponentReactionGeneration,
        selection: u64,
    ) -> Result<Signal<Output>, ComponentReactionOutputError> {
        let state = self.read_state();
        Self::ensure_generation(&state, generation)?;
        let Some(committed) = state.committed.as_ref() else {
            return Err(ComponentReactionOutputError::Absent { generation });
        };
        if committed.selection != selection {
            return Err(ComponentReactionOutputError::StaleSelection { generation });
        }
        Ok(committed.signal.clone())
    }

    fn ensure_selection(
        &self,
        generation: ComponentReactionGeneration,
        selection: u64,
    ) -> Result<(), ComponentReactionOutputError> {
        self.signal_for(generation, selection).map(|_| ())
    }

    fn ensure_generation(
        state: &ComponentReactionOutputState<Output>,
        generation: ComponentReactionGeneration,
    ) -> Result<(), ComponentReactionOutputError> {
        if generation == state.active_generation {
            Ok(())
        } else {
            Err(ComponentReactionOutputError::StaleGeneration {
                output_generation: generation,
                current_generation: state.active_generation,
            })
        }
    }

    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, ComponentReactionOutputState<Output>> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, ComponentReactionOutputState<Output>> {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct ComponentReactionOutputTransaction<Output>
where
    Output: Send + Sync + 'static,
{
    generation: ComponentReactionGeneration,
    outputs: Arc<ComponentReactionOutputs<Output>>,
    finished: bool,
}

impl<Output> ComponentReactionOutputTransaction<Output>
where
    Output: Send + Sync + 'static,
{
    fn close(&self) -> Result<Signal<Output>, ComponentReactionOutputError> {
        self.outputs.close(self.generation)
    }

    fn snapshot_pending(&self) -> Result<Signal<Output>, ComponentReactionOutputError> {
        self.outputs.snapshot_pending(self.generation)
    }

    fn commit(
        mut self,
        signal: Signal<Output>,
    ) -> Result<ComponentReactionOutput<Output>, ComponentReactionOutputError> {
        let output = self.outputs.commit(self.generation, signal)?;
        self.finished = true;
        Ok(output)
    }
}

impl<Output> Drop for ComponentReactionOutputTransaction<Output>
where
    Output: Send + Sync + 'static,
{
    fn drop(&mut self) {
        if !self.finished {
            self.outputs.abort(self.generation);
        }
    }
}

/// A typed output receipt that remains valid only while its selection is current.
pub struct ComponentReactionOutput<Output> {
    generation: ComponentReactionGeneration,
    selection: u64,
    outputs: Arc<ComponentReactionOutputs<Output>>,
}

impl<Output> ComponentReactionOutput<Output>
where
    Output: Send + Sync + 'static,
{
    /// Return the Component mount generation that produced this receipt.
    pub fn generation(&self) -> ComponentReactionGeneration {
        self.generation
    }

    /// Read the current authoritative value while this output selection is active.
    pub fn with<R>(
        &self,
        read: impl FnOnce(&Output) -> R,
    ) -> Result<R, ComponentReactionOutputError> {
        let signal = self.outputs.signal_for(self.generation, self.selection)?;
        let result = signal.with(read);
        self.outputs
            .ensure_selection(self.generation, self.selection)?;
        result.map_err(ComponentReactionOutputError::Signal)
    }

    /// Clone the current authoritative value while this output selection is active.
    pub fn cloned(&self) -> Result<Output, ComponentReactionOutputError>
    where
        Output: Clone,
    {
        self.with(Clone::clone)
    }
}

impl<Output> Clone for ComponentReactionOutput<Output> {
    fn clone(&self) -> Self {
        Self {
            generation: self.generation,
            selection: self.selection,
            outputs: Arc::clone(&self.outputs),
        }
    }
}

/// Owns one stable Component/Application host pair for explicit single reactions.
#[allow(
    deprecated,
    reason = "the compatibility runtime stores the retained deprecated ApplicationHost"
)]
#[deprecated(note = "use `Application<P>` as the mounted runtime owner")]
pub struct ComponentReactionRuntime<P, Props, Output> {
    application: ApplicationHost<P>,
    components: ComponentHost<ComponentReactionProps<Props, Output>>,
    outputs: Arc<ComponentReactionOutputs<Output>>,
    generation: ComponentReactionGeneration,
    reaction_dispatched: bool,
}

#[allow(
    deprecated,
    reason = "methods implement the deprecated ComponentReactionRuntime constructor and ownership API"
)]
impl<P, Props, Output> ComponentReactionRuntime<P, Props, Output>
where
    Output: Send + Sync + 'static,
{
    pub fn new(
        provider: P,
        root: fn(ComponentReactionProps<Props, Output>, EventInput<ProviderEvent>) -> Component,
        props: Props,
    ) -> Self {
        let generation = ComponentReactionGeneration::INITIAL;
        let outputs = Arc::new(ComponentReactionOutputs::new(generation));
        let mounted_props = ComponentReactionProps::new(props, generation, Arc::clone(&outputs));
        Self {
            application: ApplicationHost::new(provider),
            components: ComponentHost::new(root, mounted_props),
            outputs,
            generation,
            reaction_dispatched: false,
        }
    }

    /// Return the stable identity of the owned ComponentHost.
    pub fn component_host_id(&self) -> ComponentHostId {
        self.components.id()
    }

    /// Return the current immutable domain props.
    pub fn props(&self) -> &Props {
        self.components.props().value()
    }

    /// Replace immutable props for the next reaction while retaining this mount.
    ///
    /// Retained Signals and the Provider projection-diff scope stay valid. The
    /// output generation still advances so receipts from the previous reaction
    /// cannot be read as current state.
    pub fn set_props(
        &mut self,
        props: Props,
    ) -> Result<ComponentHostId, ComponentReactionRuntimeFault> {
        let next = self
            .generation
            .next()
            .ok_or(ComponentReactionRuntimeFault::GenerationExhausted)?;
        let mounted_props = ComponentReactionProps::new(props, next, Arc::clone(&self.outputs));
        self.components.set_props(mounted_props);
        self.generation = next;
        self.outputs.activate(next);
        self.reaction_dispatched = false;
        Ok(self.components.id())
    }

    /// Return the last fully committed output for the current Component generation.
    pub fn current_output(
        &self,
    ) -> Result<ComponentReactionOutput<Output>, ComponentReactionOutputError> {
        self.outputs.current()
    }

    /// Remount fresh immutable props while retaining the host pair and host identity.
    pub fn remount(
        &mut self,
        props: Props,
    ) -> Result<ComponentHostId, ComponentReactionRuntimeFault> {
        let next = self
            .generation
            .next()
            .ok_or(ComponentReactionRuntimeFault::GenerationExhausted)?;
        let mounted_props = ComponentReactionProps::new(props, next, Arc::clone(&self.outputs));
        let host_id = self.components.remount(mounted_props)?;
        self.generation = next;
        self.outputs.activate(next);
        self.reaction_dispatched = false;
        Ok(host_id)
    }
}

#[allow(
    deprecated,
    reason = "dispatch preserves the feature-gated ProviderPort compatibility contract"
)]
impl<P, Props, Output> ComponentReactionRuntime<P, Props, Output>
where
    P: ProviderPort,
    Props: Clone + Send + 'static,
    Output: Send + Sync + 'static,
{
    /// Render and execute exactly one Provider reaction, then commit current output.
    pub async fn dispatch_llm_reaction(
        &mut self,
    ) -> Result<ComponentReactionOutput<Output>, ComponentReactionRuntimeFault> {
        if self.reaction_dispatched {
            return Err(ComponentReactionRuntimeFault::ReactionAlreadyDispatched {
                generation: self.generation,
            });
        }
        self.reaction_dispatched = true;
        let transaction = self.outputs.begin(self.generation)?;
        let signal = self
            .application
            .dispatch_llm_reaction_with_pre_reconcile_capture(&mut self.components, || {
                transaction.snapshot_pending()
            })
            .await??;
        let _post_reconcile_selection = transaction.close()?;
        if !self.components.owns_signal(&signal) {
            return Err(ComponentReactionOutputError::ForeignSignal {
                generation: self.generation,
            }
            .into());
        }
        signal
            .with(|_| ())
            .map_err(ComponentReactionOutputError::Signal)?;
        Ok(transaction.commit(signal)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ComponentReactionOutputError {
    #[error("Component generation {generation:?} did not publish a committed output")]
    Absent {
        generation: ComponentReactionGeneration,
    },
    #[error(
        "output belongs to Component generation {output_generation:?}, not current generation {current_generation:?}"
    )]
    StaleGeneration {
        output_generation: ComponentReactionGeneration,
        current_generation: ComponentReactionGeneration,
    },
    #[error("output was replaced within Component generation {generation:?}")]
    StaleSelection {
        generation: ComponentReactionGeneration,
    },
    #[error("Component generation {generation:?} published a Signal owned by another host")]
    ForeignSignal {
        generation: ComponentReactionGeneration,
    },
    #[error("Component generation {generation:?} published outside an active reaction")]
    PublicationOutsideReaction {
        generation: ComponentReactionGeneration,
    },
    #[error("Component generation {generation:?} already has an active reaction")]
    ReactionInProgress {
        generation: ComponentReactionGeneration,
    },
    #[error("Component output selection space is exhausted")]
    SelectionExhausted,
    #[error("published Component Signal is not readable: {0}")]
    Signal(SignalAccessError),
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ComponentReactionRuntimeFault {
    #[error(transparent)]
    Application(#[from] ApplicationHostFault),
    #[error(transparent)]
    Component(#[from] ComponentHostFault),
    #[error(transparent)]
    Output(#[from] ComponentReactionOutputError),
    #[error("Component generation {generation:?} already consumed its reaction attempt")]
    ReactionAlreadyDispatched {
        generation: ComponentReactionGeneration,
    },
    #[error("Component reaction generation space is exhausted")]
    GenerationExhausted,
}
