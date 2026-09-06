//! Platform-neutral abstractions, specifications, and test seams for Windows Filtering Platform (WFP).

use std::collections::HashMap;
use std::fmt;

#[cfg(windows)]
pub use windows::core::GUID;

#[cfg(not(windows))]
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct GUID {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

#[cfg(not(windows))]
impl GUID {
    pub const fn from_u128(uuid: u128) -> Self {
        Self {
            data1: (uuid >> 96) as u32,
            data2: (uuid >> 80) as u16,
            data3: (uuid >> 64) as u16,
            data4: (uuid as u64).to_be_bytes(),
        }
    }
}

use crate::internet_gate::WindowsWfpError;

// Canonical Microsoft Winerror.h / FWP_E_* error constants
pub const FWP_E_FILTER_NOT_FOUND: u32 = 0x80320003;
pub const FWP_E_PROVIDER_NOT_FOUND: u32 = 0x80320005;
pub const FWP_E_SUBLAYER_NOT_FOUND: u32 = 0x80320007;
pub const FWP_E_NOT_FOUND: u32 = 0x80320008;
pub const FWP_E_ALREADY_EXISTS: u32 = 0x80320009;
pub const FWP_E_IN_USE: u32 = 0x8032000A;
pub const FWP_E_DYNAMIC_SESSION_IN_PROGRESS: u32 = 0x8032000B;
pub const FWP_E_WRONG_SESSION: u32 = 0x8032000C;
pub const FWP_E_NO_TXN_IN_PROGRESS: u32 = 0x8032000D;
pub const FWP_E_TXN_IN_PROGRESS: u32 = 0x8032000E;
pub const FWP_E_TXN_ABORTED: u32 = 0x8032000F;
pub const FWP_E_SESSION_ABORTED: u32 = 0x80320010;
pub const FWP_E_INCOMPATIBLE_TXN: u32 = 0x80320011;
pub const FWP_E_TIMEOUT: u32 = 0x80320012;

// Canonical WFP condition / enumeration constants
pub const FWP_MATCH_EQUAL: u32 = 0;
pub const FWP_SECURITY_DESCRIPTOR_TYPE: u32 = 14;
pub const FWPM_CONDITION_ALE_USER_ID: GUID =
    GUID::from_u128(0xaf043a0a_b34d_4f86_979c_c90371af6e66);
pub const FWP_ACTRL_MATCH_FILTER: u32 = 0x00000001;
pub const FWP_FILTER_ENUM_FLAG_INCLUDE_BOOTTIME: u32 = 0x00000008;
pub const FWP_FILTER_ENUM_FLAG_INCLUDE_DISABLED: u32 = 0x00000010;

// Canonical FWPM filter flags (Microsoft Windows SDK)
pub const FWPM_FILTER_FLAG_PERSISTENT: u32 = 0x00000001;
pub const FWPM_FILTER_FLAG_BOOTTIME: u32 = 0x00000002;
pub const FWPM_FILTER_FLAG_HAS_PROVIDER_CONTEXT: u32 = 0x00000004;
pub const FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT: u32 = 0x00000008;
pub const FWPM_FILTER_FLAG_PERMIT_IF_CALLOUT_UNREGISTERED: u32 = 0x00000010;
pub const FWPM_FILTER_FLAG_DISABLED: u32 = 0x00000020;
pub const FWPM_FILTER_FLAG_INDEXED: u32 = 0x00000040;

// Canonical FWPM provider flags (Microsoft Windows SDK)
pub const FWPM_PROVIDER_FLAG_PERSISTENT: u32 = 0x00000001;
pub const FWPM_PROVIDER_FLAG_DISABLED: u32 = 0x00000010;

// Canonical FWPM sublayer flags (Microsoft Windows SDK)
pub const FWPM_SUBLAYER_FLAG_PERSISTENT: u32 = 0x00000001;

/// WFP filter action type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WfpActionType {
    Block,
    Permit,
    Callout,
    Other(u32),
}

