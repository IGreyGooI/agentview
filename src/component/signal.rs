//! Retained Component signal storage.
//!
//! A [`SignalRuntime`] lives with the mounted `ComponentHost`, not with an
//! individual provider attempt. Rendering is deliberately transactional:
//!
//! 1. call [`SignalRuntime::begin_render`];
//! 2. call [`SignalRenderTransaction::render_component`] once for each
//!    mounted component, in normal tree traversal order;
//! 3. call [`SignalRenderTransaction::commit`] only after the complete render
//!    and reconcile operation succeeds.
//!
//! Dropping an uncommitted transaction preserves the previously committed
//! hook topology and invalidates handles created by the abandoned render.

use std::{
    any::{type_name, Any, TypeId},
    cell::Cell,
    collections::{HashMap, HashSet},
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard,
    },
    thread::ThreadId,
};

#[cfg(test)]
use std::panic::Location;

use tokio::sync::Notify;

use super::ComponentId;

thread_local! {
    static SIGNAL_ACCESS_RUNTIME: Cell<Option<usize>> = const { Cell::new(None) };
}

struct SignalAccessScope {
    runtime: usize,
}

impl SignalAccessScope {
    fn enter<T>(signal: &Signal<T>) -> Result<Self, SignalAccessError> {
        let runtime = Self::runtime_id(&signal.runtime);
        SIGNAL_ACCESS_RUNTIME.with(|active| {
            if let Some(previous) = active.replace(Some(runtime)) {
                active.set(Some(previous));
                Err(SignalAccessError::ReentrantAccess {
                    component: signal.component.to_string(),
                    slot: signal.slot,
                })
            } else {
                Ok(Self { runtime })
            }
        })
    }

    fn is_active_for(runtime: &Arc<SignalRuntimeCore>) -> bool {
        let runtime = Self::runtime_id(runtime);
        SIGNAL_ACCESS_RUNTIME.with(|active| active.get() == Some(runtime))
    }

    fn runtime_id(runtime: &Arc<SignalRuntimeCore>) -> usize {
        Arc::as_ptr(runtime) as usize
    }
}

impl Drop for SignalAccessScope {
    fn drop(&mut self) {
        SIGNAL_ACCESS_RUNTIME.with(|active| {
            debug_assert_eq!(active.get(), Some(self.runtime));
            active.set(None);
        });
    }
}

struct SignalWriteInvalidation<'signal> {
    runtime: &'signal SignalRuntimeCore,
    component: &'signal ComponentId,
    armed: bool,
}

impl<'signal> SignalWriteInvalidation<'signal> {
    fn new(runtime: &'signal SignalRuntimeCore, component: &'signal ComponentId) -> Self {
        Self {
            runtime,
            component,
            armed: false,
        }
    }

    fn arm(&mut self) {
        self.armed = true;
    }
}

impl Drop for SignalWriteInvalidation<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.runtime.mark_component_dirty(self.component);
        }
    }
}

/// A typed, cloneable handle to one retained Component state slot.
pub struct Signal<T> {
    runtime: Arc<SignalRuntimeCore>,
    component: ComponentId,
    generation: u64,
    mount_fence: Arc<MountFence>,
    slot: usize,
    state: Arc<RwLock<T>>,
}

impl<T> Clone for Signal<T> {
    fn clone(&self) -> Self {
        Self {
            runtime: Arc::clone(&self.runtime),
            component: self.component.clone(),
            generation: self.generation,
            mount_fence: Arc::clone(&self.mount_fence),
            slot: self.slot,
            state: Arc::clone(&self.state),
        }
    }
}

impl<T> fmt::Debug for Signal<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Signal")
            .field("component", &self.component.as_str())
            .field("generation", &self.generation)
            .field("slot", &self.slot)
            .finish_non_exhaustive()
    }
}

