//! Windows InternetGate orchestration, state truth model, and idempotency algorithms.

use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use palka_core::InternetState;

use crate::wfp::{
    FWP_ACTRL_MATCH_FILTER, FWP_MATCH_EQUAL, FWP_SECURITY_DESCRIPTOR_TYPE,
    FWPM_CONDITION_ALE_USER_ID, FWPM_FILTER_FLAG_PERSISTENT, FWPM_PROVIDER_FLAG_PERSISTENT,
    FWPM_SUBLAYER_FLAG_PERSISTENT, GUID, WfpActionType, WfpEnginePort, WfpFilterSnapshot,
    WfpFilterSpec, WfpProviderSnapshot, WfpProviderSpec, WfpSubLayerSnapshot, WfpSubLayerSpec,
};
use crate::wfp_windows::WindowsWfpEngine;

// Frozen WFP V1 GUID Identity
pub const PALKA_WFP_PROVIDER_KEY: GUID = GUID::from_u128(0xc8e542ce_d407_5540_a6cb_95b9130de318);
pub const PALKA_WFP_SUBLAYER_KEY: GUID = GUID::from_u128(0xf7c2f54d_461b_51b5_a747_4c98abd5fa20);

pub const FILTER_ALE_AUTH_CONNECT_V4_KEY: GUID =
    GUID::from_u128(0x749b417e_65fc_5d78_87be_1783dbd1054b);
pub const FILTER_ALE_AUTH_CONNECT_V6_KEY: GUID =
    GUID::from_u128(0xe9ad3f66_3337_5e9b_9eed_9cbca464a890);
pub const FILTER_ALE_AUTH_RECV_ACCEPT_V4_KEY: GUID =
    GUID::from_u128(0x82f7d08c_7d45_58d9_b30e_4a7eac8ab56c);
pub const FILTER_ALE_AUTH_RECV_ACCEPT_V6_KEY: GUID =
    GUID::from_u128(0x8d785cdf_663f_5ea1_828d_ddabaa788c00);

// Canonical ALE Layer GUIDs (Windows Filtering Platform)
pub const FWPM_LAYER_ALE_AUTH_CONNECT_V4: GUID =
    GUID::from_u128(0xc38d57d1_05a7_4c33_904f_7fbceee60e82);
pub const FWPM_LAYER_ALE_AUTH_CONNECT_V6: GUID =
    GUID::from_u128(0x4a72393b_319f_44bc_84c2_ba54d03b067f);
pub const FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4: GUID =
    GUID::from_u128(0xe1cd453a_0967_4a0a_9d08_a53443eb1496);
pub const FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6: GUID =
    GUID::from_u128(0x84e1b8b8_3e4b_4871_87a3_e8e3d6411f7c);

// Canonical Provider and Sublayer specifications
pub const PALKA_WFP_PROVIDER_NAME: &str = "PALKA Parental Control WFP Provider";
pub const PALKA_WFP_PROVIDER_DESC: &str =
    "Enforces parental control internet restrictions for configured user accounts";
pub const PALKA_WFP_PROVIDER_DATA: &[u8] = b"PALKA-WFP-V1";
pub const PALKA_WFP_SERVICE_NAME: &str = "PalkaService";

pub const PALKA_WFP_SUBLAYER_NAME: &str = "PALKA Sublayer";
pub const PALKA_WFP_SUBLAYER_DESC: &str =
    "Arbitrates parental control internet restriction filters";
pub const PALKA_WFP_SUBLAYER_WEIGHT: u16 = 0x8000;

pub const PALKA_WFP_FILTER_WEIGHT: u8 = 15;

/// Typed platform error taxonomy preserving native Win32/WFP/RPC codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowsWfpError {
    UnsupportedPlatform,
    InvalidSid { sid: String, win32_code: u32 },
    EngineUnavailable { win32_code: u32 },
    AccessDenied { win32_code: u32 },
    TransactionBeginFailure { win32_code: u32 },
    TransactionCommitFailure { win32_code: u32 },
    TransactionAbortFailure { win32_code: u32 },
    ProviderQueryFailure { win32_code: u32 },
    SublayerQueryFailure { win32_code: u32 },
    FilterQueryFailure { win32_code: u32 },
    FilterEnumerationFailure { win32_code: u32 },
    ProviderMutationFailure { win32_code: u32 },
    SublayerMutationFailure { win32_code: u32 },
    FilterAddFailure { win32_code: u32 },
    FilterDeleteFailure { win32_code: u32 },
    OwnershipConflict { key: &'static str },
    InconsistentState { details: &'static str },
}

impl fmt::Display for WindowsWfpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(f, "WFP operations are not supported on this platform")
            }
            Self::InvalidSid { sid, win32_code } => write!(
                f,
                "Invalid child SID '{sid}' (Win32 error: {win32_code} / 0x{win32_code:08X})"
            ),
            Self::EngineUnavailable { win32_code } => write!(
                f,
                "BFE filtering engine is unavailable (error: {win32_code} / 0x{win32_code:08X})"
            ),
            Self::AccessDenied { win32_code } => write!(
                f,
                "Access denied opening BFE engine (error: {win32_code} / 0x{win32_code:08X})"
            ),
            Self::TransactionBeginFailure { win32_code } => write!(
                f,
                "Failed to begin BFE transaction (error: {win32_code} / 0x{win32_code:08X})"
            ),
            Self::TransactionCommitFailure { win32_code } => write!(
                f,
                "Failed to commit BFE transaction (error: {win32_code} / 0x{win32_code:08X})"
            ),
            Self::TransactionAbortFailure { win32_code } => write!(
                f,
                "Failed to abort BFE transaction (error: {win32_code} / 0x{win32_code:08X})"
            ),
            Self::ProviderQueryFailure { win32_code } => write!(
                f,
                "Failed to query WFP provider (error: {win32_code} / 0x{win32_code:08X})"
            ),
            Self::SublayerQueryFailure { win32_code } => write!(
                f,
                "Failed to query WFP sublayer (error: {win32_code} / 0x{win32_code:08X})"
            ),
            Self::FilterQueryFailure { win32_code } => write!(
                f,
                "Failed to query WFP filter (error: {win32_code} / 0x{win32_code:08X})"
            ),
            Self::FilterEnumerationFailure { win32_code } => write!(
                f,
                "Failed to enumerate WFP filters (error: {win32_code} / 0x{win32_code:08X})"
            ),
            Self::ProviderMutationFailure { win32_code } => write!(
                f,
                "Failed to mutate WFP provider (error: {win32_code} / 0x{win32_code:08X})"
            ),
            Self::SublayerMutationFailure { win32_code } => write!(
                f,
                "Failed to mutate WFP sublayer (error: {win32_code} / 0x{win32_code:08X})"
            ),
            Self::FilterAddFailure { win32_code } => write!(
                f,
                "Failed to add WFP filter (error: {win32_code} / 0x{win32_code:08X})"
            ),
            Self::FilterDeleteFailure { win32_code } => write!(
                f,
                "Failed to delete WFP filter (error: {win32_code} / 0x{win32_code:08X})"
            ),
            Self::OwnershipConflict { key } => {
                write!(f, "Ownership conflict on WFP object key: {key}")
            }
            Self::InconsistentState { details } => {
                write!(f, "Inconsistent WFP platform state: {details}")
            }
        }
    }
}

impl std::error::Error for WindowsWfpError {}

/// Pure deterministic helper calculating retry backoff duration according to docs/018 contract.
///
/// Sequence:
/// - attempt_count == 0 -> 1s
/// - attempt_count == 1 -> 1s
/// - attempt_count == 2 -> 2s
/// - attempt_count == 3 -> 4s
/// - attempt_count == 4 -> 8s
/// - attempt_count == 5 -> 16s
/// - attempt_count == 6 -> 32s
/// - attempt_count >= 7 -> 60s
pub fn canonical_retry_delay(attempt_count: u32) -> Duration {
    const MAXIMUM_DELAY_SECONDS: u64 = 60;
    if attempt_count <= 1 {
        return Duration::from_secs(1);
    }
    let exponent = attempt_count - 1;
    let seconds = if exponent >= 6 {
        MAXIMUM_DELAY_SECONDS
    } else {
        1u64.checked_shl(exponent)
            .unwrap_or(MAXIMUM_DELAY_SECONDS)
            .min(MAXIMUM_DELAY_SECONDS)
    };
    Duration::from_secs(seconds)
}

/// Canonical provider specification generator.
fn canonical_provider_spec() -> WfpProviderSpec {
    WfpProviderSpec {
        provider_key: PALKA_WFP_PROVIDER_KEY,
        display_name: PALKA_WFP_PROVIDER_NAME.to_string(),
        description: PALKA_WFP_PROVIDER_DESC.to_string(),
        service_name: Some(PALKA_WFP_SERVICE_NAME.to_string()),
        provider_data: PALKA_WFP_PROVIDER_DATA.to_vec(),
        persistent: true,
    }
}