impl fmt::Display for WfpActionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Block => write!(f, "Block"),
            Self::Permit => write!(f, "Permit"),
            Self::Callout => write!(f, "Callout"),
            Self::Other(v) => write!(f, "Other(0x{v:08X})"),
        }
    }
}

/// Specification for creating or updating a WFP provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WfpProviderSpec {
    pub provider_key: GUID,
    pub display_name: String,
    pub description: String,
    pub service_name: Option<String>,
    pub provider_data: Vec<u8>,
    pub persistent: bool,
}

/// Truthful snapshot of an inspectable WFP provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WfpProviderSnapshot {
    pub provider_key: GUID,
    pub display_name: String,
    pub description: String,
    pub service_name: Option<String>,
    pub provider_data: Vec<u8>,
    pub flags: u32,
    pub disabled: bool,
}

/// Specification for creating a WFP sublayer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WfpSubLayerSpec {
    pub sublayer_key: GUID,
    pub display_name: String,
    pub description: String,
    pub provider_key: GUID,
    pub weight: u16,
    pub persistent: bool,
}

/// Truthful snapshot of an inspectable WFP sublayer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WfpSubLayerSnapshot {
    pub sublayer_key: GUID,
    pub display_name: String,
    pub description: String,
    pub provider_key: GUID,
    pub weight: u16,
    pub flags: u32,
}

/// Snapshot of an Access Control Entry (ACE) inside a WFP condition security descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WfpSidAceSnapshot {
    pub sid: String,
    pub mask: u32,
    pub is_allow: bool,
    pub ace_flags: u8,
}

/// Structured snapshot of a single WFP filter condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WfpConditionSnapshot {
    pub field_key: GUID,
    pub match_type: u32,
    pub condition_value_type: u32,
    pub is_valid_sd: bool,
    pub dacl_present: bool,
    pub is_self_relative: bool,
    pub aces: Vec<WfpSidAceSnapshot>,
}

impl WfpConditionSnapshot {
    /// Constructs a canonical ALE user ID condition snapshot for testing or specification.
    pub fn canonical_child_sid(child_sid: &str) -> Self {
        Self {
            field_key: FWPM_CONDITION_ALE_USER_ID,
            match_type: FWP_MATCH_EQUAL,
            condition_value_type: FWP_SECURITY_DESCRIPTOR_TYPE,
            is_valid_sd: true,
            dacl_present: true,
            is_self_relative: true,
            aces: vec![WfpSidAceSnapshot {
                sid: child_sid.to_string(),
                mask: FWP_ACTRL_MATCH_FILTER,
                is_allow: true,
                ace_flags: 0,
            }],
        }
    }
}

/// Specification for creating a canonical WFP blocking filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WfpFilterSpec {
    pub filter_key: GUID,
    pub display_name: String,
    pub description: String,
    pub layer_key: GUID,
    pub sublayer_key: GUID,
    pub provider_key: Option<GUID>,
    pub weight: u8,
    pub action_type: WfpActionType,
    pub clear_action_right: bool,
    pub persistent: bool,
    pub sid_condition: Option<String>,
}

/// Truthful snapshot of an inspectable WFP filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WfpFilterSnapshot {
    pub filter_key: GUID,
    pub display_name: String,
    pub description: String,
    pub layer_key: GUID,
    pub sublayer_key: GUID,
    pub provider_key: Option<GUID>,
    pub weight: u8,
    pub action_type: WfpActionType,
    pub flags: u32,
    pub disabled: bool,
    pub clear_action_right: bool,
    pub conditions: Vec<WfpConditionSnapshot>,
}

impl WfpFilterSnapshot {
    /// Returns the single child SID if the filter has a single condition with a single allow ACE.
    pub fn single_child_sid(&self) -> Option<&str> {
        if self.conditions.len() == 1 {
            let cond = &self.conditions[0];
            if cond.aces.len() == 1 && cond.aces[0].is_allow {
                return Some(&cond.aces[0].sid);
            }
        }
        None
    }
}

/// Port abstraction for interaction with the Windows Filtering Platform engine.
pub trait WfpEnginePort: Send + Sync {
    /// Authoritatively validates that child_sid is a valid functional Windows SID.
    fn validate_sid(&self, child_sid: &str) -> Result<(), WindowsWfpError>;