impl<T> Signal<T>
where
    T: Send + Sync + 'static,
{
    /// Read the current value during one synchronous critical section.
    /// Lifecycle operations on the owning host fail closed during the callback.
    pub fn with<R>(&self, read: impl FnOnce(&T) -> R) -> Result<R, SignalAccessError> {
        if self.runtime.is_render_owner() {
            self.ensure_render_readable()?;
            let _access = SignalAccessScope::enter(self)?;
            return self.read_state(read);
        }

        let _access = SignalAccessScope::enter(self)?;
        let _gate = self.runtime.read_gate();
        self.ensure_externally_accessible()?;
        self.read_state(read)
    }

    /// Replace the current value and wake the Component scheduler.
    pub fn set(&self, next: T) -> Result<(), SignalAccessError> {
        self.update(|current| *current = next)
    }

    /// Mutate the current value during one synchronous critical section.
    ///
    /// A callback panic propagates after poisoning this slot's state lock. The
    /// owning host is still marked dirty and woken while the panic unwinds.
    pub fn update<R>(&self, update: impl FnOnce(&mut T) -> R) -> Result<R, SignalAccessError> {
        if self.runtime.is_render_owner() {
            self.ensure_render_readable()?;
            return Err(SignalAccessError::WriteDuringRender {
                component: self.component.to_string(),
                slot: self.slot,
            });
        }

        let _access = SignalAccessScope::enter(self)?;
        let _gate = self.runtime.read_gate();
        self.ensure_externally_accessible()?;
        let mut invalidation = SignalWriteInvalidation::new(self.runtime.as_ref(), &self.component);
        let mut state = self
            .state
            .write()
            .map_err(|_| self.state_poisoned_error())?;
        invalidation.arm();
        let result = update(&mut state);
        drop(state);
        drop(invalidation);
        Ok(result)
    }

    fn read_state<R>(&self, read: impl FnOnce(&T) -> R) -> Result<R, SignalAccessError> {
        let state = self.state.read().map_err(|_| self.state_poisoned_error())?;
        Ok(read(&state))
    }

    fn ensure_render_readable(&self) -> Result<(), SignalAccessError> {
        if !self.runtime.active.load(Ordering::Acquire) {
            return Err(SignalAccessError::RuntimeInactive);
        }
        if self.mount_fence.is_render_readable() {
            Ok(())
        } else {
            Err(self.stale_error())
        }
    }

    fn ensure_externally_accessible(&self) -> Result<(), SignalAccessError> {
        if !self.runtime.active.load(Ordering::Acquire) {
            return Err(SignalAccessError::RuntimeInactive);
        }
        if self.mount_fence.is_active() {
            Ok(())
        } else {
            Err(self.stale_error())
        }
    }

    fn stale_error(&self) -> SignalAccessError {
        SignalAccessError::Stale
    }

    fn state_poisoned_error(&self) -> SignalAccessError {
        SignalAccessError::StatePoisoned {
            component: self.component.to_string(),
            slot: self.slot,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MountState {
    Pending,
    Active,
    Stale,
}

/// One lock-backed ordering point shared by every capability from a mount.
///
/// Operations that can escape the render transaction must commit through
/// [`with_active`](Self::with_active). Unmount takes the same lock before it
/// closes the generation, so either the operation is accepted first and is
/// owned by cleanup, or it observes a stale mount and performs no work.
#[derive(Debug)]
pub(crate) struct MountFence {
    state: Mutex<MountState>,
}

impl MountFence {
    fn pending() -> Self {
        Self {
            state: Mutex::new(MountState::Pending),
        }
    }

    fn activate(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert_eq!(*state, MountState::Pending);
        if *state == MountState::Pending {
            *state = MountState::Active;
        }
    }

    fn invalidate(&self) {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = MountState::Stale;
    }

    fn is_render_readable(&self) -> bool {
        matches!(
            *self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            MountState::Pending | MountState::Active
        )
    }

    fn is_active(&self) -> bool {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            == MountState::Active
    }

    pub(crate) fn with_active<R>(&self, operation: impl FnOnce() -> R) -> Option<R> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *state != MountState::Active {
            return None;
        }
        Some(operation())
    }
}

/// Failures produced by direct access through a [`Signal`] handle.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SignalAccessError {
    #[error("the Component signal runtime is no longer active")]
    RuntimeInactive,

    #[error("the Signal handle belongs to a stale Component mount")]
    Stale,

    #[error("component `{component}` signal slot {slot} cannot be written during render")]
    WriteDuringRender { component: String, slot: usize },

    #[error(
        "component `{component}` signal slot {slot} cannot be accessed from inside another Signal callback"
    )]
    ReentrantAccess { component: String, slot: usize },

    #[error("component `{component}` signal slot {slot} state lock is poisoned")]
    StatePoisoned { component: String, slot: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookSite {
    Authoring(u32),
    #[cfg(test)]
    Runtime {
        file: &'static str,
        line: u32,
        column: u32,
    },
}

impl HookSite {
    #[cfg(test)]
    fn caller(location: &'static Location<'static>) -> Self {
        Self::Runtime {
            file: location.file(),
            line: location.line(),
            column: location.column(),
        }
    }
}

impl fmt::Display for HookSite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authoring(site) => write!(formatter, "component-hook-{site}"),
            #[cfg(test)]
            Self::Runtime { file, line, column } => write!(formatter, "{file}:{line}:{column}"),
        }
    }
}