/// Canonical sublayer specification generator.
fn canonical_sublayer_spec() -> WfpSubLayerSpec {
    WfpSubLayerSpec {
        sublayer_key: PALKA_WFP_SUBLAYER_KEY,
        display_name: PALKA_WFP_SUBLAYER_NAME.to_string(),
        description: PALKA_WFP_SUBLAYER_DESC.to_string(),
        provider_key: PALKA_WFP_PROVIDER_KEY,
        weight: PALKA_WFP_SUBLAYER_WEIGHT,
        persistent: true,
    }
}

/// Metadata describing one of the 4 canonical filters.
pub(crate) struct CanonicalFilterMeta {
    key: GUID,
    layer: GUID,
    name: &'static str,
    desc: &'static str,
}

const CANONICAL_FILTERS: [CanonicalFilterMeta; 4] = [
    CanonicalFilterMeta {
        key: FILTER_ALE_AUTH_CONNECT_V4_KEY,
        layer: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        name: "PALKA Block Outbound Connect IPv4",
        desc: "Blocks outbound IPv4 connections for configured Child SID",
    },
    CanonicalFilterMeta {
        key: FILTER_ALE_AUTH_CONNECT_V6_KEY,
        layer: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        name: "PALKA Block Outbound Connect IPv6",
        desc: "Blocks outbound IPv6 connections for configured Child SID",
    },
    CanonicalFilterMeta {
        key: FILTER_ALE_AUTH_RECV_ACCEPT_V4_KEY,
        layer: FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4,
        name: "PALKA Block Inbound Recv-Accept IPv4",
        desc: "Blocks inbound IPv4 connections for configured Child SID",
    },
    CanonicalFilterMeta {
        key: FILTER_ALE_AUTH_RECV_ACCEPT_V6_KEY,
        layer: FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6,
        name: "PALKA Block Inbound Recv-Accept IPv6",
        desc: "Blocks inbound IPv6 connections for configured Child SID",
    },
];

fn canonical_filter_spec(meta: &CanonicalFilterMeta, child_sid: &str) -> WfpFilterSpec {
    WfpFilterSpec {
        filter_key: meta.key,
        display_name: meta.name.to_string(),
        description: meta.desc.to_string(),
        layer_key: meta.layer,
        sublayer_key: PALKA_WFP_SUBLAYER_KEY,
        provider_key: Some(PALKA_WFP_PROVIDER_KEY),
        weight: PALKA_WFP_FILTER_WEIGHT,
        action_type: WfpActionType::Block,
        clear_action_right: false,
        persistent: true,
        sid_condition: Some(child_sid.to_string()),
    }
}

/// Pure SID string basic format validation (S-1-...).
fn validate_sid_string_format(sid: &str) -> Result<&str, WindowsWfpError> {
    let clean = sid.trim();
    if clean.is_empty() || !clean.starts_with("S-") || clean.len() < 5 {
        return Err(WindowsWfpError::InvalidSid {
            sid: sid.to_string(),
            win32_code: 87, // ERROR_INVALID_PARAMETER
        });
    }
    // Verify components after S- are digits and dashes
    for ch in clean.chars() {
        if !ch.is_ascii_digit() && ch != '-' && ch != 'S' {
            return Err(WindowsWfpError::InvalidSid {
                sid: sid.to_string(),
                win32_code: 1337, // ERROR_INVALID_SID
            });
        }
    }
    Ok(clean)
}

/// Canonical provider validation helper checking all required production fields.
pub fn is_canonical_palka_provider(prov: &WfpProviderSnapshot) -> bool {
    prov.provider_key == PALKA_WFP_PROVIDER_KEY
        && prov.display_name == PALKA_WFP_PROVIDER_NAME
        && prov.description == PALKA_WFP_PROVIDER_DESC
        && prov.service_name.as_deref() == Some(PALKA_WFP_SERVICE_NAME)
        && prov.provider_data == PALKA_WFP_PROVIDER_DATA
        && prov.flags == FWPM_PROVIDER_FLAG_PERSISTENT
        && !prov.disabled
}

/// Canonical sublayer validation helper checking all required production fields.
pub fn is_canonical_palka_sublayer(sub: &WfpSubLayerSnapshot) -> bool {
    sub.sublayer_key == PALKA_WFP_SUBLAYER_KEY
        && sub.display_name == PALKA_WFP_SUBLAYER_NAME
        && sub.description == PALKA_WFP_SUBLAYER_DESC
        && sub.provider_key == PALKA_WFP_PROVIDER_KEY
        && sub.weight == PALKA_WFP_SUBLAYER_WEIGHT
        && sub.flags == FWPM_SUBLAYER_FLAG_PERSISTENT
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InfrastructureValidation {
    Absent,
    Canonical,
    ForeignCollision,
    InconsistentPalka,
}

fn classify_existing_provider(prov_opt: Option<&WfpProviderSnapshot>) -> InfrastructureValidation {
    match prov_opt {
        None => InfrastructureValidation::Absent,
        Some(prov) => {
            if prov.service_name.as_deref() != Some(PALKA_WFP_SERVICE_NAME)
                || prov.provider_data != PALKA_WFP_PROVIDER_DATA
            {
                InfrastructureValidation::ForeignCollision
            } else if !is_canonical_palka_provider(prov) {
                InfrastructureValidation::InconsistentPalka
            } else {
                InfrastructureValidation::Canonical
            }
        }
    }
}

fn classify_existing_sublayer(sub_opt: Option<&WfpSubLayerSnapshot>) -> InfrastructureValidation {
    match sub_opt {
        None => InfrastructureValidation::Absent,
        Some(sub) => {
            if sub.provider_key != PALKA_WFP_PROVIDER_KEY {
                InfrastructureValidation::ForeignCollision
            } else if !is_canonical_palka_sublayer(sub) {
                InfrastructureValidation::InconsistentPalka
            } else {
                InfrastructureValidation::Canonical
            }
        }
    }
}

/// Centralized canonical filter validator.
///
/// Validates that a given filter snapshot satisfies all canonical PALKA blocking requirements:
/// - filter key matches the canonical specification
/// - exact ALE layer
/// - PALKA sublayer
/// - PALKA provider
/// - action type Block
/// - weight 15
/// - persistent
/// - not disabled
/// - CLEAR_ACTION_RIGHT absent
/// - exactly one condition
/// - condition field key is FWPM_CONDITION_ALE_USER_ID
/// - condition match type is FWP_MATCH_EQUAL
/// - condition value type is FWP_SECURITY_DESCRIPTOR_TYPE
/// - valid security descriptor with DACL present
/// - exactly one allow ACE with mask FWP_ACTRL_MATCH_FILTER (0x1) matching expected child SID
pub(crate) fn is_canonical_palka_filter(
    meta: &CanonicalFilterMeta,
    filt: &WfpFilterSnapshot,
    expected_sid: &str,
) -> bool {
    if filt.filter_key != meta.key {
        return false;
    }
    if filt.layer_key != meta.layer {
        return false;
    }
    if filt.sublayer_key != PALKA_WFP_SUBLAYER_KEY {
        return false;
    }
    if filt.provider_key != Some(PALKA_WFP_PROVIDER_KEY) {
        return false;
    }
    if filt.action_type != WfpActionType::Block {
        return false;
    }
    if filt.weight != PALKA_WFP_FILTER_WEIGHT {
        return false;
    }
    if filt.clear_action_right {
        return false;
    }
    if filt.disabled {
        return false;
    }
    if filt.flags != FWPM_FILTER_FLAG_PERSISTENT {
        return false;
    }
    if filt.conditions.len() != 1 {
        return false;
    }
    let cond = &filt.conditions[0];
    if cond.field_key != FWPM_CONDITION_ALE_USER_ID {
        return false;
    }
    if cond.match_type != FWP_MATCH_EQUAL {
        return false;
    }
    if cond.condition_value_type != FWP_SECURITY_DESCRIPTOR_TYPE {
        return false;
    }
    if !cond.is_valid_sd || !cond.dacl_present || !cond.is_self_relative {
        return false;
    }
    if cond.aces.len() != 1 {
        return false;
    }
    let ace = &cond.aces[0];
    if !ace.is_allow {
        return false;
    }
    if ace.mask != FWP_ACTRL_MATCH_FILTER {
        return false;
    }
    if ace.sid != expected_sid {
        return false;
    }
    if ace.ace_flags != 0 {
        return false;
    }
    true
}

/// Transaction RAII guard ensuring automatic rollback if not explicitly committed.
struct TransactionGuard<'a, E: WfpEnginePort> {
    engine: &'a mut E,
    active: bool,
}