    /// Begins an explicit BFE write transaction.
    fn transaction_begin(&mut self) -> Result<(), WindowsWfpError>;

    /// Commits the active BFE write transaction.
    fn transaction_commit(&mut self) -> Result<(), WindowsWfpError>;

    /// Aborts the active BFE write transaction.
    fn transaction_abort(&mut self) -> Result<(), WindowsWfpError>;

    /// Queries a provider by its unique GUID key.
    fn get_provider(&self, key: &GUID) -> Result<Option<WfpProviderSnapshot>, WindowsWfpError>;

    /// Adds a persistent provider to BFE.
    fn add_provider(&mut self, spec: &WfpProviderSpec) -> Result<(), WindowsWfpError>;

    /// Deletes a provider by its GUID key.
    fn delete_provider(&mut self, key: &GUID) -> Result<(), WindowsWfpError>;

    /// Queries a sublayer by its unique GUID key.
    fn get_sublayer(&self, key: &GUID) -> Result<Option<WfpSubLayerSnapshot>, WindowsWfpError>;

    /// Adds a persistent sublayer to BFE.
    fn add_sublayer(&mut self, spec: &WfpSubLayerSpec) -> Result<(), WindowsWfpError>;

    /// Deletes a sublayer by its GUID key.
    fn delete_sublayer(&mut self, key: &GUID) -> Result<(), WindowsWfpError>;

    /// Queries a filter by its unique GUID key.
    fn get_filter(&self, key: &GUID) -> Result<Option<WfpFilterSnapshot>, WindowsWfpError>;

    /// Adds a persistent filter to BFE.
    fn add_filter(&mut self, spec: &WfpFilterSpec) -> Result<(), WindowsWfpError>;

    /// Deletes a filter by its GUID key.
    fn delete_filter(&mut self, key: &GUID) -> Result<(), WindowsWfpError>;

    /// Enumerates all filters owned by the given provider GUID.
    fn enum_filters_by_provider(
        &self,
        provider_key: &GUID,
    ) -> Result<Vec<WfpFilterSnapshot>, WindowsWfpError>;
}

/// Deterministic in-memory fake implementation of [`WfpEnginePort`] for unit testing.
#[derive(Debug, Clone, Default)]
pub struct FakeWfpEnginePort {
    committed_providers: HashMap<GUID, WfpProviderSnapshot>,
    committed_sublayers: HashMap<GUID, WfpSubLayerSnapshot>,
    committed_filters: HashMap<GUID, WfpFilterSnapshot>,

    in_transaction: bool,
    tx_providers: HashMap<GUID, WfpProviderSnapshot>,
    tx_sublayers: HashMap<GUID, WfpSubLayerSnapshot>,
    tx_filters: HashMap<GUID, WfpFilterSnapshot>,

    pub rejected_sids: std::collections::HashSet<String>,

    // Simulated failure injection hooks:
    pub fail_transaction_begin: Option<u32>,
    pub fail_transaction_commit: Option<u32>,
    pub fail_transaction_abort: Option<u32>,
    pub fail_provider_query: Option<u32>,
    pub fail_provider_mutation: Option<u32>,
    pub fail_sublayer_query: Option<u32>,
    pub fail_sublayer_mutation: Option<u32>,
    pub fail_filter_query: Option<u32>,
    pub fail_filter_add: Option<u32>,
    pub fail_filter_delete: Option<u32>,
    pub fail_filter_enum: Option<u32>,
}

impl FakeWfpEnginePort {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_raw_provider(&mut self, snapshot: WfpProviderSnapshot) {
        self.committed_providers
            .insert(snapshot.provider_key, snapshot);
    }

    pub fn insert_raw_sublayer(&mut self, snapshot: WfpSubLayerSnapshot) {
        self.committed_sublayers
            .insert(snapshot.sublayer_key, snapshot);
    }

    pub fn insert_raw_filter(&mut self, snapshot: WfpFilterSnapshot) {
        self.committed_filters.insert(snapshot.filter_key, snapshot);
    }