/// Closed runtime classification for one lexical Component hook slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Phase 8 hook kinds are wired into authoring incrementally.
pub(crate) enum HookKind {
    Signal,
    Preparation,
    ProviderEventHandler,
    ReactionCompletion,
    ReactionRequest,
    Future,
    Coroutine,
}

/// Stable identity of one Component mount generation.
///
/// This value is intentionally independent from any particular capability so
/// render reconciliation can hand retired generations to lifecycle owners.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct MountIdentity {
    component: ComponentId,
    generation: u64,
}

impl MountIdentity {
    pub(crate) fn new(component: ComponentId, generation: u64) -> Self {
        Self {
            component,
            generation,
        }
    }

    pub(crate) fn component(&self) -> &ComponentId {
        &self.component
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }
}

impl fmt::Display for HookKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Signal => "signal",
            Self::Preparation => "preparation",
            Self::ProviderEventHandler => "provider-event-handler",
            Self::ReactionCompletion => "reaction-completion",
            Self::ReactionRequest => "reaction-request",
            Self::Future => "future",
            Self::Coroutine => "coroutine",
        };
        formatter.write_str(name)
    }
}

/// Hook-shape and render-transaction failures reported to `ComponentHost`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SignalRenderError {
    #[error("the Component signal runtime is no longer active")]
    RuntimeInactive,

    #[error("a signal render transaction cannot be nested on its owner thread")]
    NestedRender,

    #[error("a Component signal runtime cannot render or remount from inside its Signal callback")]
    CallbackReentry,

    #[error("component `{component}` was rendered more than once in one transaction")]
    DuplicateComponent { component: String },

    #[error("component `{component}` hook count changed from {expected} to {observed}")]
    HookCountMismatch {
        component: String,
        expected: usize,
        observed: usize,
    },

    #[error(
        "component `{component}` signal hook {slot} changed type from `{expected}` to `{observed}`"
    )]
    HookTypeMismatch {
        component: String,
        slot: usize,
        expected: &'static str,
        observed: &'static str,
    },

    #[error("component `{component}` hook {slot} changed kind from `{expected}` to `{observed}`")]
    HookKindMismatch {
        component: String,
        slot: usize,
        expected: HookKind,
        observed: HookKind,
    },

    #[error("component `{component}` hook {slot} moved from `{expected}` to `{observed}`")]
    HookLocationMismatch {
        component: String,
        slot: usize,
        expected: HookSite,
        observed: HookSite,
    },

    #[error(
        "component `{component}` hook {slot} changed retained type from `{expected}` to `{observed}`"
    )]
    HookRetainedTypeMismatch {
        component: String,
        slot: usize,
        expected: &'static str,
        observed: &'static str,
    },
}

trait ErasedHookSlot: Send + Sync {
    fn clone_box(&self) -> Box<dyn ErasedHookSlot>;
    fn kind(&self) -> HookKind;
    fn site(&self) -> HookSite;
    fn signal_state_type_id(&self) -> Option<TypeId>;
    fn signal_state_type_name(&self) -> Option<&'static str>;
    fn signal_state_any(&self) -> Option<&dyn Any>;
    fn retained_state_type_id(&self) -> Option<TypeId> {
        None
    }
    fn retained_state_type_name(&self) -> Option<&'static str> {
        None
    }
    fn retained_state_any(&self) -> Option<&dyn Any> {
        None
    }
}

struct TypedRetainedHookSlot<T> {
    state: Arc<T>,
    kind: HookKind,
    site: HookSite,
}

impl<T> ErasedHookSlot for TypedRetainedHookSlot<T>
where
    T: Send + Sync + 'static,
{
    fn clone_box(&self) -> Box<dyn ErasedHookSlot> {
        Box::new(Self {
            state: Arc::clone(&self.state),
            kind: self.kind,
            site: self.site,
        })
    }

    fn kind(&self) -> HookKind {
        self.kind
    }

    fn site(&self) -> HookSite {
        self.site
    }

    fn signal_state_type_id(&self) -> Option<TypeId> {
        None
    }

    fn signal_state_type_name(&self) -> Option<&'static str> {
        None
    }

    fn signal_state_any(&self) -> Option<&dyn Any> {
        None
    }

    fn retained_state_type_id(&self) -> Option<TypeId> {
        Some(TypeId::of::<T>())
    }

    fn retained_state_type_name(&self) -> Option<&'static str> {
        Some(type_name::<T>())
    }

    fn retained_state_any(&self) -> Option<&dyn Any> {
        Some(&self.state)
    }
}

struct TypedSignalSlot<T> {
    state: Arc<RwLock<T>>,
    site: HookSite,
}

