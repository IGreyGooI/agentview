//! Durable System-epoch creation and rehydration contracts.
//!
//! This module is intentionally crate-private while the mounted facade is
//! being proven. It separates the one-shot [`SystemView`](super::SystemView)
//! authoring boundary from process-local runtime rebinding. Reopening an
//! active durable epoch never calls `SystemView` and never gives System bytes
//! to the provider rehydration path.

use std::{
    error::Error,
    fmt,
    num::{NonZeroU64, NonZeroUsize},
    sync::Arc,
};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    pom::{Document, ResolvedDocument},
    pom_renderer::{render_pom_document, PomRenderError},
    StorageString,
};

use super::provision::DurableSystem;
use super::{
    BindingFactoryPlan, DurableEpochId, DurableSessionId, EpochContractId, ProviderCapabilityPlan,
    ProviderToolSpec, TurnChannels,
};

/// Version 4 adds compiled component-binding identities to the durable
/// manifest. Earlier artifacts cannot prove that a reopen uses the same
/// keyed component tree, so they are intentionally rejected rather than
/// inferred.
pub(crate) const EPOCH_ARTIFACT_FORMAT_VERSION: u32 = 4;

fn validate_epoch_key(
    value: impl Into<StorageString>,
    kind: &'static str,
) -> Result<StorageString, EpochContractManifestError> {
    let value = value.into();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(EpochContractManifestError::InvalidKey {
            kind,
            value: value.to_string(),
        });
    }
    Ok(value)
}

/// Error while constructing the public provider-adapter boundary.
///
/// This deliberately avoids exposing durable manifest validation details to a
/// provider adapter. The mounted owner validates the complete persisted epoch
/// artifact separately.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderAdapterError {
    #[error(
        "invalid provider adapter identifier `{value}`; identifiers must be non-empty and contain no control characters"
    )]
    InvalidAdapter { value: String },

    #[error("provider receipt schema version must be greater than zero")]
    ZeroReceiptSchemaVersion,

    #[error("provider turn-cursor schema version must be greater than zero")]
    ZeroCursorSchemaVersion,
}

fn validate_provider_adapter_key(
    value: impl Into<StorageString>,
) -> Result<StorageString, ProviderAdapterError> {
    let value = value.into();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(ProviderAdapterError::InvalidAdapter {
            value: value.to_string(),
        });
    }
    Ok(value)
}

/// Canonical framework-produced identity of the complete rendered epoch artifact.
///
/// The digest covers the format version, durable epoch id, complete contract
/// manifest, and canonical rendered System bytes. It never uses process-local
/// type ids, closure addresses, or [`super::HarnessEpochId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct EpochArtifactFingerprint(StorageString);

impl EpochArtifactFingerprint {
    fn from_canonical_bytes(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut value = String::with_capacity("sha256:".len() + digest.len() * 2);
        value.push_str("sha256:");
        for byte in digest {
            use fmt::Write as _;
            write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
        }
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EpochArtifactFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable descriptor for one event-driven runtime factory.
///
/// `implementation_version` is author-owned because Rust cannot inspect or
/// serialize a reducer closure. Reusing the version after behavior changes is
/// a host contract violation; changing the id, route, or version makes reopen
/// reject the existing artifact.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RuntimeBindingDescriptor {
    binding_id: StorageString,
    declaration_id: StorageString,
    route_namespace: StorageString,
    route_name: StorageString,
    implementation_version: StorageString,
}

impl RuntimeBindingDescriptor {
    pub(crate) fn new(
        declaration_id: impl Into<StorageString>,
        route_namespace: impl Into<StorageString>,
        route_name: impl Into<StorageString>,
        implementation_version: impl Into<StorageString>,
    ) -> Result<Self, EpochContractManifestError> {
        let declaration_id = declaration_id.into();
        Self::with_binding_id(
            declaration_id.clone(),
            declaration_id,
            route_namespace,
            route_name,
            implementation_version,
        )
    }

    pub(crate) fn with_binding_id(
        binding_id: impl Into<StorageString>,
        declaration_id: impl Into<StorageString>,
        route_namespace: impl Into<StorageString>,
        route_name: impl Into<StorageString>,
        implementation_version: impl Into<StorageString>,
    ) -> Result<Self, EpochContractManifestError> {
        Ok(Self {
            binding_id: validate_epoch_key(binding_id, "component binding id")?,
            declaration_id: validate_epoch_key(declaration_id, "runtime declaration id")?,
            route_namespace: validate_epoch_key(route_namespace, "runtime route namespace")?,
            route_name: validate_epoch_key(route_name, "runtime route name")?,
            implementation_version: validate_epoch_key(
                implementation_version,
                "runtime implementation version",
            )?,
        })
    }

    pub(crate) fn binding_id(&self) -> &str {
        &self.binding_id
    }

    pub(crate) fn declaration_id(&self) -> &str {
        &self.declaration_id
    }

    pub(crate) fn route_namespace(&self) -> &str {
        &self.route_namespace
    }

    pub(crate) fn route_name(&self) -> &str {
        &self.route_name
    }

    pub(crate) fn implementation_version(&self) -> &str {
        &self.implementation_version
    }
}

/// Stable provider-facing descriptor for one native tool declaration.
///
/// It contains schema data only. Dispatcher factories and runtime state stay
/// inside the mounted component owner.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderToolDescriptor {
    binding_id: StorageString,
    capability_id: StorageString,
    contract_version: StorageString,
    host_implementation_version: StorageString,
    name: StorageString,
    description: StorageString,
    input_schema: Value,
}

impl ProviderToolDescriptor {
    pub(crate) fn new(
        capability_id: impl Into<StorageString>,
        implementation_version: impl Into<StorageString>,
        spec: &ProviderToolSpec,
    ) -> Result<Self, EpochContractManifestError> {
        let capability_id = capability_id.into();
        let implementation_version = implementation_version.into();
        Self::with_binding_id(
            capability_id.clone(),
            capability_id,
            implementation_version.clone(),
            implementation_version,
            spec,
        )
    }

    pub(crate) fn with_binding_id(
        binding_id: impl Into<StorageString>,
        capability_id: impl Into<StorageString>,
        contract_version: impl Into<StorageString>,
        host_implementation_version: impl Into<StorageString>,
        spec: &ProviderToolSpec,
    ) -> Result<Self, EpochContractManifestError> {
        Ok(Self {
            binding_id: validate_epoch_key(binding_id, "component binding id")?,
            capability_id: validate_epoch_key(capability_id, "provider capability id")?,
            contract_version: validate_epoch_key(
                contract_version,
                "provider capability author contract version",
            )?,
            host_implementation_version: validate_epoch_key(
                host_implementation_version,
                "provider capability host implementation version",
            )?,
            name: spec.name().into(),
            description: spec.description().into(),
            input_schema: spec.input_schema().clone(),
        })
    }

    pub(crate) fn binding_id(&self) -> &str {
        &self.binding_id
    }

    pub(crate) fn capability_id(&self) -> &str {
        &self.capability_id
    }

    pub(crate) fn contract_version(&self) -> &str {
        &self.contract_version
    }

    pub(crate) fn host_implementation_version(&self) -> &str {
        &self.host_implementation_version
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn input_schema(&self) -> &Value {
        &self.input_schema
    }
}

/// Durable identity and ordered schemas for one provider capability group.
///
/// A group is backed by one fresh per-attempt dispatcher. Persisting the group
/// prevents rebind from recreating unrelated dispatchers by matching flattened
/// tool names alone.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProviderCapabilityDescriptor {
    binding_id: StorageString,
    declaration_id: StorageString,
    contract_version: StorageString,
    host_implementation_version: StorageString,
    tools: Vec<ProviderToolDescriptor>,
}