    pub fn get_committed_filter(&self, key: &GUID) -> Option<&WfpFilterSnapshot> {
        self.committed_filters.get(key)
    }

    pub fn committed_filters(&self) -> &HashMap<GUID, WfpFilterSnapshot> {
        &self.committed_filters
    }

    pub fn committed_providers(&self) -> &HashMap<GUID, WfpProviderSnapshot> {
        &self.committed_providers
    }

    pub fn committed_sublayers(&self) -> &HashMap<GUID, WfpSubLayerSnapshot> {
        &self.committed_sublayers
    }

    pub fn is_in_transaction(&self) -> bool {
        self.in_transaction
    }
}

impl WfpEnginePort for FakeWfpEnginePort {
    fn validate_sid(&self, child_sid: &str) -> Result<(), WindowsWfpError> {
        if self.rejected_sids.contains(child_sid) {
            return Err(WindowsWfpError::InvalidSid {
                sid: child_sid.to_string(),
                win32_code: 1337, // ERROR_INVALID_SID
            });
        }
        let clean = child_sid.trim();
        if !clean.starts_with("S-1-") {
            return Err(WindowsWfpError::InvalidSid {
                sid: child_sid.to_string(),
                win32_code: 1337,
            });
        }
        let parts: Vec<&str> = clean[4..].split('-').collect();
        if parts.is_empty() || parts.len() > 16 {
            return Err(WindowsWfpError::InvalidSid {
                sid: child_sid.to_string(),
                win32_code: 1337,
            });
        }
        let auth = parts[0]
            .parse::<u64>()
            .map_err(|_| WindowsWfpError::InvalidSid {
                sid: child_sid.to_string(),
                win32_code: 1337,
            })?;
        if auth > 0xFFFF_FFFF_FFFF {
            return Err(WindowsWfpError::InvalidSid {
                sid: child_sid.to_string(),
                win32_code: 1337,
            });
        }
        for sub in &parts[1..] {
            sub.parse::<u32>()
                .map_err(|_| WindowsWfpError::InvalidSid {
                    sid: child_sid.to_string(),
                    win32_code: 1337,
                })?;
        }
        Ok(())
    }

    fn transaction_begin(&mut self) -> Result<(), WindowsWfpError> {
        if let Some(code) = self.fail_transaction_begin.take() {
            return Err(WindowsWfpError::TransactionBeginFailure { win32_code: code });
        }
        if self.in_transaction {
            return Err(WindowsWfpError::TransactionBeginFailure {
                win32_code: FWP_E_TXN_IN_PROGRESS,
            });
        }
        self.in_transaction = true;
        self.tx_providers = self.committed_providers.clone();
        self.tx_sublayers = self.committed_sublayers.clone();
        self.tx_filters = self.committed_filters.clone();
        Ok(())
    }

    fn transaction_commit(&mut self) -> Result<(), WindowsWfpError> {
        if let Some(code) = self.fail_transaction_commit.take() {
            return Err(WindowsWfpError::TransactionCommitFailure { win32_code: code });
        }
        if !self.in_transaction {
            return Err(WindowsWfpError::TransactionCommitFailure {
                win32_code: FWP_E_NO_TXN_IN_PROGRESS,
            });
        }
        self.committed_providers = std::mem::take(&mut self.tx_providers);
        self.committed_sublayers = std::mem::take(&mut self.tx_sublayers);
        self.committed_filters = std::mem::take(&mut self.tx_filters);
        self.in_transaction = false;
        Ok(())
    }

    fn transaction_abort(&mut self) -> Result<(), WindowsWfpError> {
        if let Some(code) = self.fail_transaction_abort.take() {
            return Err(WindowsWfpError::TransactionAbortFailure { win32_code: code });
        }
        if !self.in_transaction {
            return Err(WindowsWfpError::TransactionAbortFailure {
                win32_code: FWP_E_NO_TXN_IN_PROGRESS,
            });
        }
        self.tx_providers.clear();
        self.tx_sublayers.clear();
        self.tx_filters.clear();
        self.in_transaction = false;
        Ok(())
    }