impl<'a, E: WfpEnginePort> TransactionGuard<'a, E> {
    fn begin(engine: &'a mut E) -> Result<Self, WindowsWfpError> {
        engine.transaction_begin()?;
        Ok(Self {
            engine,
            active: true,
        })
    }

    fn commit_or_cleanup(mut self) -> Result<(), WindowsWfpError> {
        self.active = false;
        match self.engine.transaction_commit() {
            Ok(()) => Ok(()),
            Err(commit_err) => match self.engine.transaction_abort() {
                Ok(()) => Err(commit_err),
                Err(abort_err) => Err(abort_err),
            },
        }
    }

    fn abort_explicitly(mut self) -> Result<(), WindowsWfpError> {
        self.active = false;
        self.engine.transaction_abort()
    }

    /// Handles an operational failure inside an active transaction by attempting explicit rollback.
    /// If rollback itself fails, TransactionAbortFailure is returned, keeping the abort failure observable.
    fn handle_error(self, op_err: WindowsWfpError) -> WindowsWfpError {
        match self.abort_explicitly() {
            Ok(()) => op_err,
            Err(abort_err) => abort_err,
        }
    }
}

impl<'a, E: WfpEnginePort> Drop for TransactionGuard<'a, E> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.engine.transaction_abort();
        }
    }
}

/// Native Windows platform adapter orchestrating WFP network restrictions.
pub struct WindowsInternetGate<E: WfpEnginePort = WindowsWfpEngine> {
    engine: Mutex<E>,
}

impl WindowsInternetGate<WindowsWfpEngine> {
    /// Opens the live Windows Filtering Platform engine session.
    pub fn open_live() -> Result<Self, WindowsWfpError> {
        let engine = WindowsWfpEngine::open()?;
        Ok(Self {
            engine: Mutex::new(engine),
        })
    }
}

impl<E: WfpEnginePort> WindowsInternetGate<E> {
    /// Creates a gate using an existing WFP engine port implementation.
    pub fn new(engine: E) -> Self {
        Self {
            engine: Mutex::new(engine),
        }
    }

    /// Inspects truthful real-time BFE state for the given child SID.
    pub fn current_state(&self, child_sid: &str) -> Result<InternetState, WindowsWfpError> {
        let valid_sid = validate_sid_string_format(child_sid)?;

        let engine = self.engine.lock().unwrap();
        engine.validate_sid(valid_sid)?;

        // 1. Enumerate all filters owned by PALKA provider in BFE:
        let prov_filters = engine.enum_filters_by_provider(&PALKA_WFP_PROVIDER_KEY)?;

        // Check for unexpected PALKA filters:
        for f in &prov_filters {
            let is_canonical = CANONICAL_FILTERS.iter().any(|c| c.key == f.filter_key);
            if !is_canonical {
                // Unexpected filter under PALKA provider key -> Unknown
                return Ok(InternetState::Unknown);
            }
        }

        // 2. Query each of the 4 canonical filters:
        let mut canonical_snapshots = Vec::with_capacity(4);
        for meta in &CANONICAL_FILTERS {
            let snap_opt = engine.get_filter(&meta.key)?;
            if let Some(snap) = snap_opt {
                canonical_snapshots.push((meta, snap));
            }
        }

        let prov_opt = engine.get_provider(&PALKA_WFP_PROVIDER_KEY)?;
        let sub_opt = engine.get_sublayer(&PALKA_WFP_SUBLAYER_KEY)?;
        let prov_status = classify_existing_provider(prov_opt.as_ref());
        let sub_status = classify_existing_sublayer(sub_opt.as_ref());

        // Check canonical count:
        let count = canonical_snapshots.len();
        if count == 0 {
            // 0 canonical filters:
            // Unrestricted is ONLY truthful if Provider and Sublayer are either
            // absent or canonical PALKA infrastructure, with no unexpected filters or collisions:
            match (prov_status, sub_status) {
                (
                    InfrastructureValidation::Absent | InfrastructureValidation::Canonical,
                    InfrastructureValidation::Absent | InfrastructureValidation::Canonical,
                ) if prov_filters.is_empty() => Ok(InternetState::Unrestricted),
                _ => Ok(InternetState::Unknown),
            }
        } else if count < 4 {
            // Partial canonical set (1, 2, or 3) => Unknown
            Ok(InternetState::Unknown)
        } else {
            // Exactly 4 canonical filters are present.
            // Provider and sublayer MUST be fully canonical for Blocked state:
            if prov_status != InfrastructureValidation::Canonical
                || sub_status != InfrastructureValidation::Canonical
            {
                return Ok(InternetState::Unknown);
            }

            for (meta, filt) in canonical_snapshots {
                if !is_canonical_palka_filter(meta, &filt, valid_sid) {
                    return Ok(InternetState::Unknown);
                }
            }

            Ok(InternetState::Blocked)
        }
    }

    /// Idempotently applies the 4 canonical persistent blocking filters for the child SID.
    pub fn block_internet(&self, child_sid: &str) -> Result<(), WindowsWfpError> {
        let valid_sid = validate_sid_string_format(child_sid)?;

        let mut engine = self.engine.lock().unwrap();
        engine.validate_sid(valid_sid)?;

        // Open explicit BFE write transaction for authoritative validation and mutation:
        let tx = TransactionGuard::begin(&mut *engine)?;

        // 1. Authoritative Provider lookup & validation inside transaction:
        let existing_prov = match tx.engine.get_provider(&PALKA_WFP_PROVIDER_KEY) {
            Ok(p) => p,
            Err(e) => return Err(tx.handle_error(e)),
        };
        let prov_status = classify_existing_provider(existing_prov.as_ref());
        match prov_status {
            InfrastructureValidation::ForeignCollision => {
                return Err(tx.handle_error(WindowsWfpError::OwnershipConflict {
                    key: "PALKA_WFP_PROVIDER_KEY",
                }));
            }
            InfrastructureValidation::InconsistentPalka => {
                return Err(tx.handle_error(WindowsWfpError::InconsistentState {
                    details: "Existing PALKA provider is in non-canonical state",
                }));
            }
            _ => {}
        }

        // 2. Authoritative Sublayer lookup & validation inside transaction:
        let existing_sub = match tx.engine.get_sublayer(&PALKA_WFP_SUBLAYER_KEY) {
            Ok(s) => s,
            Err(e) => return Err(tx.handle_error(e)),
        };
        let sub_status = classify_existing_sublayer(existing_sub.as_ref());
        match sub_status {
            InfrastructureValidation::ForeignCollision => {
                return Err(tx.handle_error(WindowsWfpError::OwnershipConflict {
                    key: "PALKA_WFP_SUBLAYER_KEY",
                }));
            }
            InfrastructureValidation::InconsistentPalka => {
                return Err(tx.handle_error(WindowsWfpError::InconsistentState {
                    details: "Existing PALKA sublayer is in non-canonical state",
                }));
            }
            _ => {}
        }

        // 3. Authoritative filter key ownership validation inside transaction:
        let mut existing_canonical_filters = Vec::new();
        for meta in &CANONICAL_FILTERS {
            match tx.engine.get_filter(&meta.key) {
                Ok(Some(f)) => {
                    if f.provider_key != Some(PALKA_WFP_PROVIDER_KEY) {
                        return Err(
                            tx.handle_error(WindowsWfpError::OwnershipConflict { key: meta.name })
                        );
                    }
                    existing_canonical_filters.push((meta, f));
                }
                Ok(None) => {}
                Err(e) => return Err(tx.handle_error(e)),
            }
        }

        // 4. Authoritative unexpected PALKA filter enumeration inside transaction:
        let prov_filters = match tx.engine.enum_filters_by_provider(&PALKA_WFP_PROVIDER_KEY) {
            Ok(f) => f,
            Err(e) => return Err(tx.handle_error(e)),
        };
        for f in &prov_filters {
            let is_canonical = CANONICAL_FILTERS.iter().any(|c| c.key == f.filter_key);
            if !is_canonical {
                return Err(tx.handle_error(WindowsWfpError::InconsistentState {
                    details: "Unexpected filter exists under PALKA provider",
                }));
            }
        }

        // 5. Fast-path canonical check inside transaction:
        let can_fast_path = prov_status == InfrastructureValidation::Canonical
            && sub_status == InfrastructureValidation::Canonical
            && existing_canonical_filters.len() == 4
            && existing_canonical_filters
                .iter()
                .all(|(meta, f)| is_canonical_palka_filter(meta, f, valid_sid));

        if can_fast_path {
            tx.commit_or_cleanup()?;
            return Ok(());
        }

        // 6. Mutation path inside the same transaction:
        if existing_prov.is_none() {
            if let Err(e) = tx.engine.add_provider(&canonical_provider_spec()) {
                return Err(tx.handle_error(e));
            }
        }

        if existing_sub.is_none() {
            if let Err(e) = tx.engine.add_sublayer(&canonical_sublayer_spec()) {
                return Err(tx.handle_error(e));
            }
        }

        // Delete any existing/damaged/stale PALKA canonical filters:
        for (meta, _) in &existing_canonical_filters {
            if let Err(e) = tx.engine.delete_filter(&meta.key) {
                return Err(tx.handle_error(e));
            }
        }

        // Add all 4 canonical filters:
        for meta in &CANONICAL_FILTERS {
            let spec = canonical_filter_spec(meta, valid_sid);
            if let Err(e) = tx.engine.add_filter(&spec) {
                return Err(tx.handle_error(e));
            }
        }

        tx.commit_or_cleanup()?;
        Ok(())
    }