impl ProviderCapabilityDescriptor {
    fn new(
        binding_id: impl Into<StorageString>,
        declaration_id: impl Into<StorageString>,
        contract_version: impl Into<StorageString>,
        host_implementation_version: impl Into<StorageString>,
        tools: Vec<ProviderToolDescriptor>,
    ) -> Result<Self, EpochContractManifestError> {
        let descriptor = Self {
            binding_id: validate_epoch_key(binding_id, "component binding id")?,
            declaration_id: validate_epoch_key(
                declaration_id,
                "provider capability declaration id",
            )?,
            contract_version: validate_epoch_key(
                contract_version,
                "provider capability author contract version",
            )?,
            host_implementation_version: validate_epoch_key(
                host_implementation_version,
                "provider capability host implementation version",
            )?,
            tools,
        };
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub(crate) fn binding_id(&self) -> &str {
        &self.binding_id
    }

    pub(crate) fn declaration_id(&self) -> &str {
        &self.declaration_id
    }

    pub(crate) fn contract_version(&self) -> &str {
        &self.contract_version
    }

    pub(crate) fn host_implementation_version(&self) -> &str {
        &self.host_implementation_version
    }

    pub(crate) fn tools(&self) -> &[ProviderToolDescriptor] {
        &self.tools
    }

    fn validate(&self) -> Result<(), EpochContractManifestError> {
        if self.tools.is_empty() {
            return Err(EpochContractManifestError::EmptyProviderCapability {
                declaration_id: self.declaration_id.to_string(),
            });
        }
        for tool in &self.tools {
            if tool.binding_id() != self.binding_id
                || tool.capability_id() != self.declaration_id
                || tool.contract_version() != self.contract_version
                || tool.host_implementation_version() != self.host_implementation_version
            {
                return Err(EpochContractManifestError::ProviderCapabilityToolMismatch {
                    declaration_id: self.declaration_id.to_string(),
                    tool: tool.name().to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// Stable provider adapter and receipt-decoder contract for one epoch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderAdapterContract {
    adapter: StorageString,
    receipt_schema_version: u32,
}

impl ProviderAdapterContract {
    pub fn new(
        adapter: impl Into<StorageString>,
        receipt_schema_version: u32,
    ) -> Result<Self, ProviderAdapterError> {
        if receipt_schema_version == 0 {
            return Err(ProviderAdapterError::ZeroReceiptSchemaVersion);
        }
        Ok(Self {
            adapter: validate_provider_adapter_key(adapter)?,
            receipt_schema_version,
        })
    }

    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    pub fn receipt_schema_version(&self) -> u32 {
        self.receipt_schema_version
    }

    fn validate(&self) -> Result<(), EpochContractManifestError> {
        if self.receipt_schema_version == 0 {
            return Err(EpochContractManifestError::ZeroProviderReceiptSchemaVersion);
        }
        validate_epoch_key(self.adapter.clone(), "provider adapter contract")?;
        Ok(())
    }
}

/// Pre-render durable contract compared by the store before `SystemView` runs.
///
/// Applications should derive this from reusable component definitions rather
/// than duplicating schemas by hand. That authoring projection is the next API
/// slice; this value defines the persistence boundary it must satisfy.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EpochContractManifest {
    epoch_contract_id: EpochContractId,
    host_configuration_fingerprint: StorageString,
    runtime_binder_fingerprint: StorageString,
    /// Fixed harness behavior which must survive reopen and recovery.
    ///
    /// This is not part of the rendered System POM, but a different value can
    /// change whether an already-durable `TurnFlow::Continue` is allowed to
    /// execute. It therefore belongs to the artifact compatibility contract.
    turn_loop_max_turns: NonZeroU64,
    provider_adapter: ProviderAdapterContract,
    runtime_bindings: Vec<RuntimeBindingDescriptor>,
    provider_capabilities: Vec<ProviderCapabilityDescriptor>,
    provider_tools: Vec<ProviderToolDescriptor>,
}

impl EpochContractManifest {
    pub(crate) fn new(
        epoch_contract_id: EpochContractId,
        host_configuration_fingerprint: impl Into<StorageString>,
        runtime_binder_fingerprint: impl Into<StorageString>,
        turn_loop_max_turns: NonZeroUsize,
        provider_adapter: ProviderAdapterContract,
        runtime_bindings: Vec<RuntimeBindingDescriptor>,
        provider_tools: Vec<ProviderToolDescriptor>,
    ) -> Result<Self, EpochContractManifestError> {
        let provider_capabilities = group_provider_tools(&provider_tools)?;
        Self::with_provider_capabilities(
            epoch_contract_id,
            host_configuration_fingerprint,
            runtime_binder_fingerprint,
            turn_loop_max_turns,
            provider_adapter,
            runtime_bindings,
            provider_capabilities,
            provider_tools,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_provider_capabilities(
        epoch_contract_id: EpochContractId,
        host_configuration_fingerprint: impl Into<StorageString>,
        runtime_binder_fingerprint: impl Into<StorageString>,
        turn_loop_max_turns: NonZeroUsize,
        provider_adapter: ProviderAdapterContract,
        runtime_bindings: Vec<RuntimeBindingDescriptor>,
        provider_capabilities: Vec<ProviderCapabilityDescriptor>,
        provider_tools: Vec<ProviderToolDescriptor>,
    ) -> Result<Self, EpochContractManifestError> {
        let manifest = Self {
            epoch_contract_id,
            host_configuration_fingerprint: validate_epoch_key(
                host_configuration_fingerprint,
                "host configuration fingerprint",
            )?,
            runtime_binder_fingerprint: validate_epoch_key(
                runtime_binder_fingerprint,
                "runtime binder fingerprint",
            )?,
            turn_loop_max_turns: NonZeroU64::new(turn_loop_max_turns.get() as u64)
                .expect("a non-zero usize remains non-zero when stored as u64"),
            provider_adapter,
            runtime_bindings,
            provider_capabilities,
            provider_tools,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub(crate) fn from_runtime_plans<C, Props>(
        epoch_contract_id: EpochContractId,
        host_configuration_fingerprint: impl Into<StorageString>,
        runtime_binder_fingerprint: impl Into<StorageString>,
        turn_loop_max_turns: NonZeroUsize,
        provider_adapter: ProviderAdapterContract,
        binding_factories: &BindingFactoryPlan<C, Props>,
        provider_capabilities: &ProviderCapabilityPlan<C, Props>,
    ) -> Result<Self, DurableRuntimeContractError>
    where
        C: TurnChannels,
        Props: ?Sized + 'static,
    {
        let (runtime_bindings, provider_capabilities, provider_tools) =
            project_runtime_plans(binding_factories, provider_capabilities)?;
        Ok(Self::with_provider_capabilities(
            epoch_contract_id,
            host_configuration_fingerprint,
            runtime_binder_fingerprint,
            turn_loop_max_turns,
            provider_adapter,
            runtime_bindings,
            provider_capabilities,
            provider_tools,
        )?)
    }

    pub(crate) fn epoch_contract_id(&self) -> &EpochContractId {
        &self.epoch_contract_id
    }

    pub(crate) fn host_configuration_fingerprint(&self) -> &str {
        &self.host_configuration_fingerprint
    }

    pub(crate) fn runtime_binder_fingerprint(&self) -> &str {
        &self.runtime_binder_fingerprint
    }

    pub(crate) fn turn_loop_max_turns(&self) -> u64 {
        self.turn_loop_max_turns.get()
    }

    pub(crate) fn provider_adapter(&self) -> &ProviderAdapterContract {
        &self.provider_adapter
    }

    pub(crate) fn runtime_bindings(&self) -> &[RuntimeBindingDescriptor] {
        &self.runtime_bindings
    }

    pub(crate) fn provider_tools(&self) -> &[ProviderToolDescriptor] {
        &self.provider_tools
    }

    pub(crate) fn provider_capabilities(&self) -> &[ProviderCapabilityDescriptor] {
        &self.provider_capabilities
    }

    pub(crate) fn validate_runtime_plans<C, Props>(
        &self,
        binding_factories: &BindingFactoryPlan<C, Props>,
        provider_capabilities: &ProviderCapabilityPlan<C, Props>,
    ) -> Result<(), DurableRuntimeContractError>
    where
        C: TurnChannels,
        Props: ?Sized + 'static,
    {
        self.validate()?;
        let (runtime_bindings, provider_capabilities, provider_tools) =
            project_runtime_plans(binding_factories, provider_capabilities)?;
        if runtime_bindings != self.runtime_bindings {
            return Err(DurableRuntimeContractError::RuntimeBindingsMismatch);
        }
        if provider_tools != self.provider_tools {
            return Err(DurableRuntimeContractError::ProviderToolsMismatch);
        }
        if provider_capabilities != self.provider_capabilities {
            return Err(DurableRuntimeContractError::ProviderCapabilitiesMismatch);
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), EpochContractManifestError> {
        self.provider_adapter.validate()?;
        let mut declarations = std::collections::HashMap::new();
        let mut routes = std::collections::HashSet::new();
        for descriptor in &self.runtime_bindings {
            if declarations
                .insert(descriptor.declaration_id(), "binding")
                .is_some()
            {
                return Err(EpochContractManifestError::DuplicateRuntimeDeclaration {
                    declaration_id: descriptor.declaration_id().to_owned(),
                });
            }
            if !routes.insert((descriptor.route_namespace(), descriptor.route_name())) {
                return Err(EpochContractManifestError::DuplicateRuntimeRoute {
                    namespace: descriptor.route_namespace().to_owned(),
                    name: descriptor.route_name().to_owned(),
                });
            }
        }

        let mut tool_names = std::collections::HashSet::new();
        let mut projected_tools = Vec::new();
        for capability in &self.provider_capabilities {
            capability.validate()?;
            if declarations
                .insert(capability.declaration_id(), "provider")
                .is_some()
            {
                return Err(EpochContractManifestError::DuplicateRuntimeDeclaration {
                    declaration_id: capability.declaration_id().to_owned(),
                });
            }
            for descriptor in capability.tools() {
                if !tool_names.insert(descriptor.name()) {
                    return Err(EpochContractManifestError::DuplicateProviderTool {
                        name: descriptor.name().to_owned(),
                    });
                }
                projected_tools.push(descriptor.clone());
            }
        }
        if projected_tools != self.provider_tools {
            return Err(EpochContractManifestError::ProviderCapabilityProjectionMismatch);
        }
        Ok(())
    }
}

fn group_provider_tools(
    provider_tools: &[ProviderToolDescriptor],
) -> Result<Vec<ProviderCapabilityDescriptor>, EpochContractManifestError> {
    let mut groups = Vec::<ProviderCapabilityDescriptor>::new();
    let mut positions = std::collections::HashMap::<StorageString, usize>::new();
    for tool in provider_tools {
        if let Some(index) = positions.get(tool.capability_id()).copied() {
            let group = &mut groups[index];
            if group.contract_version() != tool.contract_version() {
                return Err(
                    EpochContractManifestError::ProviderCapabilityContractVersionMismatch {
                        declaration_id: tool.capability_id().to_owned(),
                    },
                );
            }
            if group.host_implementation_version() != tool.host_implementation_version() {
                return Err(
                    EpochContractManifestError::ProviderCapabilityHostVersionMismatch {
                        declaration_id: tool.capability_id().to_owned(),
                    },
                );
            }
            if group.binding_id() != tool.binding_id() {
                return Err(
                    EpochContractManifestError::ProviderCapabilityBindingMismatch {
                        declaration_id: tool.capability_id().to_owned(),
                    },
                );
            }
            group.tools.push(tool.clone());
        } else {
            let index = groups.len();
            positions.insert(tool.capability_id().into(), index);
            groups.push(ProviderCapabilityDescriptor::new(
                tool.binding_id(),
                tool.capability_id(),
                tool.contract_version(),
                tool.host_implementation_version(),
                vec![tool.clone()],
            )?);
        }
    }
    Ok(groups)
}

type RuntimePlanProjection = (
    Vec<RuntimeBindingDescriptor>,
    Vec<ProviderCapabilityDescriptor>,
    Vec<ProviderToolDescriptor>,
);

fn project_runtime_plans<C, Props>(
    binding_factories: &BindingFactoryPlan<C, Props>,
    provider_capabilities: &ProviderCapabilityPlan<C, Props>,
) -> Result<RuntimePlanProjection, DurableRuntimeContractError>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    let runtime_bindings = binding_factories
        .factories()
        .iter()
        .map(|factory| {
            let declaration_id = factory.declaration_id().ok_or_else(|| {
                DurableRuntimeContractError::MissingDeclarationId {
                    binding_id: factory.id().to_string(),
                }
            })?;
            let implementation_version = factory.implementation_version().ok_or_else(|| {
                DurableRuntimeContractError::MissingImplementationVersion {
                    declaration_id: declaration_id.to_owned(),
                }
            })?;
            Ok(RuntimeBindingDescriptor::with_binding_id(
                factory.id().to_string(),
                declaration_id,
                factory.route().namespace(),
                factory.route().name(),
                implementation_version,
            )?)
        })
        .collect::<Result<Vec<_>, DurableRuntimeContractError>>()?;

    let mut provider_capability_descriptors = Vec::new();
    let mut provider_tools = Vec::new();
    for capability in provider_capabilities.capabilities() {
        let capability_id = capability.declaration_id().ok_or_else(|| {
            DurableRuntimeContractError::MissingDeclarationId {
                binding_id: capability.id().to_string(),
            }
        })?;
        let contract_version = capability.contract_version().ok_or_else(|| {
            DurableRuntimeContractError::MissingProviderContractVersion {
                declaration_id: capability_id.to_owned(),
            }
        })?;
        let host_implementation_version =
            capability.host_implementation_version().ok_or_else(|| {
                DurableRuntimeContractError::MissingProviderHostImplementationVersion {
                    declaration_id: capability_id.to_owned(),
                }
            })?;
        let tools = capability
            .specs()
            .iter()
            .map(|spec| {
                ProviderToolDescriptor::with_binding_id(
                    capability.id().to_string(),
                    capability_id,
                    contract_version,
                    host_implementation_version,
                    spec,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        provider_tools.extend(tools.iter().cloned());
        provider_capability_descriptors.push(ProviderCapabilityDescriptor::new(
            capability.id().to_string(),
            capability_id,
            contract_version,
            host_implementation_version,
            tools,
        )?);
    }
    Ok((
        runtime_bindings,
        provider_capability_descriptors,
        provider_tools,
    ))
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DurableRuntimeContractError {
    #[error(transparent)]
    Manifest(#[from] EpochContractManifestError),

    #[error("compiled runtime binding `{binding_id}` has no durable declaration id")]
    MissingDeclarationId { binding_id: String },

    #[error("runtime declaration `{declaration_id}` has no durable implementation version")]
    MissingImplementationVersion { declaration_id: String },

    #[error("provider capability `{declaration_id}` has no durable author contract version")]
    MissingProviderContractVersion { declaration_id: String },

    #[error("provider capability `{declaration_id}` has no bound host implementation version")]
    MissingProviderHostImplementationVersion { declaration_id: String },

    #[error("compiled runtime binding declarations do not match the durable epoch manifest")]
    RuntimeBindingsMismatch,

    #[error("compiled provider tool declarations do not match the durable epoch manifest")]
    ProviderToolsMismatch,

    #[error("compiled provider capability groups do not match the durable epoch manifest")]
    ProviderCapabilitiesMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EpochContractManifestError {
    #[error(
        "invalid {kind} `{value}`; the value must be non-empty and contain no control characters"
    )]
    InvalidKey { kind: &'static str, value: String },

    #[error("duplicate runtime declaration `{declaration_id}` in durable epoch manifest")]
    DuplicateRuntimeDeclaration { declaration_id: String },

    #[error("duplicate runtime route `{namespace}:{name}` in durable epoch manifest")]
    DuplicateRuntimeRoute { namespace: String, name: String },

    #[error("duplicate provider tool `{name}` in durable epoch manifest")]
    DuplicateProviderTool { name: String },

    #[error("provider capability `{declaration_id}` must contain at least one tool")]
    EmptyProviderCapability { declaration_id: String },

    #[error("provider tool `{tool}` does not match its capability group `{declaration_id}`")]
    ProviderCapabilityToolMismatch {
        declaration_id: String,
        tool: String,
    },

    #[error("provider capability `{declaration_id}` uses conflicting implementation versions")]
    ProviderCapabilityContractVersionMismatch { declaration_id: String },

    #[error(
        "provider capability `{declaration_id}` has inconsistent host implementation versions"
    )]
    ProviderCapabilityHostVersionMismatch { declaration_id: String },

    #[error("provider capability `{declaration_id}` spans multiple component binding identities")]
    ProviderCapabilityBindingMismatch { declaration_id: String },

    #[error("provider capability groups do not flatten to the provider tool transport catalog")]
    ProviderCapabilityProjectionMismatch,

    #[error("provider receipt schema version must be greater than zero")]
    ZeroProviderReceiptSchemaVersion,

    #[error(
        "invalid epoch-open lease: expiry {expires_at_unix_ms} must be after issue time {issued_at_unix_ms}"
    )]
    InvalidEpochOpenLease {
        issued_at_unix_ms: u64,
        expires_at_unix_ms: u64,
    },
}

/// Persisted result of the one allowed System render, before provider attach.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RenderedEpochArtifact {
    format_version: u32,
    durable_epoch_id: DurableEpochId,
    manifest: EpochContractManifest,
    fingerprint: EpochArtifactFingerprint,
    /// The authored and resolved System POM are retained with the canonical
    /// text so a rehydrated mounted epoch can expose the same introspection
    /// surface without invoking `SystemView` again.
    system_document: Option<Document>,
    resolved_system_document: Option<ResolvedDocument>,
    rendered_system: StorageString,
}

impl RenderedEpochArtifact {
    pub(crate) fn new(
        durable_epoch_id: DurableEpochId,
        manifest: EpochContractManifest,
        rendered_system: impl Into<StorageString>,
    ) -> Result<Self, EpochArtifactBuildError> {
        Self::build(durable_epoch_id, manifest, None, None, rendered_system)
    }

    /// Build a complete mounted-System artifact. The two POM values are
    /// persisted alongside their rendered canonical bytes; reopening can
    /// therefore reconstruct the mounted epoch without executing System
    /// authoring a second time.
    pub(crate) fn with_system_documents(
        durable_epoch_id: DurableEpochId,
        manifest: EpochContractManifest,
        system_document: Document,
        resolved_system_document: ResolvedDocument,
        rendered_system: impl Into<StorageString>,
    ) -> Result<Self, EpochArtifactBuildError> {
        Self::build(
            durable_epoch_id,
            manifest,
            Some(system_document),
            Some(resolved_system_document),
            rendered_system,
        )
    }

    fn build(
        durable_epoch_id: DurableEpochId,
        manifest: EpochContractManifest,
        system_document: Option<Document>,
        resolved_system_document: Option<ResolvedDocument>,
        rendered_system: impl Into<StorageString>,
    ) -> Result<Self, EpochArtifactBuildError> {
        if system_document.is_some() != resolved_system_document.is_some() {
            return Err(EpochArtifactBuildError::IncompleteSystemDocuments);
        }
        let format_version = EPOCH_ARTIFACT_FORMAT_VERSION;
        let rendered_system = rendered_system.into();
        if let Some(resolved_system_document) = &resolved_system_document {
            let actual = render_pom_document(resolved_system_document)
                .map_err(EpochArtifactBuildError::RenderSystemDocument)?;
            if actual != rendered_system {
                return Err(EpochArtifactBuildError::RenderedSystemMismatch {
                    expected: rendered_system.to_string(),
                    actual,
                });
            }
        }
        let canonical = serde_json::to_vec(&(
            format_version,
            &durable_epoch_id,
            &manifest,
            &system_document,
            &resolved_system_document,
            &rendered_system,
        ))?;
        Ok(Self {
            format_version,
            durable_epoch_id,
            manifest,
            fingerprint: EpochArtifactFingerprint::from_canonical_bytes(&canonical),
            system_document,
            resolved_system_document,
            rendered_system,
        })
    }

    pub(crate) fn format_version(&self) -> u32 {
        self.format_version
    }

    pub(crate) fn durable_epoch_id(&self) -> &DurableEpochId {
        &self.durable_epoch_id
    }

    pub(crate) fn manifest(&self) -> &EpochContractManifest {
        &self.manifest
    }

    pub(crate) fn fingerprint(&self) -> &EpochArtifactFingerprint {
        &self.fingerprint
    }

    pub(crate) fn system_document(&self) -> Option<&Document> {
        self.system_document.as_ref()
    }

    pub(crate) fn resolved_system_document(&self) -> Option<&ResolvedDocument> {
        self.resolved_system_document.as_ref()
    }

    pub(crate) fn rendered_system(&self) -> &str {
        &self.rendered_system
    }

    pub(crate) fn validate(&self) -> Result<(), EpochArtifactValidationError> {
        if self.format_version != EPOCH_ARTIFACT_FORMAT_VERSION {
            return Err(EpochArtifactValidationError::UnsupportedFormatVersion {
                expected: EPOCH_ARTIFACT_FORMAT_VERSION,
                actual: self.format_version,
            });
        }
        self.manifest
            .validate()
            .map_err(EpochArtifactValidationError::Manifest)?;
        let canonical = serde_json::to_vec(&(
            self.format_version,
            &self.durable_epoch_id,
            &self.manifest,
            &self.system_document,
            &self.resolved_system_document,
            &self.rendered_system,
        ))
        .map_err(EpochArtifactValidationError::CanonicalEncoding)?;
        let actual = EpochArtifactFingerprint::from_canonical_bytes(&canonical);
        if actual != self.fingerprint {
            return Err(EpochArtifactValidationError::FingerprintMismatch {
                expected: self.fingerprint.clone(),
                actual,
            });
        }
        match (&self.system_document, &self.resolved_system_document) {
            (None, None) => {}
            (Some(_), Some(resolved_system_document)) => {
                let actual = render_pom_document(resolved_system_document)
                    .map_err(EpochArtifactValidationError::RenderSystemDocument)?;
                if actual != self.rendered_system {
                    return Err(EpochArtifactValidationError::RenderedSystemMismatch {
                        expected: self.rendered_system.to_string(),
                        actual,
                    });
                }
            }
            _ => return Err(EpochArtifactValidationError::IncompleteSystemDocuments),
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EpochArtifactBuildError {
    #[error("failed to canonically encode rendered epoch artifact: {0}")]
    CanonicalEncoding(#[from] serde_json::Error),

    #[error("rendered epoch artifact must retain both System POM documents or neither")]
    IncompleteSystemDocuments,

    #[error("failed to render persisted resolved System document: {0}")]
    RenderSystemDocument(#[source] PomRenderError),

    #[error("rendered System bytes differ from the persisted resolved System document")]
    RenderedSystemMismatch { expected: String, actual: String },
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EpochArtifactValidationError {
    #[error("unsupported epoch artifact format version {actual}; expected {expected}")]
    UnsupportedFormatVersion { expected: u32, actual: u32 },

    #[error(transparent)]
    Manifest(#[from] EpochContractManifestError),

    #[error("failed to canonically encode persisted epoch artifact: {0}")]
    CanonicalEncoding(#[source] serde_json::Error),

    #[error("persisted epoch artifact retains only one of its System POM documents")]
    IncompleteSystemDocuments,

    #[error("failed to render persisted resolved System document: {0}")]
    RenderSystemDocument(#[source] PomRenderError),

    #[error("persisted rendered System bytes differ from its resolved System document")]
    RenderedSystemMismatch { expected: String, actual: String },

    #[error("epoch artifact fingerprint mismatch: expected `{expected}`, computed `{actual}`")]
    FingerprintMismatch {
        expected: EpochArtifactFingerprint,
        actual: EpochArtifactFingerprint,
    },
}

/// Opaque, serializable receipt returned by a provider's idempotent attach.
///
/// The mounted runtime validates the adapter contract, durable epoch id, and
/// artifact fingerprint before persisting this value. Reopen returns it to the
/// provider without exposing System bytes or tool schemas.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderEpochReceipt {
    adapter: StorageString,
    schema_version: u32,
    durable_epoch_id: DurableEpochId,
    artifact_fingerprint: EpochArtifactFingerprint,
    value: Value,
}

/// Opaque, adapter-owned continuation state after one accepted provider turn.
///
/// Unlike [`ProviderEpochReceipt`], this value changes after successful turns.
/// The mounted store persists it atomically with the matching session mutation,
/// call checkpoint, and outbox. Binding it to the durable artifact prevents a
/// cursor from another System epoch from being used during reopen.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderTurnCursor {
    adapter: StorageString,
    schema_version: u32,
    durable_epoch_id: DurableEpochId,
    artifact_fingerprint: EpochArtifactFingerprint,
    value: Value,
}

impl ProviderTurnCursor {
    pub fn new(
        adapter: impl Into<StorageString>,
        schema_version: u32,
        durable_epoch_id: DurableEpochId,
        artifact_fingerprint: EpochArtifactFingerprint,
        value: Value,
    ) -> Result<Self, ProviderAdapterError> {
        if schema_version == 0 {
            return Err(ProviderAdapterError::ZeroCursorSchemaVersion);
        }
        Ok(Self {
            adapter: validate_provider_adapter_key(adapter)?,
            schema_version,
            durable_epoch_id,
            artifact_fingerprint,
            value,
        })
    }

    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn durable_epoch_id(&self) -> &DurableEpochId {
        &self.durable_epoch_id
    }

    pub fn artifact_fingerprint(&self) -> &EpochArtifactFingerprint {
        &self.artifact_fingerprint
    }

    pub fn value(&self) -> &Value {
        &self.value
    }
}

impl ProviderEpochReceipt {
    pub fn new(
        adapter: impl Into<StorageString>,
        schema_version: u32,
        durable_epoch_id: DurableEpochId,
        artifact_fingerprint: EpochArtifactFingerprint,
        value: Value,
    ) -> Result<Self, ProviderAdapterError> {
        if schema_version == 0 {
            return Err(ProviderAdapterError::ZeroReceiptSchemaVersion);
        }
        Ok(Self {
            adapter: validate_provider_adapter_key(adapter)?,
            schema_version,
            durable_epoch_id,
            artifact_fingerprint,
            value,
        })
    }

    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn durable_epoch_id(&self) -> &DurableEpochId {
        &self.durable_epoch_id
    }

    pub fn artifact_fingerprint(&self) -> &EpochArtifactFingerprint {
        &self.artifact_fingerprint
    }
}

/// Complete durable artifact used by every subsequent reopen.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ActiveEpochArtifact {
    rendered: RenderedEpochArtifact,
    provider_receipt: ProviderEpochReceipt,
}

impl ActiveEpochArtifact {
    pub(crate) fn new(
        rendered: RenderedEpochArtifact,
        provider_receipt: ProviderEpochReceipt,
    ) -> Result<Self, EpochArtifactActivationError> {
        let artifact = Self {
            rendered,
            provider_receipt,
        };
        artifact.validate()?;
        Ok(artifact)
    }

    pub(crate) fn rendered(&self) -> &RenderedEpochArtifact {
        &self.rendered
    }

    pub(crate) fn provider_receipt(&self) -> &ProviderEpochReceipt {
        &self.provider_receipt
    }

    pub(crate) fn validate(&self) -> Result<(), EpochArtifactActivationError> {
        self.rendered.validate()?;
        let expected = self.rendered.manifest().provider_adapter();
        if self.provider_receipt.adapter() != expected.adapter()
            || self.provider_receipt.schema_version() != expected.receipt_schema_version()
        {
            return Err(EpochArtifactActivationError::ProviderReceiptContract {
                expected_adapter: expected.adapter().to_owned(),
                expected_schema_version: expected.receipt_schema_version(),
                actual_adapter: self.provider_receipt.adapter().to_owned(),
                actual_schema_version: self.provider_receipt.schema_version(),
            });
        }
        if self.provider_receipt.durable_epoch_id() != self.rendered.durable_epoch_id()
            || self.provider_receipt.artifact_fingerprint() != self.rendered.fingerprint()
        {
            return Err(EpochArtifactActivationError::ProviderReceiptIdentity);
        }
        Ok(())
    }

    pub(crate) fn validate_provider_cursor(
        &self,
        cursor: &ProviderTurnCursor,
    ) -> Result<(), EpochArtifactActivationError> {
        if cursor.adapter() != self.provider_receipt.adapter()
            || cursor.schema_version() != self.provider_receipt.schema_version()
        {
            return Err(EpochArtifactActivationError::ProviderCursorContract {
                expected_adapter: self.provider_receipt.adapter().to_owned(),
                expected_schema_version: self.provider_receipt.schema_version(),
                actual_adapter: cursor.adapter().to_owned(),
                actual_schema_version: cursor.schema_version(),
            });
        }
        if cursor.durable_epoch_id() != self.rendered.durable_epoch_id()
            || cursor.artifact_fingerprint() != self.rendered.fingerprint()
        {
            return Err(EpochArtifactActivationError::ProviderCursorIdentity);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EpochArtifactActivationError {
    #[error(transparent)]
    Artifact(#[from] EpochArtifactValidationError),

    #[error(
        "provider receipt contract `{actual_adapter}`/v{actual_schema_version} does not match epoch manifest `{expected_adapter}`/v{expected_schema_version}"
    )]
    ProviderReceiptContract {
        expected_adapter: String,
        expected_schema_version: u32,
        actual_adapter: String,
        actual_schema_version: u32,
    },

    #[error("provider receipt is bound to a different durable epoch artifact")]
    ProviderReceiptIdentity,

    #[error(
        "provider cursor contract `{actual_adapter}`/v{actual_schema_version} does not match epoch receipt `{expected_adapter}`/v{expected_schema_version}"
    )]
    ProviderCursorContract {
        expected_adapter: String,
        expected_schema_version: u32,
        actual_adapter: String,
        actual_schema_version: u32,
    },

    #[error("provider cursor is bound to a different durable epoch artifact")]
    ProviderCursorIdentity,
}

/// Store-issued lease window for one durable epoch transition.
///
/// The store is the authority for wall-clock interpretation. AgentView only
/// carries the deadline through admissions and fence-checked writes so hosts
/// can diagnose an in-flight owner and reject a stale owner after expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EpochOpenLease {
    issued_at_unix_ms: u64,
    expires_at_unix_ms: u64,
}

impl EpochOpenLease {
    pub(crate) fn new(
        issued_at_unix_ms: u64,
        expires_at_unix_ms: u64,
    ) -> Result<Self, EpochContractManifestError> {
        if expires_at_unix_ms <= issued_at_unix_ms {
            return Err(EpochContractManifestError::InvalidEpochOpenLease {
                issued_at_unix_ms,
                expires_at_unix_ms,
            });
        }
        Ok(Self {
            issued_at_unix_ms,
            expires_at_unix_ms,
        })
    }

    pub(crate) fn issued_at_unix_ms(self) -> u64 {
        self.issued_at_unix_ms
    }

    pub(crate) fn expires_at_unix_ms(self) -> u64 {
        self.expires_at_unix_ms
    }

    pub(crate) fn is_live_at(self, unix_ms: u64) -> bool {
        unix_ms < self.expires_at_unix_ms
    }
}

/// Store-issued ownership token for one durable epoch transition.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EpochOpenFence {
    durable_epoch_id: DurableEpochId,
    token: StorageString,
    lease: EpochOpenLease,
}

impl EpochOpenFence {
    pub(crate) fn new(
        durable_epoch_id: DurableEpochId,
        token: impl Into<StorageString>,
        lease: EpochOpenLease,
    ) -> Result<Self, EpochContractManifestError> {
        Ok(Self {
            durable_epoch_id,
            token: validate_epoch_key(token, "epoch open fence")?,
            lease,
        })
    }

    pub(crate) fn durable_epoch_id(&self) -> &DurableEpochId {
        &self.durable_epoch_id
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn lease(&self) -> EpochOpenLease {
        self.lease
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum EpochOpenRecoveryPhase {
    RenderStarted,
    ProviderAttachmentIndeterminate,
}

/// Atomic store decision made before the caller can invoke `SystemView`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EpochOpenAdmission {
    /// The only owner allowed to invoke `SystemView` for this durable epoch.
    Create { fence: EpochOpenFence },
    /// Resume idempotent provider attachment from an already-rendered artifact.
    ResumeAttachment {
        fence: EpochOpenFence,
        artifact: RenderedEpochArtifact,
    },
    /// Rebind local factories and rehydrate the provider; never render System.
    Existing { artifact: ActiveEpochArtifact },
    /// Another live owner holds the epoch transition fence.
    InFlight {
        durable_epoch_id: DurableEpochId,
        lease_expires_at_unix_ms: u64,
    },
    /// Strict at-most-once semantics forbid automatically repeating this phase.
    RecoveryRequired {
        durable_epoch_id: DurableEpochId,
        phase: EpochOpenRecoveryPhase,
    },
    /// The session already points at a different durable contract manifest.
    Conflict {
        durable_epoch_id: DurableEpochId,
        existing: EpochContractManifest,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct EpochOpenRequest<'a> {
    session_id: &'a DurableSessionId,
    manifest: &'a EpochContractManifest,
}

impl<'a> EpochOpenRequest<'a> {
    pub(crate) fn new(
        session_id: &'a DurableSessionId,
        manifest: &'a EpochContractManifest,
    ) -> Self {
        Self {
            session_id,
            manifest,
        }
    }

    pub(crate) fn session_id(&self) -> &'a DurableSessionId {
        self.session_id
    }

    pub(crate) fn manifest(&self) -> &'a EpochContractManifest {
        self.manifest
    }
}

/// Persistence protocol for the store-owned durable epoch fence.
///
/// `acquire_epoch` durably records `RenderStarted` before it returns `Create`.
/// A crash in that phase therefore returns `RecoveryRequired`, not another
/// `Create`. Once `store_rendered_epoch` succeeds, a replacement owner may
/// receive `ResumeAttachment` and repeat provider attachment with the same
/// `DurableEpochId`; the provider must treat that key idempotently. If an
/// attachment operation returns an error after the rendered artifact is
/// durable, the original owner may explicitly relinquish only its attachment
/// fence. This preserves the artifact and lets the next owner resume the
/// idempotent attachment immediately; it never authorizes a second System
/// render.
#[async_trait::async_trait]
pub(crate) trait DurableEpochStore: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    async fn acquire_epoch(
        &self,
        request: EpochOpenRequest<'_>,
    ) -> Result<EpochOpenAdmission, Self::Error>;

    async fn store_rendered_epoch(
        &self,
        fence: &EpochOpenFence,
        artifact: &RenderedEpochArtifact,
    ) -> Result<(), Self::Error>;

    /// Release a completed-but-unsuccessful provider attachment attempt.
    ///
    /// The caller has observed the provider future return an error, so it no
    /// longer owns transport work under `fence`. The store must compare both
    /// the fence and immutable rendered artifact, retain that artifact, and
    /// make only `ResumeAttachment` eligible on the next acquire.
    async fn relinquish_epoch_attachment(
        &self,
        fence: &EpochOpenFence,
        artifact: &RenderedEpochArtifact,
    ) -> Result<(), Self::Error>;

    async fn activate_epoch(
        &self,
        fence: &EpochOpenFence,
        artifact: &ActiveEpochArtifact,
    ) -> Result<(), Self::Error>;
}

/// Narrow POM-free input for process-local runtime reconstruction.
///
/// This request intentionally omits canonical System bytes and provider
/// attachment data. A runtime binder can inspect only the durable identity,
/// verified declaration manifest, and artifact fingerprint.
pub(crate) struct RuntimeRebindRequest<'a> {
    durable_epoch_id: &'a DurableEpochId,
    manifest: &'a EpochContractManifest,
    fingerprint: &'a EpochArtifactFingerprint,
}

impl<'a> RuntimeRebindRequest<'a> {
    pub(crate) fn new(artifact: &'a RenderedEpochArtifact) -> Self {
        Self {
            durable_epoch_id: artifact.durable_epoch_id(),
            manifest: artifact.manifest(),
            fingerprint: artifact.fingerprint(),
        }
    }

    pub(crate) fn durable_epoch_id(&self) -> &'a DurableEpochId {
        self.durable_epoch_id
    }

    pub(crate) fn manifest(&self) -> &'a EpochContractManifest {
        self.manifest
    }

    pub(crate) fn fingerprint(&self) -> &'a EpochArtifactFingerprint {
        self.fingerprint
    }
}

/// Mount-owned source for one complete ordered durable System definition.
///
/// Create consumes its POM projection and derives the durable manifest from
/// the compiled runtime declarations. Reopen filters the same retained tree to
/// a POM-free runtime projection and receives only durable identity,
/// descriptors, and an artifact fingerprint.
pub(crate) trait RuntimeBinder<C, Props: ?Sized + 'static = ()>:
    Send + Sync + 'static
where
    C: TurnChannels,
{
    /// Return the one retained source used for both first System compilation
    /// and POM-free runtime projection.
    fn durable_system(&self) -> Arc<DurableSystem<C, Props>>;
}

/// Sealed process-local projection used by durable recovery.
///
/// Its only input is [`RuntimeRebindRequest`]. In particular, a projection has
/// no `MountProps`, POM document, System renderer, store, or provider source
/// access.
pub(crate) trait RuntimeRebindProjection: Send + Sync + 'static {
    type Runtime;
    type Error: Error + Send + Sync + 'static;

    fn rebind(&self, request: RuntimeRebindRequest<'_>) -> Result<Self::Runtime, Self::Error>;
}

/// Process-local declarations that can prove they match the durable manifest
/// before the coordinator performs provider I/O.
pub(crate) trait DurableEpochRuntime {
    type Error: Error + Send + Sync + 'static;

    fn validate_against_manifest(
        &self,
        manifest: &EpochContractManifest,
    ) -> Result<(), Self::Error>;
}

/// First provider attachment. This is the only provider request that exposes
/// canonical System bytes and normalized tool schemas.
///
/// The mounted runtime constructs this after durable Create or a resumable
/// attachment admission. Ordinary turns cannot construct or carry it.
pub struct ProviderEpochAttachRequest<'a> {
    artifact: &'a RenderedEpochArtifact,
}

impl<'a> ProviderEpochAttachRequest<'a> {
    pub(crate) fn new(artifact: &'a RenderedEpochArtifact) -> Self {
        Self { artifact }
    }

    pub fn durable_epoch_id(&self) -> &'a DurableEpochId {
        self.artifact.durable_epoch_id()
    }

    pub fn fingerprint(&self) -> &'a EpochArtifactFingerprint {
        self.artifact.fingerprint()
    }

    pub fn system(&self) -> &'a str {
        self.artifact.rendered_system()
    }

    pub fn tools(&self) -> &'a [ProviderToolDescriptor] {
        self.artifact.manifest().provider_tools()
    }
}

/// Existing provider attachment. Its type deliberately has no System or tool
/// accessor, so reopen cannot accidentally resend either payload.
pub struct ProviderEpochRehydrateRequest<'a> {
    durable_epoch_id: &'a DurableEpochId,
    fingerprint: &'a EpochArtifactFingerprint,
    receipt: &'a ProviderEpochReceipt,
    cursor: Option<&'a ProviderTurnCursor>,
}

impl<'a> ProviderEpochRehydrateRequest<'a> {
    pub(crate) fn new(
        artifact: &'a ActiveEpochArtifact,
        cursor: Option<&'a ProviderTurnCursor>,
    ) -> Result<Self, EpochArtifactActivationError> {
        if let Some(cursor) = cursor {
            artifact.validate_provider_cursor(cursor)?;
        }
        Ok(Self {
            durable_epoch_id: artifact.rendered().durable_epoch_id(),
            fingerprint: artifact.rendered().fingerprint(),
            receipt: artifact.provider_receipt(),
            cursor,
        })
    }

    pub fn durable_epoch_id(&self) -> &'a DurableEpochId {
        self.durable_epoch_id
    }

    pub fn fingerprint(&self) -> &'a EpochArtifactFingerprint {
        self.fingerprint
    }

    pub fn receipt(&self) -> &'a ProviderEpochReceipt {
        self.receipt
    }