impl<T> ErasedHookSlot for TypedSignalSlot<T>
where
    T: Send + Sync + 'static,
{
    fn clone_box(&self) -> Box<dyn ErasedHookSlot> {
        Box::new(Self {
            state: Arc::clone(&self.state),
            site: self.site,
        })
    }

    fn kind(&self) -> HookKind {
        HookKind::Signal
    }

    fn site(&self) -> HookSite {
        self.site
    }

    fn signal_state_type_id(&self) -> Option<TypeId> {
        Some(TypeId::of::<T>())
    }

    fn signal_state_type_name(&self) -> Option<&'static str> {
        Some(type_name::<T>())
    }

    fn signal_state_any(&self) -> Option<&dyn Any> {
        Some(&self.state)
    }
}

#[allow(dead_code)] // Constructed by the Phase 8 non-state authoring hooks.
struct MarkerHookSlot {
    kind: HookKind,
    site: HookSite,
}

impl ErasedHookSlot for MarkerHookSlot {
    fn clone_box(&self) -> Box<dyn ErasedHookSlot> {
        Box::new(Self {
            kind: self.kind,
            site: self.site,
        })
    }

    fn kind(&self) -> HookKind {
        self.kind
    }

    fn site(&self) -> HookSite {
        self.site
    }

    fn signal_state_type_id(&self) -> Option<TypeId> {
        None
    }

    fn signal_state_type_name(&self) -> Option<&'static str> {
        None
    }

    fn signal_state_any(&self) -> Option<&dyn Any> {
        None
    }
}

struct MountedSignalComponent {
    generation: u64,
    mount_fence: Arc<MountFence>,
    slots: Vec<Box<dyn ErasedHookSlot>>,
    pending: bool,
}

impl MountedSignalComponent {
    fn pending(generation: u64) -> Self {
        Self {
            generation,
            mount_fence: Arc::new(MountFence::pending()),
            slots: Vec::new(),
            pending: true,
        }
    }

    fn snapshot(&self) -> Self {
        Self {
            generation: self.generation,
            mount_fence: Arc::clone(&self.mount_fence),
            slots: self.slots.iter().map(|slot| slot.clone_box()).collect(),
            pending: false,
        }
    }

    fn activate(&mut self) {
        self.mount_fence.activate();
        self.pending = false;
    }

    fn invalidate(&self) {
        self.mount_fence.invalidate();
    }
}

impl Drop for MountedSignalComponent {
    fn drop(&mut self) {
        if self.pending {
            self.invalidate();
        }
    }
}

struct SignalRuntimeCore {
    active: AtomicBool,
    next_generation: AtomicU64,
    components: Mutex<HashMap<ComponentId, MountedSignalComponent>>,
    render_gate: RwLock<()>,
    #[cfg(test)]
    waiting_renderers: AtomicU64,
    render_owner: Mutex<Option<ThreadId>>,
    dirty: AtomicBool,
    dirty_components: Mutex<HashSet<ComponentId>>,
    wake_revision: AtomicU64,
    wake: Notify,
}

impl SignalRuntimeCore {
    fn read_gate(&self) -> RwLockReadGuard<'_, ()> {
        self.render_gate
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_gate(&self) -> RwLockWriteGuard<'_, ()> {
        #[cfg(test)]
        self.waiting_renderers.fetch_add(1, Ordering::AcqRel);
        let guard = self
            .render_gate
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        #[cfg(test)]
        self.waiting_renderers.fetch_sub(1, Ordering::AcqRel);
        guard
    }

    fn is_render_owner(&self) -> bool {
        self.render_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .is_some_and(|owner| *owner == std::thread::current().id())
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
        self.wake_revision.fetch_add(1, Ordering::AcqRel);
        self.wake.notify_waiters();
    }

    fn mark_component_dirty(&self, component: &ComponentId) {
        self.dirty_components
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(component.clone());
        self.mark_dirty();
    }

    fn next_generation(&self) -> u64 {
        self.next_generation
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .unwrap_or_else(|_| panic!("signal component generation space exhausted"))
    }
}

/// Host-owned retained state for all mounted Component signal slots.
pub(crate) struct SignalRuntime {
    core: Arc<SignalRuntimeCore>,
}

impl SignalRuntime {
    pub(crate) fn new() -> Self {
        Self {
            core: Arc::new(SignalRuntimeCore {
                active: AtomicBool::new(true),
                next_generation: AtomicU64::new(1),
                components: Mutex::new(HashMap::new()),
                render_gate: RwLock::new(()),
                #[cfg(test)]
                waiting_renderers: AtomicU64::new(0),
                render_owner: Mutex::new(None),
                dirty: AtomicBool::new(true),
                dirty_components: Mutex::new(HashSet::new()),
                wake_revision: AtomicU64::new(0),
                wake: Notify::new(),
            }),
        }
    }