    fn get_provider(&self, key: &GUID) -> Result<Option<WfpProviderSnapshot>, WindowsWfpError> {
        if let Some(code) = self.fail_provider_query {
            return Err(WindowsWfpError::ProviderQueryFailure { win32_code: code });
        }
        let map = if self.in_transaction {
            &self.tx_providers
        } else {
            &self.committed_providers
        };
        Ok(map.get(key).cloned())
    }

    fn add_provider(&mut self, spec: &WfpProviderSpec) -> Result<(), WindowsWfpError> {
        if let Some(code) = self.fail_provider_mutation.take() {
            return Err(WindowsWfpError::ProviderMutationFailure { win32_code: code });
        }
        let map = if self.in_transaction {
            &mut self.tx_providers
        } else {
            &mut self.committed_providers
        };
        if map.contains_key(&spec.provider_key) {
            return Err(WindowsWfpError::ProviderMutationFailure {
                win32_code: 0x80320009,
            }); // FWP_E_ALREADY_EXISTS
        }
        map.insert(
            spec.provider_key,
            WfpProviderSnapshot {
                provider_key: spec.provider_key,
                display_name: spec.display_name.clone(),
                description: spec.description.clone(),
                service_name: spec.service_name.clone(),
                provider_data: spec.provider_data.clone(),
                flags: if spec.persistent {
                    FWPM_PROVIDER_FLAG_PERSISTENT
                } else {
                    0
                },
                disabled: false,
            },
        );
        Ok(())
    }

    fn delete_provider(&mut self, key: &GUID) -> Result<(), WindowsWfpError> {
        if let Some(code) = self.fail_provider_mutation.take() {
            return Err(WindowsWfpError::ProviderMutationFailure { win32_code: code });
        }
        let map = if self.in_transaction {
            &mut self.tx_providers
        } else {
            &mut self.committed_providers
        };
        map.remove(key);
        Ok(())
    }

    fn get_sublayer(&self, key: &GUID) -> Result<Option<WfpSubLayerSnapshot>, WindowsWfpError> {
        if let Some(code) = self.fail_sublayer_query {
            return Err(WindowsWfpError::SublayerQueryFailure { win32_code: code });
        }
        let map = if self.in_transaction {
            &self.tx_sublayers
        } else {
            &self.committed_sublayers
        };
        Ok(map.get(key).cloned())
    }

    fn add_sublayer(&mut self, spec: &WfpSubLayerSpec) -> Result<(), WindowsWfpError> {
        if let Some(code) = self.fail_sublayer_mutation.take() {
            return Err(WindowsWfpError::SublayerMutationFailure { win32_code: code });
        }
        let map = if self.in_transaction {
            &mut self.tx_sublayers
        } else {
            &mut self.committed_sublayers
        };
        if map.contains_key(&spec.sublayer_key) {
            return Err(WindowsWfpError::SublayerMutationFailure {
                win32_code: 0x80320009,
            });
        }
        map.insert(
            spec.sublayer_key,
            WfpSubLayerSnapshot {
                sublayer_key: spec.sublayer_key,
                display_name: spec.display_name.clone(),
                description: spec.description.clone(),
                provider_key: spec.provider_key,
                weight: spec.weight,
                flags: if spec.persistent {
                    FWPM_SUBLAYER_FLAG_PERSISTENT
                } else {
                    0
                },
            },
        );
        Ok(())
    }

    fn delete_sublayer(&mut self, key: &GUID) -> Result<(), WindowsWfpError> {
        if let Some(code) = self.fail_sublayer_mutation.take() {
            return Err(WindowsWfpError::SublayerMutationFailure { win32_code: code });
        }
        let map = if self.in_transaction {
            &mut self.tx_sublayers
        } else {
            &mut self.committed_sublayers
        };
        map.remove(key);
        Ok(())
    }

    fn get_filter(&self, key: &GUID) -> Result<Option<WfpFilterSnapshot>, WindowsWfpError> {
        if let Some(code) = self.fail_filter_query {
            return Err(WindowsWfpError::FilterQueryFailure { win32_code: code });
        }
        let map = if self.in_transaction {
            &self.tx_filters
        } else {
            &self.committed_filters
        };
        Ok(map.get(key).cloned())
    }