    /// Last cursor committed atomically with the durable mounted session.
    pub fn cursor(&self) -> Option<&'a ProviderTurnCursor> {
        self.cursor
    }
}

/// Provider binding and receipt returned by one durable epoch attachment.
///
/// The binding is process-local. The receipt is persisted by the owner and
/// becomes the only provider-specific input available on reopen.
pub struct AttachedProviderEpoch<Binding> {
    binding: Binding,
    receipt: ProviderEpochReceipt,
}

impl<Binding> AttachedProviderEpoch<Binding> {
    pub fn new(binding: Binding, receipt: ProviderEpochReceipt) -> Self {
        Self { binding, receipt }
    }

    pub(crate) fn into_parts(self) -> (Binding, ProviderEpochReceipt) {
        (self.binding, self.receipt)
    }
}

/// Provider-side durable epoch contract used by the future mounted owner.
#[async_trait::async_trait]
pub(crate) trait DurableProviderEpoch: Send + Sync + 'static {
    type Binding: Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;

    async fn attach_epoch(
        &self,
        request: ProviderEpochAttachRequest<'_>,
    ) -> Result<AttachedProviderEpoch<Self::Binding>, Self::Error>;

    async fn rehydrate_epoch(
        &self,
        request: ProviderEpochRehydrateRequest<'_>,
    ) -> Result<Self::Binding, Self::Error>;
}