    pub(crate) fn preflight_render(&self) -> Result<(), SignalRenderError> {
        if !self.core.active.load(Ordering::Acquire) {
            return Err(SignalRenderError::RuntimeInactive);
        }
        if self.core.is_render_owner() {
            return Err(SignalRenderError::NestedRender);
        }
        if SignalAccessScope::is_active_for(&self.core) {
            return Err(SignalRenderError::CallbackReentry);
        }
        Ok(())
    }

    /// Begin one complete tree render while excluding external Signal access.
    pub(crate) fn begin_render(&self) -> Result<SignalRenderTransaction<'_>, SignalRenderError> {
        self.preflight_render()?;

        let gate = self.core.write_gate();
        if !self.core.active.load(Ordering::Acquire) {
            return Err(SignalRenderError::RuntimeInactive);
        }
        *self
            .core
            .render_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(std::thread::current().id());

        Ok(SignalRenderTransaction {
            runtime: self,
            _gate: gate,
            staged: HashMap::new(),
        })
    }

    /// Mark host-owned inputs dirty without changing any Signal slot.
    pub(crate) fn mark_dirty(&self) {
        self.core.mark_dirty();
    }

    #[cfg(feature = "legacy-provider-port")]
    pub(crate) fn owns<T>(&self, signal: &Signal<T>) -> bool {
        Arc::ptr_eq(&self.core, &signal.runtime)
    }

    /// Explicitly remount the complete tree and fence every previously issued handle.
    pub(crate) fn invalidate_all(&self) -> Result<Vec<MountIdentity>, SignalRenderError> {
        if self.core.is_render_owner() {
            return Err(SignalRenderError::NestedRender);
        }
        if SignalAccessScope::is_active_for(&self.core) {
            return Err(SignalRenderError::CallbackReentry);
        }
        let _gate = self.core.write_gate();
        if !self.core.active.load(Ordering::Acquire) {
            return Err(SignalRenderError::RuntimeInactive);
        }
        let mut components = self
            .core
            .components
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let retired = components
            .iter()
            .map(|(identity, component)| MountIdentity::new(identity.clone(), component.generation))
            .collect::<Vec<_>>();
        for component in components.values() {
            component.invalidate();
        }
        components.clear();
        drop(components);
        self.core
            .dirty_components
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.core.mark_dirty();
        Ok(retired)
    }

    /// Explicitly unmount one identity, fencing every handle from its old generation.
    #[cfg(test)]
    pub(crate) fn remount_component(
        &self,
        component: &ComponentId,
    ) -> Result<bool, SignalRenderError> {
        if self.core.is_render_owner() {
            return Err(SignalRenderError::NestedRender);
        }
        if SignalAccessScope::is_active_for(&self.core) {
            return Err(SignalRenderError::CallbackReentry);
        }
        let _gate = self.core.write_gate();
        if !self.core.active.load(Ordering::Acquire) {
            return Err(SignalRenderError::RuntimeInactive);
        }
        let removed = self
            .core
            .components
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(component);
        if let Some(removed_component) = removed {
            removed_component.invalidate();
            self.core
                .dirty_components
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(component);
            self.core.mark_dirty();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.core.dirty.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn take_dirty(&self) -> bool {
        self.core.dirty.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn wake_revision(&self) -> u64 {
        self.core.wake_revision.load(Ordering::Acquire)
    }

    /// Wait until a write advances the supplied scheduler revision.
    pub(crate) async fn wait_for_wake_after(&self, observed: u64) -> u64 {
        loop {
            let notified = self.core.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let current = self.wake_revision();
            if current != observed {
                return current;
            }
            notified.await;
        }
    }

    #[cfg(test)]
    fn mounted_components(&self) -> usize {
        self.core
            .components
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    #[cfg(test)]
    fn waiting_renderers(&self) -> u64 {
        self.core.waiting_renderers.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn is_component_dirty(&self, component: &ComponentId) -> bool {
        self.core
            .dirty_components
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(component)
    }
}

impl Default for SignalRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SignalRuntime {
    fn drop(&mut self) {
        self.core.active.store(false, Ordering::Release);
        let _gate = if SignalAccessScope::is_active_for(&self.core) {
            None
        } else {
            Some(self.core.write_gate())
        };
        let components = self
            .core
            .components
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for component in components.values() {
            component.invalidate();
        }
        self.core.wake.notify_waiters();
    }
}

/// Stages one complete render without changing committed hook topology.
pub(crate) struct SignalRenderTransaction<'runtime> {
    runtime: &'runtime SignalRuntime,
    _gate: RwLockWriteGuard<'runtime, ()>,
    staged: HashMap<ComponentId, MountedSignalComponent>,
}

impl SignalRenderTransaction<'_> {
    /// Render one Component identity. The closure may call `scope.use_signal`.
    pub(crate) fn render_component<R>(
        &mut self,
        component: ComponentId,
        render: impl FnOnce(&mut SignalRenderScope) -> Result<R, SignalRenderError>,
    ) -> Result<R, SignalRenderError> {
        if self.staged.contains_key(&component) {
            return Err(SignalRenderError::DuplicateComponent {
                component: component.to_string(),
            });
        }

        let mounted = self
            .runtime
            .core
            .components
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&component)
            .map(MountedSignalComponent::snapshot)
            .unwrap_or_else(|| {
                MountedSignalComponent::pending(self.runtime.core.next_generation())
            });
        let was_pending = mounted.pending;
        let mut scope = SignalRenderScope {
            runtime: Arc::clone(&self.runtime.core),
            component: component.clone(),
            mounted,
            expected_shape: !was_pending,
            cursor: 0,
            fault: None,
        };

        let rendered = render(&mut scope);
        if let Err(fault) = &rendered {
            scope.record_fault(fault.clone());
        }
        let shape = scope.finish();
        match (rendered, shape) {
            (Err(fault), _) | (Ok(_), Err(fault)) => Err(fault),
            (Ok(value), Ok(mounted)) => {
                self.staged.insert(component, mounted);
                Ok(value)
            }
        }
    }

    /// Publish all staged hook shapes and immediately activate new mounts.
    pub(crate) fn commit(self) {
        self.commit_deferred().activate();
    }

    /// Publish hook topology and fence identities absent from this render.
    ///
    /// Newly introduced mounts remain pending until the returned transition is
    /// activated. This lets an async lifecycle owner retire and join tasks from
    /// old mounts before any capability from a replacement mount becomes live.
    pub(crate) fn commit_deferred(mut self) -> SignalMountTransition {
        let mut components = self
            .runtime
            .core
            .components
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut retired = Vec::new();
        for (identity, previous) in components.iter() {
            if !self.staged.contains_key(identity) {
                previous.invalidate();
                retired.push(MountIdentity::new(identity.clone(), previous.generation));
            }
        }
        let pending = self
            .staged
            .iter()
            .filter(|(_, component)| component.pending)
            .map(|(identity, component)| MountIdentity::new(identity.clone(), component.generation))
            .collect();
        *components = std::mem::take(&mut self.staged);
        self.runtime
            .core
            .dirty_components
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.runtime.core.dirty.store(false, Ordering::Release);
        SignalMountTransition {
            runtime: Arc::clone(&self.runtime.core),
            pending,
            retired,
            finished: false,
        }
    }
}

/// A committed hook topology waiting for replacement mounts to become active.
///
/// Dropping an unfinished transition removes its still-pending mounts and
/// marks the runtime dirty so a later render can safely recreate them.
pub(crate) struct SignalMountTransition {
    runtime: Arc<SignalRuntimeCore>,
    pending: Vec<MountIdentity>,
    retired: Vec<MountIdentity>,
    finished: bool,
}

impl SignalMountTransition {
    pub(crate) fn retired(&self) -> &[MountIdentity] {
        &self.retired
    }

    pub(crate) fn activate(mut self) {
        let mut components = self
            .runtime
            .components
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for identity in &self.pending {
            let component = components
                .get_mut(identity.component())
                .filter(|component| component.generation == identity.generation())
                .expect("pending Component mount must remain installed until activation");
            component.activate();
        }
        self.finished = true;
    }
}

impl Drop for SignalMountTransition {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut components = self
            .runtime
            .components
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for identity in &self.pending {
            let should_remove = components
                .get(identity.component())
                .is_some_and(|component| {
                    component.pending && component.generation == identity.generation()
                });
            if should_remove {
                components.remove(identity.component());
            }
        }
        drop(components);
        self.runtime.mark_dirty();
    }
}