    fn add_filter(&mut self, spec: &WfpFilterSpec) -> Result<(), WindowsWfpError> {
        if let Some(code) = self.fail_filter_add.take() {
            return Err(WindowsWfpError::FilterAddFailure { win32_code: code });
        }
        let map = if self.in_transaction {
            &mut self.tx_filters
        } else {
            &mut self.committed_filters
        };
        if map.contains_key(&spec.filter_key) {
            return Err(WindowsWfpError::FilterAddFailure {
                win32_code: 0x80320009,
            });
        }
        let mut flags = 0u32;
        if spec.persistent {
            flags |= FWPM_FILTER_FLAG_PERSISTENT;
        }
        if spec.clear_action_right {
            flags |= FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT;
        }
        let conditions = if let Some(ref sid) = spec.sid_condition {
            vec![WfpConditionSnapshot::canonical_child_sid(sid)]
        } else {
            Vec::new()
        };
        map.insert(
            spec.filter_key,
            WfpFilterSnapshot {
                filter_key: spec.filter_key,
                display_name: spec.display_name.clone(),
                description: spec.description.clone(),
                layer_key: spec.layer_key,
                sublayer_key: spec.sublayer_key,
                provider_key: spec.provider_key,
                weight: spec.weight,
                action_type: spec.action_type,
                flags,
                disabled: false,
                clear_action_right: spec.clear_action_right,
                conditions,
            },
        );
        Ok(())
    }

    fn delete_filter(&mut self, key: &GUID) -> Result<(), WindowsWfpError> {
        if let Some(code) = self.fail_filter_delete.take() {
            return Err(WindowsWfpError::FilterDeleteFailure { win32_code: code });
        }
        let map = if self.in_transaction {
            &mut self.tx_filters
        } else {
            &mut self.committed_filters
        };
        map.remove(key);
        Ok(())
    }