/// Runtime and canonical System output produced by the one permitted render.
pub(crate) struct FirstEpochMount<Runtime> {
    compiled_manifest: EpochContractManifest,
    system_document: Option<Document>,
    resolved_system_document: Option<ResolvedDocument>,
    rendered_system: StorageString,
    runtime: Runtime,
}

impl<Runtime> FirstEpochMount<Runtime> {
    pub(crate) fn new(
        compiled_manifest: EpochContractManifest,
        rendered_system: impl Into<StorageString>,
        runtime: Runtime,
    ) -> Self {
        Self {
            compiled_manifest,
            system_document: None,
            resolved_system_document: None,
            rendered_system: rendered_system.into(),
            runtime,
        }
    }

    pub(crate) fn with_system_documents(
        mut self,
        system_document: Document,
        resolved_system_document: ResolvedDocument,
    ) -> Self {
        self.system_document = Some(system_document);
        self.resolved_system_document = Some(resolved_system_document);
        self
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        EpochContractManifest,
        Option<Document>,
        Option<ResolvedDocument>,
        StorageString,
        Runtime,
    ) {
        (
            self.compiled_manifest,
            self.system_document,
            self.resolved_system_document,
            self.rendered_system,
            self.runtime,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DurableEpochOpenKind {
    Created,
    AttachmentResumed,
    Rehydrated,
}

/// Complete process-local result of opening one durable epoch.
pub(crate) struct OpenedDurableEpoch<Runtime, ProviderBinding> {
    kind: DurableEpochOpenKind,
    artifact: ActiveEpochArtifact,
    runtime: Runtime,
    provider_binding: ProviderBinding,
}

impl<Runtime, ProviderBinding> OpenedDurableEpoch<Runtime, ProviderBinding> {
    pub(crate) fn kind(&self) -> DurableEpochOpenKind {
        self.kind
    }

    pub(crate) fn artifact(&self) -> &ActiveEpochArtifact {
        &self.artifact
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        DurableEpochOpenKind,
        ActiveEpochArtifact,
        Runtime,
        ProviderBinding,
    ) {
        (
            self.kind,
            self.artifact,
            self.runtime,
            self.provider_binding,
        )
    }
}

type BoxEpochOpenError = Box<dyn Error + Send + Sync + 'static>;

#[derive(Debug, thiserror::Error)]
#[error(
    "provider attachment failed: {attachment}; attachment fence relinquish also failed: {relinquish}"
)]
struct ProviderAttachmentRelinquishError {
    attachment: BoxEpochOpenError,
    relinquish: BoxEpochOpenError,
}

/// A provider continuation belongs to an already activated epoch artifact.
///
/// A Create/attachment-resume admission has no active artifact against which
/// a cursor can be validated, so accepting one would let stale provider state
/// survive into a newly attached System epoch.
#[derive(Debug, thiserror::Error)]
#[error("durable epoch `{durable_epoch_id}` cannot accept a provider cursor before activation")]
struct ProviderCursorBeforeActivation {
    durable_epoch_id: DurableEpochId,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DurableEpochCoordinatorError {
    #[error("durable epoch {phase} failed: {source}")]
    Phase {
        phase: &'static str,
        #[source]
        source: BoxEpochOpenError,
    },

    #[error("{phase} manifest does not match the store-acquired durable epoch contract")]
    ManifestMismatch { phase: &'static str },

    #[error(
        "durable epoch `{durable_epoch_id}` is currently being opened by another owner until lease deadline {lease_expires_at_unix_ms}"
    )]
    InFlight {
        durable_epoch_id: DurableEpochId,
        lease_expires_at_unix_ms: u64,
    },