    /// Idempotently removes PALKA blocking filters without deleting provider, sublayer, or foreign objects.
    pub fn unblock_internet(&self, child_sid: &str) -> Result<(), WindowsWfpError> {
        let valid_sid = validate_sid_string_format(child_sid)?;

        let mut engine = self.engine.lock().unwrap();
        engine.validate_sid(valid_sid)?;

        let tx = TransactionGuard::begin(&mut *engine)?;

        // Check for unexpected extra filters belonging to PALKA provider inside transaction:
        let prov_filters = match tx.engine.enum_filters_by_provider(&PALKA_WFP_PROVIDER_KEY) {
            Ok(f) => f,
            Err(e) => return Err(tx.handle_error(e)),
        };
        for f in &prov_filters {
            let is_canonical = CANONICAL_FILTERS.iter().any(|c| c.key == f.filter_key);
            if !is_canonical {
                return Err(tx.handle_error(WindowsWfpError::InconsistentState {
                    details: "Unexpected filter exists under PALKA provider",
                }));
            }
        }

        // Check if any canonical filter is present:
        let mut to_delete = Vec::new();
        for meta in &CANONICAL_FILTERS {
            match tx.engine.get_filter(&meta.key) {
                Ok(Some(f)) => {
                    if f.provider_key == Some(PALKA_WFP_PROVIDER_KEY) {
                        to_delete.push(meta.key);
                    }
                }
                Ok(None) => {}
                Err(e) => return Err(tx.handle_error(e)),
            }
        }

        if to_delete.is_empty() {
            tx.commit_or_cleanup()?;
            return Ok(());
        }

        for key in to_delete {
            if let Err(e) = tx.engine.delete_filter(&key) {
                return Err(tx.handle_error(e));
            }
        }

        tx.commit_or_cleanup()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wfp::*;

    const TEST_CHILD_SID: &str = "S-1-5-21-123456789-123456789-123456789-1001";
    const ALT_CHILD_SID: &str = "S-1-5-21-987654321-987654321-987654321-1002";

    #[test]
    fn test_canonical_retry_delay_sequence() {
        assert_eq!(canonical_retry_delay(0), Duration::from_secs(1));
        assert_eq!(canonical_retry_delay(1), Duration::from_secs(1));
        assert_eq!(canonical_retry_delay(2), Duration::from_secs(2));
        assert_eq!(canonical_retry_delay(3), Duration::from_secs(4));
        assert_eq!(canonical_retry_delay(4), Duration::from_secs(8));
        assert_eq!(canonical_retry_delay(5), Duration::from_secs(16));
        assert_eq!(canonical_retry_delay(6), Duration::from_secs(32));
        assert_eq!(canonical_retry_delay(7), Duration::from_secs(60));
        assert_eq!(canonical_retry_delay(8), Duration::from_secs(60));
        assert_eq!(canonical_retry_delay(100), Duration::from_secs(60));
    }

    #[test]
    fn test_initial_state_is_unrestricted() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        let state = gate.current_state(TEST_CHILD_SID).unwrap();
        assert_eq!(state, InternetState::Unrestricted);
    }

    #[test]
    fn test_block_creates_exact_canonical_set_and_reports_blocked() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        gate.block_internet(TEST_CHILD_SID).unwrap();
        let state = gate.current_state(TEST_CHILD_SID).unwrap();
        assert_eq!(state, InternetState::Blocked);

        // Verify committed objects
        let engine = gate.engine.lock().unwrap();
        assert_eq!(engine.committed_filters().len(), 4);
        assert!(
            engine
                .committed_providers()
                .contains_key(&PALKA_WFP_PROVIDER_KEY)
        );
        assert!(
            engine
                .committed_sublayers()
                .contains_key(&PALKA_WFP_SUBLAYER_KEY)
        );