impl Drop for SignalRenderTransaction<'_> {
    fn drop(&mut self) {
        *self
            .runtime
            .core
            .render_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

/// Hook authority for one Component invocation inside a render transaction.
pub(crate) struct SignalRenderScope {
    runtime: Arc<SignalRuntimeCore>,
    component: ComponentId,
    mounted: MountedSignalComponent,
    expected_shape: bool,
    cursor: usize,
    fault: Option<SignalRenderError>,
}

impl SignalRenderScope {
    #[cfg(test)]
    #[track_caller]
    pub(crate) fn use_signal<T>(
        &mut self,
        initialize: impl FnOnce() -> T,
    ) -> Result<Signal<T>, SignalRenderError>
    where
        T: Send + Sync + 'static,
    {
        self.use_signal_with_site(HookSite::caller(Location::caller()), initialize)
    }

    pub(crate) fn use_signal_at<T>(
        &mut self,
        site: u32,
        initialize: impl FnOnce() -> T,
    ) -> Result<Signal<T>, SignalRenderError>
    where
        T: Send + Sync + 'static,
    {
        self.use_signal_with_site(HookSite::Authoring(site), initialize)
    }

    fn use_signal_with_site<T>(
        &mut self,
        site: HookSite,
        initialize: impl FnOnce() -> T,
    ) -> Result<Signal<T>, SignalRenderError>
    where
        T: Send + Sync + 'static,
    {
        if let Some(fault) = &self.fault {
            return Err(fault.clone());
        }

        let slot_index = self.cursor;
        let state = if self.expected_shape {
            let Some(slot) = self.mounted.slots.get(slot_index) else {
                let fault = SignalRenderError::HookCountMismatch {
                    component: self.component.to_string(),
                    expected: self.mounted.slots.len(),
                    observed: slot_index + 1,
                };
                self.record_fault(fault.clone());
                return Err(fault);
            };
            if slot.kind() != HookKind::Signal {
                let fault = SignalRenderError::HookKindMismatch {
                    component: self.component.to_string(),
                    slot: slot_index,
                    expected: slot.kind(),
                    observed: HookKind::Signal,
                };
                self.record_fault(fault.clone());
                return Err(fault);
            }
            if slot.signal_state_type_id() != Some(TypeId::of::<T>()) {
                let fault = SignalRenderError::HookTypeMismatch {
                    component: self.component.to_string(),
                    slot: slot_index,
                    expected: slot
                        .signal_state_type_name()
                        .expect("a signal hook slot has a state type"),
                    observed: type_name::<T>(),
                };
                self.record_fault(fault.clone());
                return Err(fault);
            }
            if slot.site() != site {
                let fault = SignalRenderError::HookLocationMismatch {
                    component: self.component.to_string(),
                    slot: slot_index,
                    expected: slot.site(),
                    observed: site,
                };
                self.record_fault(fault.clone());
                return Err(fault);
            }
            slot.signal_state_any()
                .expect("a signal hook slot has state")
                .downcast_ref::<Arc<RwLock<T>>>()
                .expect("matching signal TypeId must downcast")
                .clone()
        } else {
            let state = Arc::new(RwLock::new(initialize()));
            self.mounted.slots.push(Box::new(TypedSignalSlot {
                state: Arc::clone(&state),
                site,
            }));
            state
        };

        self.cursor += 1;
        Ok(Signal {
            runtime: Arc::clone(&self.runtime),
            component: self.component.clone(),
            generation: self.mounted.generation,
            mount_fence: Arc::clone(&self.mounted.mount_fence),
            slot: slot_index,
            state,
        })
    }

    #[allow(dead_code)] // Called by the Phase 8 non-state authoring hooks.
    pub(crate) fn use_marker_at(
        &mut self,
        site: u32,
        kind: HookKind,
    ) -> Result<HookMount, SignalRenderError> {
        self.use_marker_with_site(HookSite::Authoring(site), kind)
    }

    #[allow(dead_code)] // Called by the Phase 8 non-state authoring hooks.
    fn use_marker_with_site(
        &mut self,
        site: HookSite,
        kind: HookKind,
    ) -> Result<HookMount, SignalRenderError> {
        if let Some(fault) = &self.fault {
            return Err(fault.clone());
        }

        let slot_index = self.cursor;
        if self.expected_shape {
            let Some(slot) = self.mounted.slots.get(slot_index) else {
                let fault = SignalRenderError::HookCountMismatch {
                    component: self.component.to_string(),
                    expected: self.mounted.slots.len(),
                    observed: slot_index + 1,
                };
                self.record_fault(fault.clone());
                return Err(fault);
            };
            if slot.kind() != kind {
                let fault = SignalRenderError::HookKindMismatch {
                    component: self.component.to_string(),
                    slot: slot_index,
                    expected: slot.kind(),
                    observed: kind,
                };
                self.record_fault(fault.clone());
                return Err(fault);
            }
            if slot.site() != site {
                let fault = SignalRenderError::HookLocationMismatch {
                    component: self.component.to_string(),
                    slot: slot_index,
                    expected: slot.site(),
                    observed: site,
                };
                self.record_fault(fault.clone());
                return Err(fault);
            }
        } else {
            self.mounted
                .slots
                .push(Box::new(MarkerHookSlot { kind, site }));
        }

        self.cursor += 1;
        Ok(HookMount {
            component: self.component.clone(),
            generation: self.mounted.generation,
            mount_fence: Arc::clone(&self.mounted.mount_fence),
            slot: slot_index,
            new_mount: self.mounted.pending,
        })
    }

    pub(crate) fn use_retained_at<T>(
        &mut self,
        site: u32,
        kind: HookKind,
        initialize: impl FnOnce() -> T,
    ) -> Result<(HookMount, Arc<T>), SignalRenderError>
    where
        T: Send + Sync + 'static,
    {
        if let Some(fault) = &self.fault {
            return Err(fault.clone());
        }

        let site = HookSite::Authoring(site);
        let slot_index = self.cursor;
        let state = if self.expected_shape {
            let Some(slot) = self.mounted.slots.get(slot_index) else {
                let fault = SignalRenderError::HookCountMismatch {
                    component: self.component.to_string(),
                    expected: self.mounted.slots.len(),
                    observed: slot_index + 1,
                };
                self.record_fault(fault.clone());
                return Err(fault);
            };
            if slot.kind() != kind {
                let fault = SignalRenderError::HookKindMismatch {
                    component: self.component.to_string(),
                    slot: slot_index,
                    expected: slot.kind(),
                    observed: kind,
                };
                self.record_fault(fault.clone());
                return Err(fault);
            }
            if slot.site() != site {
                let fault = SignalRenderError::HookLocationMismatch {
                    component: self.component.to_string(),
                    slot: slot_index,
                    expected: slot.site(),
                    observed: site,
                };
                self.record_fault(fault.clone());
                return Err(fault);
            }
            if slot.retained_state_type_id() != Some(TypeId::of::<T>()) {
                let fault = SignalRenderError::HookRetainedTypeMismatch {
                    component: self.component.to_string(),
                    slot: slot_index,
                    expected: slot
                        .retained_state_type_name()
                        .unwrap_or("non-retained hook state"),
                    observed: type_name::<T>(),
                };
                self.record_fault(fault.clone());
                return Err(fault);
            }
            slot.retained_state_any()
                .expect("a retained hook slot has state")
                .downcast_ref::<Arc<T>>()
                .expect("matching retained TypeId must downcast")
                .clone()
        } else {
            let state = Arc::new(initialize());
            self.mounted.slots.push(Box::new(TypedRetainedHookSlot {
                state: Arc::clone(&state),
                kind,
                site,
            }));
            state
        };

        self.cursor += 1;
        Ok((
            HookMount {
                component: self.component.clone(),
                generation: self.mounted.generation,
                mount_fence: Arc::clone(&self.mounted.mount_fence),
                slot: slot_index,
                new_mount: self.mounted.pending,
            },
            state,
        ))
    }

    fn record_fault(&mut self, fault: SignalRenderError) {
        if self.fault.is_none() {
            self.fault = Some(fault);
        }
    }

    fn finish(mut self) -> Result<MountedSignalComponent, SignalRenderError> {
        if self.fault.is_none() && self.expected_shape && self.cursor != self.mounted.slots.len() {
            self.fault = Some(SignalRenderError::HookCountMismatch {
                component: self.component.to_string(),
                expected: self.mounted.slots.len(),
                observed: self.cursor,
            });
        }
        match self.fault {
            Some(fault) => Err(fault),
            None => Ok(self.mounted),
        }
    }
}

/// Mount identity returned to non-state hooks after topology validation.
#[derive(Clone)]
#[allow(dead_code)] // Consumed by Phase 8 binding and task lifecycle fences.
pub(crate) struct HookMount {
    pub(crate) component: ComponentId,
    pub(crate) generation: u64,
    mount_fence: Arc<MountFence>,
    pub(crate) slot: usize,
    new_mount: bool,
}

pub(crate) struct MountOperationPermit {
    _fence: Arc<MountFence>,
}

impl HookMount {
    pub(crate) const fn is_new_mount(&self) -> bool {
        self.new_mount
    }
    pub(crate) fn authorize(&self) -> Option<MountOperationPermit> {
        self.mount_fence.with_active(|| MountOperationPermit {
            _fence: Arc::clone(&self.mount_fence),
        })
    }

    #[allow(dead_code)] // Used by Phase 8 demand/task registration in the next slice.
    pub(crate) fn with_active<R>(&self, operation: impl FnOnce() -> R) -> Option<R> {
        self.mount_fence.with_active(operation)
    }
}

#[cfg(test)]
mod tests;