    #[error("durable epoch `{durable_epoch_id}` requires explicit recovery after {phase:?}")]
    RecoveryRequired {
        durable_epoch_id: DurableEpochId,
        phase: EpochOpenRecoveryPhase,
    },

    #[error("durable epoch `{durable_epoch_id}` already uses a different contract manifest")]
    Conflict { durable_epoch_id: DurableEpochId },
}

impl DurableEpochCoordinatorError {
    fn phase(phase: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
        Self::Phase {
            phase,
            source: Box::new(source),
        }
    }

    fn ensure_manifest(
        phase: &'static str,
        expected: &EpochContractManifest,
        actual: &EpochContractManifest,
    ) -> Result<(), Self> {
        if expected != actual {
            return Err(Self::ManifestMismatch { phase });
        }
        Ok(())
    }
}

async fn attach_rendered_epoch<Store, Provider>(
    store: &Store,
    provider: &Provider,
    fence: &EpochOpenFence,
    artifact: &RenderedEpochArtifact,
    phase: &'static str,
) -> Result<AttachedProviderEpoch<Provider::Binding>, DurableEpochCoordinatorError>
where
    Store: DurableEpochStore,
    Provider: DurableProviderEpoch,
{
    match provider
        .attach_epoch(ProviderEpochAttachRequest::new(artifact))
        .await
    {
        Ok(attachment) => Ok(attachment),
        Err(attachment) => match store.relinquish_epoch_attachment(fence, artifact).await {
            Ok(()) => Err(DurableEpochCoordinatorError::phase(phase, attachment)),
            Err(relinquish) => Err(DurableEpochCoordinatorError::phase(
                "provider attachment fence relinquish",
                ProviderAttachmentRelinquishError {
                    attachment: Box::new(attachment),
                    relinquish: Box::new(relinquish),
                },
            )),
        },
    }
}

/// Open one durable epoch without allowing reopen paths to invoke SystemView.
///
/// The `render_system` closure is consumed only for the store's sole `Create`
/// admission. `ResumeAttachment` and `Existing` instead call the POM-free
/// runtime binder. A failed provider attach leaves the already-rendered
/// artifact in the store so a later owner retries with the same
/// `DurableEpochId`. `provider_cursor` is valid only for `Existing`: it must
/// have been committed with that active artifact's session snapshot and is
/// validated before provider rehydration.
pub(crate) async fn open_durable_epoch<Store, Projection, Provider, RenderSystem, RenderError>(
    store: &Store,
    projection: &Projection,
    provider: &Provider,
    session_id: &DurableSessionId,
    manifest: &EpochContractManifest,
    provider_cursor: Option<&ProviderTurnCursor>,
    render_system: RenderSystem,
) -> Result<OpenedDurableEpoch<Projection::Runtime, Provider::Binding>, DurableEpochCoordinatorError>
where
    Store: DurableEpochStore,
    Projection: RuntimeRebindProjection,
    Projection::Runtime: DurableEpochRuntime,
    Provider: DurableProviderEpoch,
    RenderSystem:
        FnOnce(&DurableEpochId) -> Result<FirstEpochMount<Projection::Runtime>, RenderError>,
    RenderError: Error + Send + Sync + 'static,
{
    let admission = store
        .acquire_epoch(EpochOpenRequest::new(session_id, manifest))
        .await
        .map_err(|source| DurableEpochCoordinatorError::phase("acquire", source))?;

    match admission {
        EpochOpenAdmission::Create { fence } => {
            if provider_cursor.is_some() {
                return Err(DurableEpochCoordinatorError::phase(
                    "provider cursor validation",
                    ProviderCursorBeforeActivation {
                        durable_epoch_id: fence.durable_epoch_id().clone(),
                    },
                ));
            }
            let mounted = render_system(fence.durable_epoch_id())
                .map_err(|source| DurableEpochCoordinatorError::phase("System render", source))?;
            let FirstEpochMount {
                compiled_manifest,
                system_document,
                resolved_system_document,
                rendered_system,
                runtime,
            } = mounted;
            DurableEpochCoordinatorError::ensure_manifest(
                "compiled System",
                manifest,
                &compiled_manifest,
            )?;
            runtime
                .validate_against_manifest(&compiled_manifest)
                .map_err(|source| {
                    DurableEpochCoordinatorError::phase("compiled runtime contract", source)
                })?;
            let rendered = match (system_document, resolved_system_document) {
                (None, None) => RenderedEpochArtifact::new(
                    fence.durable_epoch_id().clone(),
                    compiled_manifest,
                    rendered_system,
                ),
                (Some(system_document), Some(resolved_system_document)) => {
                    RenderedEpochArtifact::with_system_documents(
                        fence.durable_epoch_id().clone(),
                        compiled_manifest,
                        system_document,
                        resolved_system_document,
                        rendered_system,
                    )
                }
                _ => Err(EpochArtifactBuildError::IncompleteSystemDocuments),
            }
            .map_err(|source| DurableEpochCoordinatorError::phase("artifact build", source))?;
            store
                .store_rendered_epoch(&fence, &rendered)
                .await
                .map_err(|source| {
                    DurableEpochCoordinatorError::phase("artifact persistence", source)
                })?;
            let (provider_binding, receipt) =
                attach_rendered_epoch(store, provider, &fence, &rendered, "provider attachment")
                    .await?
                    .into_parts();
            let active = ActiveEpochArtifact::new(rendered, receipt).map_err(|source| {
                DurableEpochCoordinatorError::phase("provider receipt validation", source)
            })?;
            store
                .activate_epoch(&fence, &active)
                .await
                .map_err(|source| {
                    DurableEpochCoordinatorError::phase("epoch activation", source)
                })?;
            Ok(OpenedDurableEpoch {
                kind: DurableEpochOpenKind::Created,
                artifact: active,
                runtime,
                provider_binding,
            })
        }
        EpochOpenAdmission::ResumeAttachment { fence, artifact } => {
            if provider_cursor.is_some() {
                return Err(DurableEpochCoordinatorError::phase(
                    "provider cursor validation",
                    ProviderCursorBeforeActivation {
                        durable_epoch_id: fence.durable_epoch_id().clone(),
                    },
                ));
            }
            artifact.validate().map_err(|source| {
                DurableEpochCoordinatorError::phase("artifact validation", source)
            })?;
            DurableEpochCoordinatorError::ensure_manifest(
                "persisted artifact",
                manifest,
                artifact.manifest(),
            )?;
            let runtime = projection
                .rebind(RuntimeRebindRequest::new(&artifact))
                .map_err(|source| DurableEpochCoordinatorError::phase("runtime rebind", source))?;
            runtime
                .validate_against_manifest(artifact.manifest())
                .map_err(|source| {
                    DurableEpochCoordinatorError::phase("rebound runtime contract", source)
                })?;
            let (provider_binding, receipt) = attach_rendered_epoch(
                store,
                provider,
                &fence,
                &artifact,
                "provider attachment resume",
            )
            .await?
            .into_parts();
            let active = ActiveEpochArtifact::new(artifact, receipt).map_err(|source| {
                DurableEpochCoordinatorError::phase("provider receipt validation", source)
            })?;
            store
                .activate_epoch(&fence, &active)
                .await
                .map_err(|source| {
                    DurableEpochCoordinatorError::phase("epoch activation", source)
                })?;
            Ok(OpenedDurableEpoch {
                kind: DurableEpochOpenKind::AttachmentResumed,
                artifact: active,
                runtime,
                provider_binding,
            })
        }
        EpochOpenAdmission::Existing { artifact } => {
            artifact.validate().map_err(|source| {
                DurableEpochCoordinatorError::phase("active artifact validation", source)
            })?;
            DurableEpochCoordinatorError::ensure_manifest(
                "persisted artifact",
                manifest,
                artifact.rendered().manifest(),
            )?;
            let runtime = projection
                .rebind(RuntimeRebindRequest::new(artifact.rendered()))
                .map_err(|source| DurableEpochCoordinatorError::phase("runtime rebind", source))?;
            runtime
                .validate_against_manifest(artifact.rendered().manifest())
                .map_err(|source| {
                    DurableEpochCoordinatorError::phase("rebound runtime contract", source)
                })?;
            let rehydrate = ProviderEpochRehydrateRequest::new(&artifact, provider_cursor)
                .map_err(|source| {
                    DurableEpochCoordinatorError::phase("provider cursor validation", source)
                })?;
            let provider_binding = provider
                .rehydrate_epoch(rehydrate)
                .await
                .map_err(|source| {
                    DurableEpochCoordinatorError::phase("provider rehydration", source)
                })?;
            Ok(OpenedDurableEpoch {
                kind: DurableEpochOpenKind::Rehydrated,
                artifact,
                runtime,
                provider_binding,
            })
        }
        EpochOpenAdmission::InFlight {
            durable_epoch_id,
            lease_expires_at_unix_ms,
        } => Err(DurableEpochCoordinatorError::InFlight {
            durable_epoch_id,
            lease_expires_at_unix_ms,
        }),
        EpochOpenAdmission::RecoveryRequired {
            durable_epoch_id,
            phase,
        } => Err(DurableEpochCoordinatorError::RecoveryRequired {
            durable_epoch_id,
            phase,
        }),
        EpochOpenAdmission::Conflict {
            durable_epoch_id, ..
        } => Err(DurableEpochCoordinatorError::Conflict { durable_epoch_id }),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        convert::Infallible,
        sync::{Arc, Mutex},
    };

    use serde_json::json;

    use super::*;

    fn runtime_descriptor(version: &str) -> RuntimeBindingDescriptor {
        RuntimeBindingDescriptor::new("root/select_intent::xml", "xml", "select_intent", version)
            .unwrap()
    }

    fn tool_descriptor(description: &str) -> ProviderToolDescriptor {
        let spec = ProviderToolSpec::new(
            "inspect",
            description,
            json!({
                "type": "object",
                "properties": { "subject": { "type": "string" } },
                "required": ["subject"]
            }),
        )
        .unwrap();
        ProviderToolDescriptor::new("root/director_tools", "v1", &spec).unwrap()
    }

    fn manifest(runtime_version: &str, tool_description: &str) -> EpochContractManifest {
        manifest_with_turn_loop_limit(runtime_version, tool_description, NonZeroUsize::MIN)
    }

    fn manifest_with_turn_loop_limit(
        runtime_version: &str,
        tool_description: &str,
        turn_loop_max_turns: NonZeroUsize,
    ) -> EpochContractManifest {
        EpochContractManifest::new(
            EpochContractId::new("player/v1").unwrap(),
            "player-config/v1",
            "player-runtime/v1",
            turn_loop_max_turns,
            ProviderAdapterContract::new("test-provider", 1).unwrap(),
            vec![runtime_descriptor(runtime_version)],
            vec![tool_descriptor(tool_description)],
        )
        .unwrap()
    }

    fn rendered_artifact() -> RenderedEpochArtifact {
        RenderedEpochArtifact::new(
            DurableEpochId::new("forgotten-city/player/epoch-7").unwrap(),
            manifest("v1", "Inspect one location"),
            "stable system",
        )
        .unwrap()
    }

    #[test]
    fn manifest_detects_runtime_and_tool_changes_even_with_same_contract_id() {
        let expected = manifest("v1", "Inspect one location");
        let changed_runtime = manifest("v2", "Inspect one location");
        let changed_tool = manifest("v1", "Inspect the whole district");

        assert_eq!(
            expected.epoch_contract_id(),
            changed_runtime.epoch_contract_id()
        );
        assert_ne!(expected, changed_runtime);
        assert_ne!(expected, changed_tool);
    }

    #[test]
    fn turn_loop_policy_changes_the_durable_manifest_and_artifact_fingerprint() {
        let one_turn =
            manifest_with_turn_loop_limit("v1", "Inspect one location", NonZeroUsize::MIN);
        let two_turns = manifest_with_turn_loop_limit(
            "v1",
            "Inspect one location",
            NonZeroUsize::new(2).unwrap(),
        );
        assert_eq!(one_turn.turn_loop_max_turns(), 1);
        assert_eq!(two_turns.turn_loop_max_turns(), 2);
        assert_ne!(one_turn, two_turns);

        let one_turn_artifact = RenderedEpochArtifact::new(
            DurableEpochId::new("forgotten-city/player/loop-policy").unwrap(),
            one_turn,
            "stable system",
        )
        .unwrap();
        let two_turn_artifact = RenderedEpochArtifact::new(
            DurableEpochId::new("forgotten-city/player/loop-policy").unwrap(),
            two_turns,
            "stable system",
        )
        .unwrap();
        assert_ne!(
            one_turn_artifact.fingerprint(),
            two_turn_artifact.fingerprint()
        );
    }

    #[test]
    fn epoch_open_lease_requires_a_strictly_future_deadline() {
        assert!(matches!(
            EpochOpenLease::new(42, 42),
            Err(EpochContractManifestError::InvalidEpochOpenLease {
                issued_at_unix_ms: 42,
                expires_at_unix_ms: 42,
            })
        ));
        let lease = EpochOpenLease::new(42, 43).unwrap();
        assert_eq!(lease.issued_at_unix_ms(), 42);
        assert_eq!(lease.expires_at_unix_ms(), 43);
        assert!(lease.is_live_at(42));
        assert!(!lease.is_live_at(43));
    }

    #[test]
    fn manifest_rejects_ambiguous_runtime_and_tool_names() {
        let duplicate_runtime = EpochContractManifest::new(
            EpochContractId::new("player/v1").unwrap(),
            "player-config/v1",
            "player-runtime/v1",
            NonZeroUsize::MIN,
            ProviderAdapterContract::new("test-provider", 1).unwrap(),
            vec![runtime_descriptor("v1"), runtime_descriptor("v2")],
            vec![],
        );
        assert!(matches!(
            duplicate_runtime,
            Err(EpochContractManifestError::DuplicateRuntimeDeclaration { .. })
        ));

        let duplicate_tools = EpochContractManifest::new(
            EpochContractId::new("player/v1").unwrap(),
            "player-config/v1",
            "player-runtime/v1",
            NonZeroUsize::MIN,
            ProviderAdapterContract::new("test-provider", 1).unwrap(),
            vec![],
            vec![tool_descriptor("first"), tool_descriptor("second")],
        );
        assert!(matches!(
            duplicate_tools,
            Err(EpochContractManifestError::DuplicateProviderTool { .. })
        ));

        let binding_and_capability_share_an_id = EpochContractManifest::new(
            EpochContractId::new("player/v1").unwrap(),
            "player-config/v1",
            "player-runtime/v1",
            NonZeroUsize::MIN,
            ProviderAdapterContract::new("test-provider", 1).unwrap(),
            vec![runtime_descriptor("v1")],
            vec![ProviderToolDescriptor::new(
                "root/select_intent::xml",
                "v1",
                &ProviderToolSpec::new("inspect", "first", json!({ "type": "object" })).unwrap(),
            )
            .unwrap()],
        );
        assert!(matches!(
            binding_and_capability_share_an_id,
            Err(EpochContractManifestError::DuplicateRuntimeDeclaration { .. })
        ));

        let duplicate_route = EpochContractManifest::new(
            EpochContractId::new("player/v1").unwrap(),
            "player-config/v1",
            "player-runtime/v1",
            NonZeroUsize::MIN,
            ProviderAdapterContract::new("test-provider", 1).unwrap(),
            vec![
                runtime_descriptor("v1"),
                RuntimeBindingDescriptor::new("root/other::xml", "xml", "select_intent", "v1")
                    .unwrap(),
            ],
            vec![],
        );
        assert!(matches!(
            duplicate_route,
            Err(EpochContractManifestError::DuplicateRuntimeRoute { .. })
        ));
    }

    #[derive(Default)]
    struct IdempotentProviderState {
        remote_system_receives: usize,
        attach_calls: usize,
        rehydrate_calls: usize,
        sessions: HashMap<DurableEpochId, ProviderEpochReceipt>,
    }

    #[derive(Default)]
    struct IdempotentProvider {
        state: Arc<Mutex<IdempotentProviderState>>,
    }

    #[async_trait::async_trait]
    impl DurableProviderEpoch for IdempotentProvider {
        type Binding = DurableEpochId;
        type Error = Infallible;

        async fn attach_epoch(
            &self,
            request: ProviderEpochAttachRequest<'_>,
        ) -> Result<AttachedProviderEpoch<Self::Binding>, Self::Error> {
            let mut state = self.state.lock().unwrap();
            state.attach_calls += 1;
            let durable_epoch_id = request.durable_epoch_id().clone();
            let receipt = match state.sessions.get(&durable_epoch_id) {
                Some(receipt) => receipt.clone(),
                None => {
                    assert_eq!(request.system(), "stable system");
                    assert_eq!(request.tools().len(), 1);
                    state.remote_system_receives += 1;
                    let receipt = ProviderEpochReceipt::new(
                        "test-provider",
                        1,
                        durable_epoch_id.clone(),
                        request.fingerprint().clone(),
                        json!({ "remote_session": "session-7" }),
                    )
                    .unwrap();
                    state
                        .sessions
                        .insert(durable_epoch_id.clone(), receipt.clone());
                    receipt
                }
            };
            Ok(AttachedProviderEpoch::new(durable_epoch_id, receipt))
        }

        async fn rehydrate_epoch(
            &self,
            request: ProviderEpochRehydrateRequest<'_>,
        ) -> Result<Self::Binding, Self::Error> {
            let mut state = self.state.lock().unwrap();
            state.rehydrate_calls += 1;
            assert_eq!(request.receipt().adapter(), "test-provider");
            Ok(request.durable_epoch_id().clone())
        }
    }

    #[tokio::test]
    async fn attachment_retry_is_idempotent_and_rehydrate_carries_no_system() {
        let provider = IdempotentProvider::default();
        let rendered = rendered_artifact();

        let (_, first_receipt) = provider
            .attach_epoch(ProviderEpochAttachRequest::new(&rendered))
            .await
            .unwrap()
            .into_parts();
        let (_, retry_receipt) = provider
            .attach_epoch(ProviderEpochAttachRequest::new(&rendered))
            .await
            .unwrap()
            .into_parts();
        assert_eq!(first_receipt, retry_receipt);

        let active = ActiveEpochArtifact::new(rendered, retry_receipt).unwrap();
        let rebound = provider
            .rehydrate_epoch(ProviderEpochRehydrateRequest::new(&active, None).unwrap())
            .await
            .unwrap();
        assert_eq!(rebound, *active.rendered().durable_epoch_id());

        let state = provider.state.lock().unwrap();
        assert_eq!(state.attach_calls, 2);
        assert_eq!(state.remote_system_receives, 1);
        assert_eq!(state.rehydrate_calls, 1);
    }

    #[test]
    fn artifact_round_trips_without_process_local_epoch_identity() {
        let rendered = rendered_artifact();
        assert!(rendered.fingerprint().as_str().starts_with("sha256:"));
        assert_eq!(rendered.fingerprint().as_str().len(), 71);
        let encoded = serde_json::to_string(&rendered).unwrap();
        assert!(!encoded.contains("HarnessEpochId"));

        let decoded: RenderedEpochArtifact = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, rendered);
        assert_eq!(decoded.format_version(), EPOCH_ARTIFACT_FORMAT_VERSION);
        decoded.validate().unwrap();

        let mut tampered: Value = serde_json::from_str(&encoded).unwrap();
        tampered["rendered_system"] = Value::String("changed system".to_owned());
        let tampered: RenderedEpochArtifact = serde_json::from_value(tampered).unwrap();
        assert!(matches!(
            tampered.validate(),
            Err(EpochArtifactValidationError::FingerprintMismatch { .. })
        ));

        let mut zero_turn_limit: Value = serde_json::from_str(&encoded).unwrap();
        zero_turn_limit["manifest"]["turn_loop_max_turns"] = Value::from(0_u64);
        assert!(serde_json::from_value::<RenderedEpochArtifact>(zero_turn_limit).is_err());

        let wrong_receipt = ProviderEpochReceipt::new(
            "other-provider",
            1,
            rendered.durable_epoch_id().clone(),
            rendered.fingerprint().clone(),
            json!({ "session": "7" }),
        )
        .unwrap();
        assert!(matches!(
            ActiveEpochArtifact::new(rendered, wrong_receipt),
            Err(EpochArtifactActivationError::ProviderReceiptContract { .. })
        ));
    }

    #[test]
    fn legacy_artifacts_are_rejected_after_manifest_upgrades() {
        let rendered = rendered_artifact();
        for format_version in [1_u32, 2] {
            let mut encoded = serde_json::to_value(&rendered).unwrap();
            encoded["format_version"] = Value::from(format_version);
            let old: RenderedEpochArtifact = serde_json::from_value(encoded).unwrap();

            assert!(matches!(
                old.validate(),
                Err(EpochArtifactValidationError::UnsupportedFormatVersion {
                    expected: EPOCH_ARTIFACT_FORMAT_VERSION,
                    actual,
                }) if actual == format_version
            ));
        }
    }
}

#[cfg(test)]
#[path = "durable_epoch/coordinator_tests.rs"]
mod coordinator_tests;