    fn enum_filters_by_provider(
        &self,
        provider_key: &GUID,
    ) -> Result<Vec<WfpFilterSnapshot>, WindowsWfpError> {
        if let Some(code) = self.fail_filter_enum {
            return Err(WindowsWfpError::FilterEnumerationFailure { win32_code: code });
        }
        let map = if self.in_transaction {
            &self.tx_filters
        } else {
            &self.committed_filters
        };
        let res = map
            .values()
            .filter(|f| f.provider_key.as_ref() == Some(provider_key))
            .cloned()
            .collect();
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonical_wfp_error_identities() {
        assert_eq!(FWP_E_FILTER_NOT_FOUND, 0x80320003);
        assert_eq!(FWP_E_PROVIDER_NOT_FOUND, 0x80320005);
        assert_eq!(FWP_E_SUBLAYER_NOT_FOUND, 0x80320007);
        assert_eq!(FWP_E_NOT_FOUND, 0x80320008);
        assert_eq!(FWP_E_ALREADY_EXISTS, 0x80320009);
        assert_eq!(FWP_E_IN_USE, 0x8032000A);
        assert_eq!(FWP_E_DYNAMIC_SESSION_IN_PROGRESS, 0x8032000B);
        assert_eq!(FWP_E_WRONG_SESSION, 0x8032000C);
        assert_eq!(FWP_E_NO_TXN_IN_PROGRESS, 0x8032000D);
        assert_eq!(FWP_E_TXN_IN_PROGRESS, 0x8032000E);
        assert_eq!(FWP_E_TXN_ABORTED, 0x8032000F);
        assert_eq!(FWP_E_SESSION_ABORTED, 0x80320010);
        assert_eq!(FWP_E_INCOMPATIBLE_TXN, 0x80320011);
        assert_eq!(FWP_E_TIMEOUT, 0x80320012);

        assert_eq!(FWP_MATCH_EQUAL, 0);
        assert_eq!(FWP_SECURITY_DESCRIPTOR_TYPE, 14);
        assert_eq!(FWP_ACTRL_MATCH_FILTER, 0x00000001);
        assert_eq!(FWP_FILTER_ENUM_FLAG_INCLUDE_BOOTTIME, 0x00000008);
        assert_eq!(FWP_FILTER_ENUM_FLAG_INCLUDE_DISABLED, 0x00000010);
    }

    #[test]
    fn test_canonical_constants_match_windows_sdk() {
        assert_eq!(FWPM_FILTER_FLAG_PERSISTENT, 0x00000001);
        assert_eq!(FWPM_FILTER_FLAG_BOOTTIME, 0x00000002);
        assert_eq!(FWPM_FILTER_FLAG_HAS_PROVIDER_CONTEXT, 0x00000004);
        assert_eq!(FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT, 0x00000008);
        assert_eq!(FWPM_FILTER_FLAG_PERMIT_IF_CALLOUT_UNREGISTERED, 0x00000010);
        assert_eq!(FWPM_FILTER_FLAG_DISABLED, 0x00000020);
        assert_eq!(FWPM_FILTER_FLAG_INDEXED, 0x00000040);

        assert_eq!(FWPM_PROVIDER_FLAG_PERSISTENT, 0x00000001);
        assert_eq!(FWPM_PROVIDER_FLAG_DISABLED, 0x00000010);

        assert_eq!(FWPM_SUBLAYER_FLAG_PERSISTENT, 0x00000001);

        // Explicit proof that FWPM_FILTER_FLAG_BOOTTIME != FWP_FILTER_ENUM_FLAG_INCLUDE_BOOTTIME
        assert_ne!(
            FWPM_FILTER_FLAG_BOOTTIME,
            FWP_FILTER_ENUM_FLAG_INCLUDE_BOOTTIME
        );
        assert_eq!(FWPM_FILTER_FLAG_BOOTTIME, 0x00000002);
        assert_eq!(FWP_FILTER_ENUM_FLAG_INCLUDE_BOOTTIME, 0x00000008);

        // Explicit proof that FWPM_FILTER_FLAG_DISABLED != FWP_FILTER_ENUM_FLAG_INCLUDE_DISABLED
        assert_ne!(
            FWPM_FILTER_FLAG_DISABLED,
            FWP_FILTER_ENUM_FLAG_INCLUDE_DISABLED
        );
        assert_eq!(FWPM_FILTER_FLAG_DISABLED, 0x00000020);
        assert_eq!(FWP_FILTER_ENUM_FLAG_INCLUDE_DISABLED, 0x00000010);

        #[cfg(windows)]
        {
            use windows::Win32::NetworkManagement::WindowsFilteringPlatform::{
                FWP_ACTRL_MATCH_FILTER as WIN_ACTRL_MATCH_FILTER,
                FWP_FILTER_ENUM_FLAG_INCLUDE_BOOTTIME as WIN_INCLUDE_BOOTTIME,
                FWP_FILTER_ENUM_FLAG_INCLUDE_DISABLED as WIN_INCLUDE_DISABLED,
                FWP_MATCH_EQUAL as WIN_MATCH_EQUAL,
                FWP_SECURITY_DESCRIPTOR_TYPE as WIN_SECURITY_DESCRIPTOR_TYPE,
                FWPM_CONDITION_ALE_USER_ID as WIN_ALE_USER_ID,
                FWPM_FILTER_FLAG_BOOTTIME as WIN_FILTER_FLAG_BOOTTIME,
                FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT as WIN_FILTER_FLAG_CLEAR_ACTION_RIGHT,
                FWPM_FILTER_FLAG_DISABLED as WIN_FILTER_FLAG_DISABLED,
                FWPM_FILTER_FLAG_HAS_PROVIDER_CONTEXT as WIN_FILTER_FLAG_HAS_PROVIDER_CONTEXT,
                FWPM_FILTER_FLAG_INDEXED as WIN_FILTER_FLAG_INDEXED,
                FWPM_FILTER_FLAG_PERMIT_IF_CALLOUT_UNREGISTERED as WIN_FILTER_FLAG_PERMIT_IF_CALLOUT_UNREGISTERED,
                FWPM_FILTER_FLAG_PERSISTENT as WIN_FILTER_FLAG_PERSISTENT,
                FWPM_PROVIDER_FLAG_DISABLED as WIN_PROVIDER_FLAG_DISABLED,
                FWPM_PROVIDER_FLAG_PERSISTENT as WIN_PROVIDER_FLAG_PERSISTENT,
                FWPM_SUBLAYER_FLAG_PERSISTENT as WIN_SUBLAYER_FLAG_PERSISTENT,
            };
            assert_eq!(FWPM_CONDITION_ALE_USER_ID, WIN_ALE_USER_ID);
            assert_eq!(FWP_MATCH_EQUAL, WIN_MATCH_EQUAL.0 as u32);
            assert_eq!(
                FWP_SECURITY_DESCRIPTOR_TYPE,
                WIN_SECURITY_DESCRIPTOR_TYPE.0 as u32
            );
            assert_eq!(FWP_ACTRL_MATCH_FILTER, WIN_ACTRL_MATCH_FILTER);
            assert_eq!(FWP_FILTER_ENUM_FLAG_INCLUDE_BOOTTIME, WIN_INCLUDE_BOOTTIME);
            assert_eq!(FWP_FILTER_ENUM_FLAG_INCLUDE_DISABLED, WIN_INCLUDE_DISABLED);

            assert_eq!(FWPM_FILTER_FLAG_PERSISTENT, WIN_FILTER_FLAG_PERSISTENT.0);
            assert_eq!(FWPM_FILTER_FLAG_BOOTTIME, WIN_FILTER_FLAG_BOOTTIME.0);
            assert_eq!(
                FWPM_FILTER_FLAG_HAS_PROVIDER_CONTEXT,
                WIN_FILTER_FLAG_HAS_PROVIDER_CONTEXT.0
            );
            assert_eq!(
                FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT,
                WIN_FILTER_FLAG_CLEAR_ACTION_RIGHT.0
            );
            assert_eq!(
                FWPM_FILTER_FLAG_PERMIT_IF_CALLOUT_UNREGISTERED,
                WIN_FILTER_FLAG_PERMIT_IF_CALLOUT_UNREGISTERED.0
            );
            assert_eq!(FWPM_FILTER_FLAG_DISABLED, WIN_FILTER_FLAG_DISABLED.0);
            assert_eq!(FWPM_FILTER_FLAG_INDEXED, WIN_FILTER_FLAG_INDEXED.0);

            assert_eq!(FWPM_PROVIDER_FLAG_PERSISTENT, WIN_PROVIDER_FLAG_PERSISTENT);
            assert_eq!(FWPM_PROVIDER_FLAG_DISABLED, WIN_PROVIDER_FLAG_DISABLED);

            assert_eq!(FWPM_SUBLAYER_FLAG_PERSISTENT, WIN_SUBLAYER_FLAG_PERSISTENT);
        }
    }

    #[test]
    fn fake_double_begin_returns_txn_in_progress() {
        let mut fake = FakeWfpEnginePort::new();
        fake.transaction_begin().unwrap();
        let err = fake.transaction_begin().unwrap_err();
        assert_eq!(
            err,
            WindowsWfpError::TransactionBeginFailure {
                win32_code: FWP_E_TXN_IN_PROGRESS
            }
        );
    }

    #[test]
    fn fake_commit_without_transaction_returns_no_txn() {
        let mut fake = FakeWfpEnginePort::new();
        let err = fake.transaction_commit().unwrap_err();
        assert_eq!(
            err,
            WindowsWfpError::TransactionCommitFailure {
                win32_code: FWP_E_NO_TXN_IN_PROGRESS
            }
        );
    }

    #[test]
    fn fake_abort_without_transaction_returns_no_txn() {
        let mut fake = FakeWfpEnginePort::new();
        let err = fake.transaction_abort().unwrap_err();
        assert_eq!(
            err,
            WindowsWfpError::TransactionAbortFailure {
                win32_code: FWP_E_NO_TXN_IN_PROGRESS
            }
        );
    }
}