        for meta in &CANONICAL_FILTERS {
            let f = engine.get_committed_filter(&meta.key).unwrap();
            assert_eq!(f.layer_key, meta.layer);
            assert_eq!(f.sublayer_key, PALKA_WFP_SUBLAYER_KEY);
            assert_eq!(f.provider_key, Some(PALKA_WFP_PROVIDER_KEY));
            assert_eq!(f.action_type, WfpActionType::Block);
            assert_eq!(f.weight, 15);
            assert!(!f.clear_action_right);
            assert_eq!(f.single_child_sid(), Some(TEST_CHILD_SID));
        }
    }

    #[test]
    fn test_block_idempotency_no_duplicates() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        gate.block_internet(TEST_CHILD_SID).unwrap();
        gate.block_internet(TEST_CHILD_SID).unwrap();
        let state = gate.current_state(TEST_CHILD_SID).unwrap();
        assert_eq!(state, InternetState::Blocked);

        let engine = gate.engine.lock().unwrap();
        assert_eq!(engine.committed_filters().len(), 4);
    }

    #[test]
    fn test_unblock_removes_canonical_filters_and_leaves_unrestricted() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        gate.block_internet(TEST_CHILD_SID).unwrap();
        gate.unblock_internet(TEST_CHILD_SID).unwrap();

        let state = gate.current_state(TEST_CHILD_SID).unwrap();
        assert_eq!(state, InternetState::Unrestricted);

        let engine = gate.engine.lock().unwrap();
        assert_eq!(engine.committed_filters().len(), 0);
        // Provider and sublayer are retained
        assert!(
            engine
                .committed_providers()
                .contains_key(&PALKA_WFP_PROVIDER_KEY)
        );
        assert!(
            engine
                .committed_sublayers()
                .contains_key(&PALKA_WFP_SUBLAYER_KEY)
        );
    }

    #[test]
    fn test_unblock_idempotency_repeated_calls() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        gate.unblock_internet(TEST_CHILD_SID).unwrap();
        gate.unblock_internet(TEST_CHILD_SID).unwrap();
        let state = gate.current_state(TEST_CHILD_SID).unwrap();
        assert_eq!(state, InternetState::Unrestricted);
    }

    #[test]
    fn test_partial_filter_set_reports_unknown() {
        let mut fake = FakeWfpEnginePort::new();
        let prov = WfpProviderSnapshot {
            provider_key: PALKA_WFP_PROVIDER_KEY,
            display_name: PALKA_WFP_PROVIDER_NAME.to_string(),
            description: PALKA_WFP_PROVIDER_DESC.to_string(),
            service_name: Some(PALKA_WFP_SERVICE_NAME.to_string()),
            provider_data: PALKA_WFP_PROVIDER_DATA.to_vec(),
            flags: 1,
            disabled: false,
        };
        let sub = WfpSubLayerSnapshot {
            sublayer_key: PALKA_WFP_SUBLAYER_KEY,
            display_name: PALKA_WFP_SUBLAYER_NAME.to_string(),
            description: PALKA_WFP_SUBLAYER_DESC.to_string(),
            provider_key: PALKA_WFP_PROVIDER_KEY,
            weight: PALKA_WFP_SUBLAYER_WEIGHT,
            flags: 1,
        };
        fake.insert_raw_provider(prov);
        fake.insert_raw_sublayer(sub);

        // Insert only 2 of 4 filters
        for meta in &CANONICAL_FILTERS[0..2] {
            let f = WfpFilterSnapshot {
                filter_key: meta.key,
                display_name: meta.name.to_string(),
                description: meta.desc.to_string(),
                layer_key: meta.layer,
                sublayer_key: PALKA_WFP_SUBLAYER_KEY,
                provider_key: Some(PALKA_WFP_PROVIDER_KEY),
                weight: 15,
                action_type: WfpActionType::Block,
                flags: 1,
                disabled: false,
                clear_action_right: false,
                conditions: vec![WfpConditionSnapshot::canonical_child_sid(TEST_CHILD_SID)],
            };
            fake.insert_raw_filter(f);
        }

        let gate = WindowsInternetGate::new(fake);
        let state = gate.current_state(TEST_CHILD_SID).unwrap();
        assert_eq!(state, InternetState::Unknown);

        // block_internet repairs partial set to full 4
        gate.block_internet(TEST_CHILD_SID).unwrap();
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Blocked
        );
        let engine = gate.engine.lock().unwrap();
        assert_eq!(engine.committed_filters().len(), 4);
    }

    #[test]
    fn test_wrong_sid_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        gate.block_internet(TEST_CHILD_SID).unwrap();

        // Querying with another SID returns Unknown
        let state = gate.current_state(ALT_CHILD_SID).unwrap();
        assert_eq!(state, InternetState::Unknown);

        // block_internet with new SID updates filters
        gate.block_internet(ALT_CHILD_SID).unwrap();
        assert_eq!(
            gate.current_state(ALT_CHILD_SID).unwrap(),
            InternetState::Blocked
        );
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn test_disabled_filter_reports_unknown() {
        let mut fake = FakeWfpEnginePort::new();
        let prov = WfpProviderSnapshot {
            provider_key: PALKA_WFP_PROVIDER_KEY,
            display_name: PALKA_WFP_PROVIDER_NAME.to_string(),
            description: PALKA_WFP_PROVIDER_DESC.to_string(),
            service_name: Some(PALKA_WFP_SERVICE_NAME.to_string()),
            provider_data: PALKA_WFP_PROVIDER_DATA.to_vec(),
            flags: 1,
            disabled: false,
        };
        fake.insert_raw_provider(prov);

        for (i, meta) in CANONICAL_FILTERS.iter().enumerate() {
            let f = WfpFilterSnapshot {
                filter_key: meta.key,
                display_name: meta.name.to_string(),
                description: meta.desc.to_string(),
                layer_key: meta.layer,
                sublayer_key: PALKA_WFP_SUBLAYER_KEY,
                provider_key: Some(PALKA_WFP_PROVIDER_KEY),
                weight: 15,
                action_type: WfpActionType::Block,
                flags: 1,
                disabled: i == 0, // First filter is disabled
                clear_action_right: false,
                conditions: vec![WfpConditionSnapshot::canonical_child_sid(TEST_CHILD_SID)],
            };
            fake.insert_raw_filter(f);
        }

        let gate = WindowsInternetGate::new(fake);
        let state = gate.current_state(TEST_CHILD_SID).unwrap();
        assert_eq!(state, InternetState::Unknown);
    }

    #[test]
    fn test_clear_action_right_present_reports_unknown() {
        let mut fake = FakeWfpEnginePort::new();
        for (i, meta) in CANONICAL_FILTERS.iter().enumerate() {
            let f = WfpFilterSnapshot {
                filter_key: meta.key,
                display_name: meta.name.to_string(),
                description: meta.desc.to_string(),
                layer_key: meta.layer,
                sublayer_key: PALKA_WFP_SUBLAYER_KEY,
                provider_key: Some(PALKA_WFP_PROVIDER_KEY),
                weight: 15,
                action_type: WfpActionType::Block,
                flags: 1,
                disabled: false,
                clear_action_right: i == 1, // Forbidden flag present
                conditions: vec![WfpConditionSnapshot::canonical_child_sid(TEST_CHILD_SID)],
            };
            fake.insert_raw_filter(f);
        }
        let gate = WindowsInternetGate::new(fake);
        let state = gate.current_state(TEST_CHILD_SID).unwrap();
        assert_eq!(state, InternetState::Unknown);
    }

    #[test]
    fn test_unexpected_filter_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        gate.block_internet(TEST_CHILD_SID).unwrap();

        // Inject unexpected extra filter under PALKA provider
        let foreign_key = GUID::from_u128(0x11112222_3333_4444_5555_666677778888);
        let extra = WfpFilterSnapshot {
            filter_key: foreign_key,
            display_name: "Sneaky Extra".to_string(),
            description: "Desc".to_string(),
            layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
            sublayer_key: PALKA_WFP_SUBLAYER_KEY,
            provider_key: Some(PALKA_WFP_PROVIDER_KEY),
            weight: 15,
            action_type: WfpActionType::Block,
            flags: 1,
            disabled: false,
            clear_action_right: false,
            conditions: vec![WfpConditionSnapshot::canonical_child_sid(TEST_CHILD_SID)],
        };
        gate.engine.lock().unwrap().insert_raw_filter(extra);

        let state = gate.current_state(TEST_CHILD_SID).unwrap();
        assert_eq!(state, InternetState::Unknown);

        // block_internet detects inconsistent extra filter and fails
        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        assert!(matches!(err, WindowsWfpError::InconsistentState { .. }));
    }

    #[test]
    fn test_foreign_object_collision_fails_ownership_conflict() {
        let mut fake = FakeWfpEnginePort::new();
        let foreign_prov = WfpProviderSnapshot {
            provider_key: PALKA_WFP_PROVIDER_KEY,
            display_name: "Foreign Antivirus".to_string(),
            description: "Alien".to_string(),
            service_name: Some("AlienAV".to_string()),
            provider_data: b"ALIEN-DATA".to_vec(),
            flags: 1,
            disabled: false,
        };
        fake.insert_raw_provider(foreign_prov);

        let gate = WindowsInternetGate::new(fake);
        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        assert!(matches!(err, WindowsWfpError::OwnershipConflict { .. }));

        // Ensure foreign provider was NOT deleted or touched
        let engine = gate.engine.lock().unwrap();
        assert_eq!(
            engine
                .committed_providers()
                .get(&PALKA_WFP_PROVIDER_KEY)
                .unwrap()
                .service_name
                .as_deref(),
            Some("AlienAV")
        );
    }

    #[test]
    fn test_foreign_filter_never_deleted_on_unblock() {
        let mut fake = FakeWfpEnginePort::new();
        let foreign_key = FILTER_ALE_AUTH_CONNECT_V4_KEY;
        let foreign_provider_key = GUID::from_u128(0x99999999_8888_7777_6666_555544443333);
        let foreign_filter = WfpFilterSnapshot {
            filter_key: foreign_key,
            display_name: "Third Party Filter".to_string(),
            description: "Alien Filter".to_string(),
            layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
            sublayer_key: PALKA_WFP_SUBLAYER_KEY,
            provider_key: Some(foreign_provider_key),
            weight: 10,
            action_type: WfpActionType::Permit,
            flags: 1,
            disabled: false,
            clear_action_right: false,
            conditions: Vec::new(),
        };
        fake.insert_raw_filter(foreign_filter);

        let gate = WindowsInternetGate::new(fake);
        gate.unblock_internet(TEST_CHILD_SID).unwrap();

        // Foreign filter must still exist!
        let engine = gate.engine.lock().unwrap();
        assert!(engine.committed_filters().contains_key(&foreign_key));
    }

    #[test]
    fn test_simulated_add_failure_aborts_transaction_cleanly() {
        let mut fake = FakeWfpEnginePort::new();
        // Fail on filter add with FWP_E_ALREADY_EXISTS (0x80320009)
        fake.fail_filter_add = Some(0x80320009);

        let gate = WindowsInternetGate::new(fake);
        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        assert_eq!(
            err,
            WindowsWfpError::FilterAddFailure {
                win32_code: 0x80320009
            }
        );

        // Verify transaction was aborted: no partial filters or objects committed!
        let engine = gate.engine.lock().unwrap();
        assert_eq!(engine.committed_filters().len(), 0);
        assert!(!engine.is_in_transaction());
    }

    #[test]
    fn test_simulated_delete_failure_aborts_transaction() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        gate.block_internet(TEST_CHILD_SID).unwrap();

        // Fail delete during unblock with FWP_E_FILTER_NOT_FOUND (0x80320003)
        gate.engine.lock().unwrap().fail_filter_delete = Some(0x80320003);
        let err = gate.unblock_internet(TEST_CHILD_SID).unwrap_err();
        assert_eq!(
            err,
            WindowsWfpError::FilterDeleteFailure {
                win32_code: 0x80320003
            }
        );

        // Filters remain intact due to rollback
        let engine = gate.engine.lock().unwrap();
        assert_eq!(engine.committed_filters().len(), 4);
        assert!(!engine.is_in_transaction());
    }

    #[test]
    fn test_mutation_failure_plus_abort_failure_remains_observable() {
        let mut fake = FakeWfpEnginePort::new();
        // Fail filter add
        fake.fail_filter_add = Some(0x80320009);
        // AND fail transaction abort
        fake.fail_transaction_abort = Some(0x8032000F); // FWP_E_TXN_ABORTED

        let gate = WindowsInternetGate::new(fake);
        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        // Transaction abort failure MUST remain observable and not masked!
        assert_eq!(
            err,
            WindowsWfpError::TransactionAbortFailure {
                win32_code: 0x8032000F
            }
        );
    }

    #[test]
    fn test_clean_machine_first_block_path() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        // Clean machine: no provider, no sublayer, no filters
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unrestricted
        );

        gate.block_internet(TEST_CHILD_SID).unwrap();
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Blocked
        );

        let engine = gate.engine.lock().unwrap();
        assert!(
            engine
                .committed_providers()
                .contains_key(&PALKA_WFP_PROVIDER_KEY)
        );
        assert!(
            engine
                .committed_sublayers()
                .contains_key(&PALKA_WFP_SUBLAYER_KEY)
        );
        assert_eq!(engine.committed_filters().len(), 4);
    }

    #[test]
    fn test_nonpersistent_provider_prevents_blocked() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        // Mutate provider to non-persistent
        let mut prov = gate
            .engine
            .lock()
            .unwrap()
            .committed_providers()
            .get(&PALKA_WFP_PROVIDER_KEY)
            .unwrap()
            .clone();
        prov.flags = 0; // Not persistent!
        gate.engine.lock().unwrap().insert_raw_provider(prov);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn test_nonpersistent_sublayer_prevents_blocked() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        // Mutate sublayer to non-persistent
        let mut sub = gate
            .engine
            .lock()
            .unwrap()
            .committed_sublayers()
            .get(&PALKA_WFP_SUBLAYER_KEY)
            .unwrap()
            .clone();
        sub.flags = 0; // Not persistent!
        gate.engine.lock().unwrap().insert_raw_sublayer(sub);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn test_wrong_sublayer_weight_prevents_fast_path() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        // Corrupt sublayer weight
        let mut sub = gate
            .engine
            .lock()
            .unwrap()
            .committed_sublayers()
            .get(&PALKA_WFP_SUBLAYER_KEY)
            .unwrap()
            .clone();
        sub.weight = 0x4000;
        gate.engine.lock().unwrap().insert_raw_sublayer(sub);

        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        assert!(matches!(err, WindowsWfpError::InconsistentState { .. }));
    }

    #[test]
    fn test_block_internet_does_not_return_ok_for_invalid_infrastructure() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        // Disable provider
        let mut prov = gate
            .engine
            .lock()
            .unwrap()
            .committed_providers()
            .get(&PALKA_WFP_PROVIDER_KEY)
            .unwrap()
            .clone();
        prov.disabled = true;
        gate.engine.lock().unwrap().insert_raw_provider(prov);

        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        assert!(matches!(err, WindowsWfpError::InconsistentState { .. }));
    }

    #[test]
    fn test_zero_filter_current_state_inconsistent_infrastructure_reports_unknown() {
        let mut fake = FakeWfpEnginePort::new();
        // Insert disabled provider with zero filters
        let prov = WfpProviderSnapshot {
            provider_key: PALKA_WFP_PROVIDER_KEY,
            display_name: PALKA_WFP_PROVIDER_NAME.to_string(),
            description: PALKA_WFP_PROVIDER_DESC.to_string(),
            service_name: Some(PALKA_WFP_SERVICE_NAME.to_string()),
            provider_data: PALKA_WFP_PROVIDER_DATA.to_vec(),
            flags: 1,
            disabled: true, // Inconsistent!
        };
        fake.insert_raw_provider(prov);

        let gate = WindowsInternetGate::new(fake);
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn test_unblock_invalid_sid_rejected() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        let err = gate.unblock_internet("").unwrap_err();
        assert!(matches!(err, WindowsWfpError::InvalidSid { .. }));

        let err2 = gate.unblock_internet("invalid_user_id").unwrap_err();
        assert!(matches!(err2, WindowsWfpError::InvalidSid { .. }));

        let err3 = gate.unblock_internet("S-bad!format").unwrap_err();
        assert!(matches!(err3, WindowsWfpError::InvalidSid { .. }));
    }

    #[test]
    fn test_valid_different_sid_still_removes_stale_canonical_filters() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        gate.block_internet(TEST_CHILD_SID).unwrap();
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Blocked
        );

        // Valid different child SID cleanly unblocks stale filters
        gate.unblock_internet(ALT_CHILD_SID).unwrap();
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unrestricted
        );
        assert_eq!(
            gate.current_state(ALT_CHILD_SID).unwrap(),
            InternetState::Unrestricted
        );
    }

    #[test]
    fn test_transaction_begin_failure_classified() {
        let mut fake = FakeWfpEnginePort::new();
        fake.fail_transaction_begin = Some(0x8032000E); // FWP_E_TXN_IN_PROGRESS

        let gate = WindowsInternetGate::new(fake);
        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        assert_eq!(
            err,
            WindowsWfpError::TransactionBeginFailure {
                win32_code: 0x8032000E
            }
        );
    }

    #[test]
    fn test_transaction_commit_failure_classified() {
        let mut fake = FakeWfpEnginePort::new();
        fake.fail_transaction_commit = Some(0x80320010); // FWP_E_SESSION_ABORTED

        let gate = WindowsInternetGate::new(fake);
        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        assert_eq!(
            err,
            WindowsWfpError::TransactionCommitFailure {
                win32_code: 0x80320010
            }
        );
    }

    #[test]
    fn test_invalid_sid_syntax_rejected() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        let err = gate.block_internet("").unwrap_err();
        assert!(matches!(err, WindowsWfpError::InvalidSid { .. }));

        let err2 = gate.block_internet("invalid_user_id").unwrap_err();
        assert!(matches!(err2, WindowsWfpError::InvalidSid { .. }));

        let err3 = gate.current_state("S-bad!syntax").unwrap_err();
        assert!(matches!(err3, WindowsWfpError::InvalidSid { .. }));
    }

    #[test]
    fn test_query_failure_propagates_error() {
        let mut fake = FakeWfpEnginePort::new();
        fake.fail_filter_query = Some(1722); // RPC_S_SERVER_UNAVAILABLE

        let gate = WindowsInternetGate::new(fake);
        let err = gate.current_state(TEST_CHILD_SID).unwrap_err();
        assert_eq!(
            err,
            WindowsWfpError::FilterQueryFailure { win32_code: 1722 }
        );
    }

    #[test]
    fn test_one_or_three_filters_reports_unknown() {
        let mut fake = FakeWfpEnginePort::new();
        let prov = WfpProviderSnapshot {
            provider_key: PALKA_WFP_PROVIDER_KEY,
            display_name: PALKA_WFP_PROVIDER_NAME.to_string(),
            description: PALKA_WFP_PROVIDER_DESC.to_string(),
            service_name: Some(PALKA_WFP_SERVICE_NAME.to_string()),
            provider_data: PALKA_WFP_PROVIDER_DATA.to_vec(),
            flags: 1,
            disabled: false,
        };
        let sub = WfpSubLayerSnapshot {
            sublayer_key: PALKA_WFP_SUBLAYER_KEY,
            display_name: PALKA_WFP_SUBLAYER_NAME.to_string(),
            description: PALKA_WFP_SUBLAYER_DESC.to_string(),
            provider_key: PALKA_WFP_PROVIDER_KEY,
            weight: PALKA_WFP_SUBLAYER_WEIGHT,
            flags: 1,
        };
        fake.insert_raw_provider(prov);
        fake.insert_raw_sublayer(sub);

        // Exactly 1 filter:
        let f1 = WfpFilterSnapshot {
            filter_key: CANONICAL_FILTERS[0].key,
            display_name: CANONICAL_FILTERS[0].name.to_string(),
            description: CANONICAL_FILTERS[0].desc.to_string(),
            layer_key: CANONICAL_FILTERS[0].layer,
            sublayer_key: PALKA_WFP_SUBLAYER_KEY,
            provider_key: Some(PALKA_WFP_PROVIDER_KEY),
            weight: 15,
            action_type: WfpActionType::Block,
            flags: 1,
            disabled: false,
            clear_action_right: false,
            conditions: vec![WfpConditionSnapshot::canonical_child_sid(TEST_CHILD_SID)],
        };
        fake.insert_raw_filter(f1);

        let gate = WindowsInternetGate::new(fake.clone());
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );

        // Exactly 3 filters:
        for meta in &CANONICAL_FILTERS[1..3] {
            let f = WfpFilterSnapshot {
                filter_key: meta.key,
                display_name: meta.name.to_string(),
                description: meta.desc.to_string(),
                layer_key: meta.layer,
                sublayer_key: PALKA_WFP_SUBLAYER_KEY,
                provider_key: Some(PALKA_WFP_PROVIDER_KEY),
                weight: 15,
                action_type: WfpActionType::Block,
                flags: 1,
                disabled: false,
                clear_action_right: false,
                conditions: vec![WfpConditionSnapshot::canonical_child_sid(TEST_CHILD_SID)],
            };
            fake.insert_raw_filter(f);
        }

        let gate3 = WindowsInternetGate::new(fake);
        assert_eq!(
            gate3.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn test_disabled_provider_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        // Mark provider as disabled:
        let mut prov = gate
            .engine
            .lock()
            .unwrap()
            .committed_providers()
            .get(&PALKA_WFP_PROVIDER_KEY)
            .unwrap()
            .clone();
        prov.disabled = true;
        gate.engine.lock().unwrap().insert_raw_provider(prov);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn test_wrong_filter_weight_layer_action_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        // Wrong weight:
        let mut f_wrong_weight = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        f_wrong_weight.weight = 10;
        gate.engine
            .lock()
            .unwrap()
            .insert_raw_filter(f_wrong_weight);
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );

        // Restore and test wrong layer:
        gate.block_internet(TEST_CHILD_SID).unwrap();
        let mut f_wrong_layer = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        f_wrong_layer.layer_key = GUID::from_u128(0x12345678_1234_1234_1234_123456789abc);
        gate.engine.lock().unwrap().insert_raw_filter(f_wrong_layer);
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );

        // Restore and test wrong action:
        gate.block_internet(TEST_CHILD_SID).unwrap();
        let mut f_wrong_action = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        f_wrong_action.action_type = WfpActionType::Permit;
        gate.engine
            .lock()
            .unwrap()
            .insert_raw_filter(f_wrong_action);
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn test_stale_sid_cleanup_on_unblock() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        // Block with old SID
        gate.block_internet(ALT_CHILD_SID).unwrap();
        assert_eq!(
            gate.current_state(ALT_CHILD_SID).unwrap(),
            InternetState::Blocked
        );

        // Unblock with child_sid removes canonical filters even if SID differs
        gate.unblock_internet(TEST_CHILD_SID).unwrap();
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unrestricted
        );
        assert_eq!(
            gate.current_state(ALT_CHILD_SID).unwrap(),
            InternetState::Unrestricted
        );
    }

    #[test]
    fn test_transaction_abort_failure_classified() {
        let mut fake = FakeWfpEnginePort::new();
        // Fail abort
        fake.fail_transaction_abort = Some(0x8032000B);

        // Begin tx
        fake.transaction_begin().unwrap();
        let err = fake.transaction_abort().unwrap_err();
        assert_eq!(
            err,
            WindowsWfpError::TransactionAbortFailure {
                win32_code: 0x8032000B
            }
        );
    }

    // --- Condition Integrity Regression Tests (Section 10) ---

    #[test]
    fn filter_with_extra_condition_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        f.conditions.push(crate::wfp::WfpConditionSnapshot {
            field_key: GUID::from_u128(0x11111111_2222_3333_4444_555566667777),
            match_type: FWP_MATCH_EQUAL,
            condition_value_type: 0,
            is_valid_sd: true,
            dacl_present: true,
            is_self_relative: true,
            aces: vec![],
        });
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn filter_with_wrong_match_type_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        f.conditions[0].match_type = 1; // Not FWP_MATCH_EQUAL
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn filter_with_wrong_condition_type_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        f.conditions[0].condition_value_type = 0; // Not FWP_SECURITY_DESCRIPTOR_TYPE
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn filter_with_missing_sid_condition_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        f.conditions.clear();
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn filter_with_wrong_sid_ace_mask_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        f.conditions[0].aces[0].mask = 0x0000_0002; // Wrong mask (must be 0x1)
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn filter_with_additional_sid_ace_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        f.conditions[0].aces.push(crate::wfp::WfpSidAceSnapshot {
            sid: ALT_CHILD_SID.to_string(),
            mask: FWP_ACTRL_MATCH_FILTER,
            is_allow: true,
            ace_flags: 0,
        });
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn block_repairs_owned_filter_with_noncanonical_condition() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        // Corrupt condition on filter 0
        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        f.conditions.push(crate::wfp::WfpConditionSnapshot {
            field_key: GUID::from_u128(0x99999999_9999_9999_9999_999999999999),
            match_type: FWP_MATCH_EQUAL,
            condition_value_type: 0,
            is_valid_sd: true,
            dacl_present: true,
            is_self_relative: true,
            aces: vec![],
        });
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );

        // Block must repair the filter transactionally
        gate.block_internet(TEST_CHILD_SID).unwrap();

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Blocked
        );

        let repaired = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        assert_eq!(repaired.conditions.len(), 1);
        assert_eq!(repaired.conditions[0].aces.len(), 1);
        assert_eq!(repaired.conditions[0].aces[0].sid, TEST_CHILD_SID);
        assert_eq!(repaired.conditions[0].aces[0].mask, FWP_ACTRL_MATCH_FILTER);
    }

    #[test]
    fn fast_path_rejects_noncanonical_condition() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        // Mutate filter to have wrong mask
        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[2].key)
            .unwrap()
            .clone();
        f.conditions[0].aces[0].mask = 0x2;
        gate.engine.lock().unwrap().insert_raw_filter(f);

        // Calling block_internet must NOT take fast-path Ok(()). It must repair!
        gate.block_internet(TEST_CHILD_SID).unwrap();

        // Verify repaired condition has canonical mask:
        let checked = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[2].key)
            .unwrap()
            .clone();
        assert_eq!(checked.conditions[0].aces[0].mask, FWP_ACTRL_MATCH_FILTER);
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Blocked
        );
    }

    // --- Native SID Validation Regression Tests (Section 14) ---

    #[test]
    fn test_plausible_but_invalid_sid_rejected_before_bfe_state() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        // This SID has digits and dashes matching `S-1-5-...` but subauthority overflows u32
        let invalid_sid = "S-1-5-21-99999999999999999999";

        let cs_err = gate.current_state(invalid_sid).unwrap_err();
        match cs_err {
            WindowsWfpError::InvalidSid { sid, .. } => assert_eq!(sid, invalid_sid),
            other => panic!("expected InvalidSid, got {:?}", other),
        }

        let bl_err = gate.block_internet(invalid_sid).unwrap_err();
        match bl_err {
            WindowsWfpError::InvalidSid { sid, .. } => assert_eq!(sid, invalid_sid),
            other => panic!("expected InvalidSid, got {:?}", other),
        }

        let ub_err = gate.unblock_internet(invalid_sid).unwrap_err();
        match ub_err {
            WindowsWfpError::InvalidSid { sid, .. } => assert_eq!(sid, invalid_sid),
            other => panic!("expected InvalidSid, got {:?}", other),
        }

        // Prove no WFP mutation occurred
        assert!(gate.engine.lock().unwrap().committed_filters().is_empty());
        assert!(gate.engine.lock().unwrap().committed_providers().is_empty());
        assert!(gate.engine.lock().unwrap().committed_sublayers().is_empty());
    }

    // --- Enumeration Regression Tests (Section 16) ---

    #[test]
    fn unexpected_disabled_filter_under_palka_provider_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let foreign_key = GUID::from_u128(0xdeadbeef_0001_0002_0003_000000000001);
        let mut disabled_filt = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        disabled_filt.filter_key = foreign_key;
        disabled_filt.disabled = true;
        gate.engine.lock().unwrap().insert_raw_filter(disabled_filt);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn unexpected_disabled_filter_under_palka_provider_blocks_mutation() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        let foreign_key = GUID::from_u128(0xdeadbeef_0001_0002_0003_000000000002);
        gate.engine
            .lock()
            .unwrap()
            .insert_raw_filter(WfpFilterSnapshot {
                filter_key: foreign_key,
                display_name: "unexpected".into(),
                description: "unexpected".into(),
                layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
                sublayer_key: PALKA_WFP_SUBLAYER_KEY,
                provider_key: Some(PALKA_WFP_PROVIDER_KEY),
                weight: 15,
                action_type: WfpActionType::Block,
                flags: 0,
                disabled: true,
                clear_action_right: false,
                conditions: vec![],
            });

        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        match err {
            WindowsWfpError::InconsistentState { details } => {
                assert!(details.contains("Unexpected filter exists under PALKA provider"));
            }
            other => panic!("expected InconsistentState, got {:?}", other),
        }
    }

    #[test]
    fn unexpected_boot_time_filter_under_palka_provider_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let foreign_key = GUID::from_u128(0xdeadbeef_0001_0002_0003_000000000003);
        let mut boottime_filt = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        boottime_filt.filter_key = foreign_key;
        boottime_filt.flags = crate::wfp::FWPM_FILTER_FLAG_BOOTTIME;
        gate.engine.lock().unwrap().insert_raw_filter(boottime_filt);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        match err {
            WindowsWfpError::InconsistentState { .. } => {}
            other => panic!("expected InconsistentState, got {:?}", other),
        }
    }

    // --- Extra Flag & Exact Canonical Flag Tests (Sections 6 & 7) ---

    #[test]
    fn canonical_key_boottime_filter_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        f.flags = crate::wfp::FWPM_FILTER_FLAG_PERSISTENT | crate::wfp::FWPM_FILTER_FLAG_BOOTTIME;
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn canonical_key_filter_with_provider_context_flag_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[1].key)
            .unwrap()
            .clone();
        f.flags = crate::wfp::FWPM_FILTER_FLAG_PERSISTENT
            | crate::wfp::FWPM_FILTER_FLAG_HAS_PROVIDER_CONTEXT;
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn canonical_key_filter_with_indexed_flag_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[2].key)
            .unwrap()
            .clone();
        f.flags = crate::wfp::FWPM_FILTER_FLAG_PERSISTENT | crate::wfp::FWPM_FILTER_FLAG_INDEXED;
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn canonical_key_filter_with_extra_flag_rejected_by_fast_path() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        // Mutate filter 3 to have extra flag
        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[3].key)
            .unwrap()
            .clone();
        f.flags = crate::wfp::FWPM_FILTER_FLAG_PERSISTENT | crate::wfp::FWPM_FILTER_FLAG_BOOTTIME;
        gate.engine.lock().unwrap().insert_raw_filter(f);

        // block_internet must NOT accept the fast-path Ok(()). It must repair the filter!
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let repaired = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[3].key)
            .unwrap()
            .clone();
        assert_eq!(repaired.flags, crate::wfp::FWPM_FILTER_FLAG_PERSISTENT);
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Blocked
        );
    }

    #[test]
    fn block_repairs_owned_filter_with_noncanonical_flags() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        f.flags = crate::wfp::FWPM_FILTER_FLAG_PERSISTENT
            | crate::wfp::FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT;
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );

        gate.block_internet(TEST_CHILD_SID).unwrap();

        let repaired = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        assert_eq!(repaired.flags, crate::wfp::FWPM_FILTER_FLAG_PERSISTENT);
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Blocked
        );
    }

    #[test]
    fn provider_with_noncanonical_flags_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut prov = gate
            .engine
            .lock()
            .unwrap()
            .committed_providers()
            .get(&PALKA_WFP_PROVIDER_KEY)
            .unwrap()
            .clone();
        prov.flags = crate::wfp::FWPM_PROVIDER_FLAG_PERSISTENT | 0x80;
        gate.engine.lock().unwrap().insert_raw_provider(prov);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn sublayer_with_noncanonical_flags_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut sub = gate
            .engine
            .lock()
            .unwrap()
            .committed_sublayers()
            .get(&PALKA_WFP_SUBLAYER_KEY)
            .unwrap()
            .clone();
        sub.flags = crate::wfp::FWPM_SUBLAYER_FLAG_PERSISTENT | 0x80;
        gate.engine.lock().unwrap().insert_raw_sublayer(sub);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    // --- Self-Relative Security Descriptor Tests (Sections 8, 9, 10) ---

    #[test]
    fn non_self_relative_condition_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        f.conditions[0].is_self_relative = false;
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn non_self_relative_condition_rejected_by_fast_path() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[1].key)
            .unwrap()
            .clone();
        f.conditions[0].is_self_relative = false;
        gate.engine.lock().unwrap().insert_raw_filter(f);

        // block_internet must repair
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let repaired = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[1].key)
            .unwrap()
            .clone();
        assert!(repaired.conditions[0].is_self_relative);
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Blocked
        );
    }

    #[test]
    fn block_repairs_owned_non_self_relative_condition() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[2].key)
            .unwrap()
            .clone();
        f.conditions[0].is_self_relative = false;
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );

        gate.block_internet(TEST_CHILD_SID).unwrap();

        let repaired = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[2].key)
            .unwrap()
            .clone();
        assert!(repaired.conditions[0].is_self_relative);
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Blocked
        );
    }

    // --- Canonical Child SID ACE Flags Regression Tests (Section 6) ---

    #[test]
    fn canonical_child_sid_ace_has_zero_flags() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        for meta in &CANONICAL_FILTERS {
            let f = gate
                .engine
                .lock()
                .unwrap()
                .get_committed_filter(&meta.key)
                .unwrap()
                .clone();
            assert_eq!(f.conditions.len(), 1);
            assert_eq!(f.conditions[0].aces.len(), 1);
            assert_eq!(f.conditions[0].aces[0].ace_flags, 0);
        }
    }

    #[test]
    fn inherit_only_child_sid_ace_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[0].key)
            .unwrap()
            .clone();
        f.conditions[0].aces[0].ace_flags = 0x08; // INHERIT_ONLY_ACE
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    #[test]
    fn inherit_only_child_sid_ace_rejected_by_fast_path() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[1].key)
            .unwrap()
            .clone();
        f.conditions[0].aces[0].ace_flags = 0x08; // INHERIT_ONLY_ACE
        gate.engine.lock().unwrap().insert_raw_filter(f);

        // Fast path must be rejected, block_internet repairs filter
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let repaired = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[1].key)
            .unwrap()
            .clone();
        assert_eq!(repaired.conditions[0].aces[0].ace_flags, 0);
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Blocked
        );
    }

    #[test]
    fn block_repairs_owned_filter_with_inherit_only_ace() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[2].key)
            .unwrap()
            .clone();
        f.conditions[0].aces[0].ace_flags = 0x08; // INHERIT_ONLY_ACE
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );

        gate.block_internet(TEST_CHILD_SID).unwrap();

        let repaired = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[2].key)
            .unwrap()
            .clone();
        assert_eq!(repaired.conditions[0].aces[0].ace_flags, 0);
        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Blocked
        );
    }

    #[test]
    fn child_sid_ace_with_other_nonzero_flags_reports_unknown() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.block_internet(TEST_CHILD_SID).unwrap();

        let mut f = gate
            .engine
            .lock()
            .unwrap()
            .get_committed_filter(&CANONICAL_FILTERS[3].key)
            .unwrap()
            .clone();
        f.conditions[0].aces[0].ace_flags = 0x01; // OBJECT_INHERIT_ACE
        gate.engine.lock().unwrap().insert_raw_filter(f);

        assert_eq!(
            gate.current_state(TEST_CHILD_SID).unwrap(),
            InternetState::Unknown
        );
    }

    // --- Transaction Failure Handling Regression Tests (Section 22) ---

    #[test]
    fn provider_query_failure_inside_transaction_aborts() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.engine.lock().unwrap().fail_provider_query = Some(0x80320001);

        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        match err {
            WindowsWfpError::ProviderQueryFailure { win32_code } => {
                assert_eq!(win32_code, 0x80320001)
            }
            other => panic!("expected ProviderQueryFailure, got {:?}", other),
        }
        assert!(!gate.engine.lock().unwrap().is_in_transaction());
    }

    #[test]
    fn filter_query_failure_inside_transaction_aborts() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.engine.lock().unwrap().fail_filter_query = Some(0x80320002);

        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        match err {
            WindowsWfpError::FilterQueryFailure { win32_code } => {
                assert_eq!(win32_code, 0x80320002)
            }
            other => panic!("expected FilterQueryFailure, got {:?}", other),
        }
        assert!(!gate.engine.lock().unwrap().is_in_transaction());
    }

    #[test]
    fn enumeration_failure_inside_transaction_aborts() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.engine.lock().unwrap().fail_filter_enum = Some(0x80320003);

        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        match err {
            WindowsWfpError::FilterEnumerationFailure { win32_code } => {
                assert_eq!(win32_code, 0x80320003)
            }
            other => panic!("expected FilterEnumerationFailure, got {:?}", other),
        }
        assert!(!gate.engine.lock().unwrap().is_in_transaction());
    }

    #[test]
    fn query_failure_plus_abort_failure_reports_abort_failure() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.engine.lock().unwrap().fail_filter_query = Some(0x80320002);
        gate.engine.lock().unwrap().fail_transaction_abort = Some(0x8032000B);

        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        assert_eq!(
            err,
            WindowsWfpError::TransactionAbortFailure {
                win32_code: 0x8032000B
            }
        );
    }

    #[test]
    fn commit_failure_attempts_cleanup() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.engine.lock().unwrap().fail_transaction_commit = Some(0x8032000C);

        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        assert_eq!(
            err,
            WindowsWfpError::TransactionCommitFailure {
                win32_code: 0x8032000C
            }
        );
        // Abort must have cleaned up the transaction:
        assert!(!gate.engine.lock().unwrap().is_in_transaction());
    }

    #[test]
    fn commit_failure_plus_abort_failure_keeps_abort_failure_observable() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);
        gate.engine.lock().unwrap().fail_transaction_commit = Some(0x8032000C);
        gate.engine.lock().unwrap().fail_transaction_abort = Some(0x8032000B);

        let err = gate.block_internet(TEST_CHILD_SID).unwrap_err();
        assert_eq!(
            err,
            WindowsWfpError::TransactionAbortFailure {
                win32_code: 0x8032000B
            }
        );
    }
}
