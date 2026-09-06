//! Windows Filtering Platform (WFP) Win32 Management API implementation.

use crate::internet_gate::WindowsWfpError;
use crate::wfp::{
    GUID, WfpActionType, WfpConditionSnapshot, WfpEnginePort, WfpFilterSnapshot, WfpFilterSpec,
    WfpProviderSnapshot, WfpProviderSpec, WfpSidAceSnapshot, WfpSubLayerSnapshot, WfpSubLayerSpec,
};

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::wfp::{
        FWP_E_FILTER_NOT_FOUND, FWP_E_NOT_FOUND, FWP_E_PROVIDER_NOT_FOUND, FWP_E_SUBLAYER_NOT_FOUND,
    };
    use std::ffi::c_void;
    use windows::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_INVALID_SID,
        ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree,
    };
    use windows::Win32::NetworkManagement::WindowsFilteringPlatform::*;
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        ConvertStringSidToSidW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACCESS_DENIED_ACE, ACE_HEADER, ACL, GetAce,
        GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetSecurityDescriptorLength,
        IsValidAcl, IsValidSecurityDescriptor, IsValidSid, PSECURITY_DESCRIPTOR, PSID,
        SE_SELF_RELATIVE,
    };
    use windows::core::{BOOL, PCWSTR, PWSTR};

    // RPC_C_AUTHN_WINNT is 10
    const RPC_C_AUTHN_WINNT_VAL: u32 = 10;

    struct AutoLocalAlloc<T>(*mut T);
    impl<T> Drop for AutoLocalAlloc<T> {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(self.0 as _)));
                }
                self.0 = std::ptr::null_mut();
            }
        }
    }

    struct AutoFwpmMemory(*mut c_void);
    impl Drop for AutoFwpmMemory {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let mut p = self.0;
                    FwpmFreeMemory0(&raw mut p);
                }
                self.0 = std::ptr::null_mut();
            }
        }
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Validates that child_sid is a valid functional Windows SID.
    pub fn validate_windows_sid(child_sid: &str) -> Result<(), WindowsWfpError> {
        let child_sid_clean = child_sid.trim();
        if child_sid_clean.is_empty() {
            return Err(WindowsWfpError::InvalidSid {
                sid: child_sid.to_string(),
                win32_code: ERROR_INVALID_PARAMETER.0,
            });
        }

        let wide_sid = to_wide(child_sid_clean);
        let mut psid = PSID::default();
        let sid_res = unsafe { ConvertStringSidToSidW(PCWSTR(wide_sid.as_ptr()), &mut psid) };
        if let Err(err) = sid_res {
            return Err(WindowsWfpError::InvalidSid {
                sid: child_sid.to_string(),
                win32_code: err.code().0 as u32,
            });
        }
        let _sid_guard = AutoLocalAlloc(psid.0);

        let is_valid = unsafe { IsValidSid(psid) };
        if !is_valid.as_bool() {
            return Err(WindowsWfpError::InvalidSid {
                sid: child_sid.to_string(),
                win32_code: ERROR_INVALID_SID.0,
            });
        }
        Ok(())
    }

    /// Creates an object management security descriptor granting GA to SYSTEM and BUILTIN\Administrators.
    fn create_object_security_descriptor() -> Result<AutoLocalAlloc<c_void>, u32> {
        let wide_sddl = to_wide("D:P(A;;GA;;;SY)(A;;GA;;;BA)");
        let mut p_sd = PSECURITY_DESCRIPTOR::default();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide_sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut p_sd,
                None,
            )
        };
        if let Err(e) = ok {
            let code = e.code().0 as u32;
            return Err(code);
        }
        Ok(AutoLocalAlloc(p_sd.0))
    }

    /// Validates the child SID and creates a self-relative security descriptor granting 0x1 (FWP_ACTRL_MATCH_FILTER) to that SID.
    pub fn create_child_sid_condition_sd(child_sid: &str) -> Result<Vec<u8>, WindowsWfpError> {
        validate_windows_sid(child_sid)?;
        let child_sid_clean = child_sid.trim();

        // Create self-relative Security Descriptor with DACL granting 0x1 (FWP_ACTRL_MATCH_FILTER) to child_sid
        let sddl = format!("D:(A;;0x1;;;{})\0", child_sid_clean);
        let wide_sddl: Vec<u16> = sddl.encode_utf16().collect();
        let mut p_sd = PSECURITY_DESCRIPTOR::default();
        let conv_res = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide_sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut p_sd,
                None,
            )
        };
        if let Err(err) = conv_res {
            return Err(WindowsWfpError::InvalidSid {
                sid: child_sid.to_string(),
                win32_code: err.code().0 as u32,
            });
        }
        let sd_guard = AutoLocalAlloc(p_sd.0);

        let sd_len = unsafe { GetSecurityDescriptorLength(p_sd) };
        if sd_len == 0 {
            return Err(WindowsWfpError::InvalidSid {
                sid: child_sid.to_string(),
                win32_code: ERROR_INVALID_PARAMETER.0,
            });
        }

        let slice = unsafe { std::slice::from_raw_parts(p_sd.0 as *const u8, sd_len as usize) };
        let mut sd_bytes = Vec::with_capacity(sd_len as usize);
        sd_bytes.extend_from_slice(slice);
        drop(sd_guard);
        Ok(sd_bytes)
    }

    /// Authoritatively inspects a condition security descriptor buffer and extracts DACL ACEs.
    pub fn parse_security_descriptor_condition(
        sd_bytes: &[u8],
    ) -> (bool, bool, bool, Vec<WfpSidAceSnapshot>) {
        if sd_bytes.is_empty() {
            return (false, false, false, Vec::new());
        }
        let p_sd = PSECURITY_DESCRIPTOR(sd_bytes.as_ptr() as *mut c_void);
        let is_valid = unsafe { IsValidSecurityDescriptor(p_sd) };
        if !is_valid.as_bool() {
            return (false, false, false, Vec::new());
        }

        let mut control = 0u16;
        let mut revision = 0u32;
        let ctrl_ok = unsafe { GetSecurityDescriptorControl(p_sd, &mut control, &mut revision) };
        let is_self_relative = ctrl_ok.is_ok() && (control & SE_SELF_RELATIVE.0) != 0;

        let mut dacl_present = BOOL::default();
        let mut p_dacl: *mut ACL = std::ptr::null_mut();
        let mut dacl_defaulted = BOOL::default();
        let ok = unsafe {
            GetSecurityDescriptorDacl(p_sd, &mut dacl_present, &mut p_dacl, &mut dacl_defaulted)
        };
        if ok.is_err() || !dacl_present.as_bool() || p_dacl.is_null() {
            return (true, false, is_self_relative, Vec::new());
        }

        // Section 7: ACL VALIDITY GATE
        let is_valid_acl = unsafe { IsValidAcl(p_dacl) };
        if !is_valid_acl.as_bool() {
            return (true, false, is_self_relative, Vec::new());
        }

        let dacl = unsafe { &*p_dacl };
        let ace_count = dacl.AceCount as u32;
        let mut aces = Vec::with_capacity(ace_count as usize);

        for i in 0..ace_count {
            let mut p_ace: *mut c_void = std::ptr::null_mut();
            let ace_res = unsafe { GetAce(p_dacl, i, &mut p_ace) };
            if ace_res.is_err() || p_ace.is_null() {
                // Malformed ACE in DACL -> whole condition is malformed/noncanonical
                return (false, false, false, Vec::new());
            }
            let header = unsafe { &*(p_ace as *const ACE_HEADER) };
            let ace_flags = header.AceFlags;
            if header.AceType == 0 {
                // ACCESS_ALLOWED_ACE_TYPE
                let allowed = unsafe { &*(p_ace as *const ACCESS_ALLOWED_ACE) };
                let mask = allowed.Mask;
                let p_sid = PSID((&raw const allowed.SidStart) as *mut c_void);
                if unsafe { IsValidSid(p_sid) }.as_bool() {
                    let mut pwstr = PWSTR::null();
                    let conv_res = unsafe { ConvertSidToStringSidW(p_sid, &mut pwstr) };
                    if conv_res.is_ok() && !pwstr.is_null() {
                        let _guard = AutoLocalAlloc(pwstr.0);
                        let sid_str = unsafe { pwstr.to_string().unwrap_or_default() };
                        aces.push(WfpSidAceSnapshot {
                            sid: sid_str,
                            mask,
                            is_allow: true,
                            ace_flags,
                        });
                    } else {
                        return (false, false, false, Vec::new());
                    }
                } else {
                    return (false, false, false, Vec::new());
                }
            } else if header.AceType == 1 {
                // ACCESS_DENIED_ACE_TYPE
                let denied = unsafe { &*(p_ace as *const ACCESS_DENIED_ACE) };
                let mask = denied.Mask;
                let p_sid = PSID((&raw const denied.SidStart) as *mut c_void);
                if unsafe { IsValidSid(p_sid) }.as_bool() {
                    let mut pwstr = PWSTR::null();
                    let conv_res = unsafe { ConvertSidToStringSidW(p_sid, &mut pwstr) };
                    if conv_res.is_ok() && !pwstr.is_null() {
                        let _guard = AutoLocalAlloc(pwstr.0);
                        let sid_str = unsafe { pwstr.to_string().unwrap_or_default() };
                        aces.push(WfpSidAceSnapshot {
                            sid: sid_str,
                            mask,
                            is_allow: false,
                            ace_flags,
                        });
                    } else {
                        return (false, false, false, Vec::new());
                    }
                } else {
                    return (false, false, false, Vec::new());
                }
            } else {
                aces.push(WfpSidAceSnapshot {
                    sid: String::new(),
                    mask: 0,
                    is_allow: false,
                    ace_flags,
                });
            }
        }

        (true, true, is_self_relative, aces)
    }

    /// Parses all filter conditions from an FWPM_FILTER0 structure into truthful condition snapshots.
    pub fn parse_filter_conditions(filt: &FWPM_FILTER0) -> Vec<WfpConditionSnapshot> {
        let mut conditions = Vec::new();
        if filt.numFilterConditions > 0 && !filt.filterCondition.is_null() {
            let conds = unsafe {
                std::slice::from_raw_parts(filt.filterCondition, filt.numFilterConditions as usize)
            };
            for cond in conds {
                let mut is_valid_sd = false;
                let mut dacl_present = false;
                let mut is_self_relative = false;
                let mut aces = Vec::new();

                if cond.conditionValue.r#type == FWP_SECURITY_DESCRIPTOR_TYPE {
                    unsafe {
                        let sd_blob = cond.conditionValue.Anonymous.sd;
                        if !sd_blob.is_null() && (*sd_blob).size > 0 && !(*sd_blob).data.is_null() {
                            let blob_slice = std::slice::from_raw_parts(
                                (*sd_blob).data,
                                (*sd_blob).size as usize,
                            );
                            let (valid, dacl, self_rel, parsed_aces) =
                                parse_security_descriptor_condition(blob_slice);
                            is_valid_sd = valid;
                            dacl_present = dacl;
                            is_self_relative = self_rel;
                            aces = parsed_aces;
                        }
                    }
                }

                conditions.push(WfpConditionSnapshot {
                    field_key: cond.fieldKey,
                    match_type: cond.matchType.0 as u32,
                    condition_value_type: cond.conditionValue.r#type.0 as u32,
                    is_valid_sd,
                    dacl_present,
                    is_self_relative,
                    aces,
                });
            }
        }
        conditions
    }

    /// Pure helper constructing the authoritative PALKA-provider filter enumeration template.
    pub fn build_provider_filter_enum_template(
        provider_key: &mut GUID,
    ) -> FWPM_FILTER_ENUM_TEMPLATE0 {
        FWPM_FILTER_ENUM_TEMPLATE0 {
            providerKey: provider_key as *mut GUID,
            layerKey: GUID::default(),
            enumType: FWP_FILTER_ENUM_FULLY_CONTAINED,
            flags: FWP_FILTER_ENUM_FLAG_INCLUDE_DISABLED | FWP_FILTER_ENUM_FLAG_INCLUDE_BOOTTIME,
            actionMask: 0xFFFFFFFF,
            ..Default::default()
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FilterEnumPageDecision {
        ExhaustedNormally,
        ExhaustedFreeMemory,
        ProcessPage(u32),
    }

    /// Pure helper classifying FwpmFilterEnum0 page output.
    pub fn classify_filter_enum_page(
        num_entries: u32,
        has_entries_ptr: bool,
    ) -> Result<FilterEnumPageDecision, WindowsWfpError> {
        match (num_entries, has_entries_ptr) {
            (0, false) => Ok(FilterEnumPageDecision::ExhaustedNormally),
            (0, true) => Ok(FilterEnumPageDecision::ExhaustedFreeMemory),
            (_, false) => Err(WindowsWfpError::InconsistentState {
                details: "BFE filter enumeration returned non-zero count with null entries pointer",
            }),
            (n, true) => Ok(FilterEnumPageDecision::ProcessPage(n)),
        }
    }

    struct PageMemoryGuard(*mut c_void);
    impl Drop for PageMemoryGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let mut p = self.0;
                    FwpmFreeMemory0(&raw mut p);
                }
                self.0 = std::ptr::null_mut();
            }
        }
    }

    /// Safe wrapper around BFE Engine Handle.
    pub struct WindowsWfpEngine {
        handle: HANDLE,
    }

    impl WindowsWfpEngine {
        /// Opens a standard non-dynamic BFE engine session.
        pub fn open() -> Result<Self, WindowsWfpError> {
            let session = FWPM_SESSION0 {
                flags: 0, // NON_DYNAMIC! FWPM_SESSION_FLAG_DYNAMIC is strictly forbidden
                txnWaitTimeoutInMSec: 5000,
                ..Default::default()
            };
            let mut handle = HANDLE::default();
            let ret = unsafe {
                FwpmEngineOpen0(
                    PCWSTR::null(),
                    RPC_C_AUTHN_WINNT_VAL,
                    None,
                    Some(&session),
                    &mut handle,
                )
            };
            if ret != ERROR_SUCCESS.0 {
                if ret == ERROR_ACCESS_DENIED.0 {
                    return Err(WindowsWfpError::AccessDenied { win32_code: ret });
                }
                return Err(WindowsWfpError::EngineUnavailable { win32_code: ret });
            }
            Ok(Self { handle })
        }

        pub fn raw_handle(&self) -> HANDLE {
            self.handle
        }
    }

    impl Drop for WindowsWfpEngine {
        fn drop(&mut self) {
            if !self.handle.is_invalid() {
                unsafe {
                    let _ = FwpmEngineClose0(self.handle);
                }
                self.handle = HANDLE::default();
            }
        }
    }

    unsafe impl Send for WindowsWfpEngine {}
    unsafe impl Sync for WindowsWfpEngine {}

    impl WfpEnginePort for WindowsWfpEngine {
        fn validate_sid(&self, child_sid: &str) -> Result<(), WindowsWfpError> {
            validate_windows_sid(child_sid)
        }

        fn transaction_begin(&mut self) -> Result<(), WindowsWfpError> {
            let ret = unsafe { FwpmTransactionBegin0(self.handle, 0) };
            if ret != ERROR_SUCCESS.0 {
                return Err(WindowsWfpError::TransactionBeginFailure { win32_code: ret });
            }
            Ok(())
        }

        fn transaction_commit(&mut self) -> Result<(), WindowsWfpError> {
            let ret = unsafe { FwpmTransactionCommit0(self.handle) };
            if ret != ERROR_SUCCESS.0 {
                return Err(WindowsWfpError::TransactionCommitFailure { win32_code: ret });
            }
            Ok(())
        }

        fn transaction_abort(&mut self) -> Result<(), WindowsWfpError> {
            let ret = unsafe { FwpmTransactionAbort0(self.handle) };
            if ret != ERROR_SUCCESS.0 {
                return Err(WindowsWfpError::TransactionAbortFailure { win32_code: ret });
            }
            Ok(())
        }

        fn get_provider(&self, key: &GUID) -> Result<Option<WfpProviderSnapshot>, WindowsWfpError> {
            let mut p_provider: *mut FWPM_PROVIDER0 = std::ptr::null_mut();
            let ret = unsafe { FwpmProviderGetByKey0(self.handle, key, &mut p_provider) };
            if ret == FWP_E_PROVIDER_NOT_FOUND
                || ret == FWP_E_NOT_FOUND
                || ret == ERROR_FILE_NOT_FOUND.0
            {
                return Ok(None);
            }
            if ret != ERROR_SUCCESS.0 {
                return Err(WindowsWfpError::ProviderQueryFailure { win32_code: ret });
            }
            if p_provider.is_null() {
                return Ok(None);
            }
            let _guard = AutoFwpmMemory(p_provider as _);
            let prov = unsafe { &*p_provider };

            let name = unsafe {
                if prov.displayData.name.is_null() {
                    String::new()
                } else {
                    prov.displayData.name.to_string().unwrap_or_default()
                }
            };
            let desc = unsafe {
                if prov.displayData.description.is_null() {
                    String::new()
                } else {
                    prov.displayData.description.to_string().unwrap_or_default()
                }
            };
            let svc = unsafe {
                if prov.serviceName.is_null() {
                    None
                } else {
                    prov.serviceName.to_string().ok()
                }
            };
            let data = if prov.providerData.size > 0 && !prov.providerData.data.is_null() {
                unsafe {
                    std::slice::from_raw_parts(
                        prov.providerData.data,
                        prov.providerData.size as usize,
                    )
                    .to_vec()
                }
            } else {
                Vec::new()
            };
            let disabled = (prov.flags & FWPM_PROVIDER_FLAG_DISABLED) != 0;

            Ok(Some(WfpProviderSnapshot {
                provider_key: prov.providerKey,
                display_name: name,
                description: desc,
                service_name: svc,
                provider_data: data,
                flags: prov.flags,
                disabled,
            }))
        }

        fn add_provider(&mut self, spec: &WfpProviderSpec) -> Result<(), WindowsWfpError> {
            let sd_guard = create_object_security_descriptor()
                .map_err(|code| WindowsWfpError::ProviderMutationFailure { win32_code: code })?;
            let wide_name = to_wide(&spec.display_name);
            let wide_desc = to_wide(&spec.description);
            let wide_svc = spec.service_name.as_ref().map(|s| to_wide(s));

            let mut flags = 0u32;
            if spec.persistent {
                flags |= FWPM_PROVIDER_FLAG_PERSISTENT;
            }

            let mut prov_data_bytes = spec.provider_data.clone();
            let prov_data = FWP_BYTE_BLOB {
                size: prov_data_bytes.len() as u32,
                data: if prov_data_bytes.is_empty() {
                    std::ptr::null_mut()
                } else {
                    prov_data_bytes.as_mut_ptr()
                },
            };

            let provider = FWPM_PROVIDER0 {
                providerKey: spec.provider_key,
                displayData: FWPM_DISPLAY_DATA0 {
                    name: PWSTR(wide_name.as_ptr() as *mut u16),
                    description: PWSTR(wide_desc.as_ptr() as *mut u16),
                },
                flags,
                providerData: prov_data,
                serviceName: if let Some(ref svc) = wide_svc {
                    PWSTR(svc.as_ptr() as *mut u16)
                } else {
                    PWSTR::null()
                },
            };

            let ret = unsafe {
                FwpmProviderAdd0(
                    self.handle,
                    &provider,
                    Some(PSECURITY_DESCRIPTOR(sd_guard.0)),
                )
            };
            if ret != ERROR_SUCCESS.0 {
                return Err(WindowsWfpError::ProviderMutationFailure { win32_code: ret });
            }
            Ok(())
        }

        fn delete_provider(&mut self, key: &GUID) -> Result<(), WindowsWfpError> {
            let ret = unsafe { FwpmProviderDeleteByKey0(self.handle, key) };
            if ret == FWP_E_PROVIDER_NOT_FOUND
                || ret == FWP_E_NOT_FOUND
                || ret == ERROR_FILE_NOT_FOUND.0
            {
                return Ok(());
            }
            if ret != ERROR_SUCCESS.0 {
                return Err(WindowsWfpError::ProviderMutationFailure { win32_code: ret });
            }
            Ok(())
        }

        fn get_sublayer(&self, key: &GUID) -> Result<Option<WfpSubLayerSnapshot>, WindowsWfpError> {
            let mut p_sublayer: *mut FWPM_SUBLAYER0 = std::ptr::null_mut();
            let ret = unsafe { FwpmSubLayerGetByKey0(self.handle, key, &mut p_sublayer) };
            if ret == FWP_E_SUBLAYER_NOT_FOUND
                || ret == FWP_E_NOT_FOUND
                || ret == ERROR_FILE_NOT_FOUND.0
            {
                return Ok(None);
            }
            if ret != ERROR_SUCCESS.0 {
                return Err(WindowsWfpError::SublayerQueryFailure { win32_code: ret });
            }
            if p_sublayer.is_null() {
                return Ok(None);
            }
            let _guard = AutoFwpmMemory(p_sublayer as _);
            let sub = unsafe { &*p_sublayer };

            let name = unsafe {
                if sub.displayData.name.is_null() {
                    String::new()
                } else {
                    sub.displayData.name.to_string().unwrap_or_default()
                }
            };
            let desc = unsafe {
                if sub.displayData.description.is_null() {
                    String::new()
                } else {
                    sub.displayData.description.to_string().unwrap_or_default()
                }
            };

            let prov_key = if sub.providerKey.is_null() {
                GUID::default()
            } else {
                unsafe { *sub.providerKey }
            };

            Ok(Some(WfpSubLayerSnapshot {
                sublayer_key: sub.subLayerKey,
                display_name: name,
                description: desc,
                provider_key: prov_key,
                weight: sub.weight,
                flags: sub.flags,
            }))
        }

        fn add_sublayer(&mut self, spec: &WfpSubLayerSpec) -> Result<(), WindowsWfpError> {
            let sd_guard = create_object_security_descriptor()
                .map_err(|code| WindowsWfpError::SublayerMutationFailure { win32_code: code })?;
            let wide_name = to_wide(&spec.display_name);
            let wide_desc = to_wide(&spec.description);

            let mut flags = 0u32;
            if spec.persistent {
                flags |= FWPM_SUBLAYER_FLAG_PERSISTENT;
            }

            let mut prov_key = spec.provider_key;
            let sublayer = FWPM_SUBLAYER0 {
                subLayerKey: spec.sublayer_key,
                displayData: FWPM_DISPLAY_DATA0 {
                    name: PWSTR(wide_name.as_ptr() as *mut u16),
                    description: PWSTR(wide_desc.as_ptr() as *mut u16),
                },
                flags,
                providerKey: &mut prov_key as *mut GUID,
                providerData: FWP_BYTE_BLOB::default(),
                weight: spec.weight,
            };

            let ret = unsafe {
                FwpmSubLayerAdd0(
                    self.handle,
                    &sublayer,
                    Some(PSECURITY_DESCRIPTOR(sd_guard.0)),
                )
            };
            if ret != ERROR_SUCCESS.0 {
                return Err(WindowsWfpError::SublayerMutationFailure { win32_code: ret });
            }
            Ok(())
        }

        fn delete_sublayer(&mut self, key: &GUID) -> Result<(), WindowsWfpError> {
            let ret = unsafe { FwpmSubLayerDeleteByKey0(self.handle, key) };
            if ret == FWP_E_SUBLAYER_NOT_FOUND
                || ret == FWP_E_NOT_FOUND
                || ret == ERROR_FILE_NOT_FOUND.0
            {
                return Ok(());
            }
            if ret != ERROR_SUCCESS.0 {
                return Err(WindowsWfpError::SublayerMutationFailure { win32_code: ret });
            }
            Ok(())
        }

        fn get_filter(&self, key: &GUID) -> Result<Option<WfpFilterSnapshot>, WindowsWfpError> {
            let mut p_filter: *mut FWPM_FILTER0 = std::ptr::null_mut();
            let ret = unsafe { FwpmFilterGetByKey0(self.handle, key, &mut p_filter) };
            if ret == FWP_E_FILTER_NOT_FOUND
                || ret == FWP_E_NOT_FOUND
                || ret == ERROR_FILE_NOT_FOUND.0
            {
                return Ok(None);
            }
            if ret != ERROR_SUCCESS.0 {
                return Err(WindowsWfpError::FilterQueryFailure { win32_code: ret });
            }
            if p_filter.is_null() {
                return Ok(None);
            }
            let _guard = AutoFwpmMemory(p_filter as _);
            let filt = unsafe { &*p_filter };

            let name = unsafe {
                if filt.displayData.name.is_null() {
                    String::new()
                } else {
                    filt.displayData.name.to_string().unwrap_or_default()
                }
            };
            let desc = unsafe {
                if filt.displayData.description.is_null() {
                    String::new()
                } else {
                    filt.displayData.description.to_string().unwrap_or_default()
                }
            };

            let action_type = match filt.action.r#type {
                FWP_ACTION_BLOCK => WfpActionType::Block,
                FWP_ACTION_PERMIT => WfpActionType::Permit,
                FWP_ACTION_CALLOUT_TERMINATING
                | FWP_ACTION_CALLOUT_INSPECTION
                | FWP_ACTION_CALLOUT_UNKNOWN => WfpActionType::Callout,
                other => WfpActionType::Other(other.0),
            };

            let weight_u8 = if filt.weight.r#type == FWP_UINT8 {
                unsafe { filt.weight.Anonymous.uint8 }
            } else {
                0
            };

            let prov_key = if filt.providerKey.is_null() {
                None
            } else {
                unsafe { Some(*filt.providerKey) }
            };

            let disabled = (filt.flags.0 & FWPM_FILTER_FLAG_DISABLED.0) != 0;
            let clear_action_right = (filt.flags.0 & FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT.0) != 0;

            let conditions = parse_filter_conditions(filt);

            Ok(Some(WfpFilterSnapshot {
                filter_key: filt.filterKey,
                display_name: name,
                description: desc,
                layer_key: filt.layerKey,
                sublayer_key: filt.subLayerKey,
                provider_key: prov_key,
                weight: weight_u8,
                action_type,
                flags: filt.flags.0,
                disabled,
                clear_action_right,
                conditions,
            }))
        }

        fn add_filter(&mut self, spec: &WfpFilterSpec) -> Result<(), WindowsWfpError> {
            let sd_guard = create_object_security_descriptor()
                .map_err(|code| WindowsWfpError::FilterAddFailure { win32_code: code })?;
            let wide_name = to_wide(&spec.display_name);
            let wide_desc = to_wide(&spec.description);

            let mut flags = 0u32;
            if spec.persistent {
                flags |= FWPM_FILTER_FLAG_PERSISTENT.0;
            }
            if spec.clear_action_right {
                flags |= FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT.0;
            }

            let action_type_val = match spec.action_type {
                WfpActionType::Block => FWP_ACTION_BLOCK,
                WfpActionType::Permit => FWP_ACTION_PERMIT,
                WfpActionType::Callout => FWP_ACTION_CALLOUT_TERMINATING,
                WfpActionType::Other(v) => FWP_ACTION_TYPE(v),
            };

            let mut cond_sd_bytes: Option<Vec<u8>> = None;
            let mut cond_blob = FWP_BYTE_BLOB::default();
            let mut filter_conds = Vec::new();

            if let Some(ref sid_str) = spec.sid_condition {
                let bytes = create_child_sid_condition_sd(sid_str)?;
                cond_blob.size = bytes.len() as u32;
                cond_blob.data = bytes.as_ptr() as *mut u8;
                cond_sd_bytes = Some(bytes);

                let cond = FWPM_FILTER_CONDITION0 {
                    fieldKey: FWPM_CONDITION_ALE_USER_ID,
                    matchType: FWP_MATCH_EQUAL,
                    conditionValue: FWP_CONDITION_VALUE0 {
                        r#type: FWP_SECURITY_DESCRIPTOR_TYPE,
                        Anonymous: FWP_CONDITION_VALUE0_0 {
                            sd: &mut cond_blob as *mut FWP_BYTE_BLOB,
                        },
                    },
                };
                filter_conds.push(cond);
            }

            let mut prov_key = spec.provider_key;
            let filter = FWPM_FILTER0 {
                filterKey: spec.filter_key,
                displayData: FWPM_DISPLAY_DATA0 {
                    name: PWSTR(wide_name.as_ptr() as *mut u16),
                    description: PWSTR(wide_desc.as_ptr() as *mut u16),
                },
                flags: FWPM_FILTER_FLAGS(flags),
                providerKey: if let Some(ref mut k) = prov_key {
                    k as *mut GUID
                } else {
                    std::ptr::null_mut()
                },
                layerKey: spec.layer_key,
                subLayerKey: spec.sublayer_key,
                weight: FWP_VALUE0 {
                    r#type: FWP_UINT8,
                    Anonymous: FWP_VALUE0_0 { uint8: spec.weight },
                },
                numFilterConditions: filter_conds.len() as u32,
                filterCondition: if filter_conds.is_empty() {
                    std::ptr::null_mut()
                } else {
                    filter_conds.as_mut_ptr()
                },
                action: FWPM_ACTION0 {
                    r#type: action_type_val,
                    ..Default::default()
                },
                ..Default::default()
            };

            let ret = unsafe {
                FwpmFilterAdd0(
                    self.handle,
                    &filter,
                    Some(PSECURITY_DESCRIPTOR(sd_guard.0)),
                    None,
                )
            };
            drop(cond_sd_bytes);
            if ret != ERROR_SUCCESS.0 {
                return Err(WindowsWfpError::FilterAddFailure { win32_code: ret });
            }
            Ok(())
        }

        fn delete_filter(&mut self, key: &GUID) -> Result<(), WindowsWfpError> {
            let ret = unsafe { FwpmFilterDeleteByKey0(self.handle, key) };
            if ret == FWP_E_FILTER_NOT_FOUND
                || ret == FWP_E_NOT_FOUND
                || ret == ERROR_FILE_NOT_FOUND.0
            {
                return Ok(());
            }
            if ret != ERROR_SUCCESS.0 {
                return Err(WindowsWfpError::FilterDeleteFailure { win32_code: ret });
            }
            Ok(())
        }

        fn enum_filters_by_provider(
            &self,
            provider_key: &GUID,
        ) -> Result<Vec<WfpFilterSnapshot>, WindowsWfpError> {
            let mut enum_handle = HANDLE::default();
            let mut prov_key = *provider_key;
            let enum_template = build_provider_filter_enum_template(&mut prov_key);

            let ret = unsafe {
                FwpmFilterCreateEnumHandle0(self.handle, Some(&enum_template), &mut enum_handle)
            };
            if ret != ERROR_SUCCESS.0 {
                return Err(WindowsWfpError::FilterEnumerationFailure { win32_code: ret });
            }

            struct EnumGuard {
                engine: HANDLE,
                enum_handle: HANDLE,
            }
            impl Drop for EnumGuard {
                fn drop(&mut self) {
                    if !self.enum_handle.is_invalid() {
                        unsafe {
                            let _ = FwpmFilterDestroyEnumHandle0(self.engine, self.enum_handle);
                        }
                    }
                }
            }
            let _enum_guard = EnumGuard {
                engine: self.handle,
                enum_handle,
            };

            let mut all_filters = Vec::new();
            const PAGE_SIZE: u32 = 100;

            loop {
                let mut p_entries: *mut *mut FWPM_FILTER0 = std::ptr::null_mut();
                let mut num_entries: u32 = 0;
                let ret_enum = unsafe {
                    FwpmFilterEnum0(
                        self.handle,
                        enum_handle,
                        PAGE_SIZE,
                        &mut p_entries,
                        &mut num_entries,
                    )
                };
                if ret_enum != ERROR_SUCCESS.0 {
                    return Err(WindowsWfpError::FilterEnumerationFailure {
                        win32_code: ret_enum,
                    });
                }

                let page_guard = PageMemoryGuard(p_entries as *mut c_void);
                let decision = classify_filter_enum_page(num_entries, !p_entries.is_null())?;

                match decision {
                    FilterEnumPageDecision::ExhaustedNormally => break,
                    FilterEnumPageDecision::ExhaustedFreeMemory => {
                        drop(page_guard);
                        break;
                    }
                    FilterEnumPageDecision::ProcessPage(count) => {
                        unsafe {
                            let filter_ptrs = std::slice::from_raw_parts(p_entries, count as usize);
                            for &p_filt in filter_ptrs {
                                if p_filt.is_null() {
                                    return Err(WindowsWfpError::InconsistentState {
                                        details: "BFE filter enumeration returned null filter entry in page",
                                    });
                                }
                                let filt = &*p_filt;

                                let name = if filt.displayData.name.is_null() {
                                    String::new()
                                } else {
                                    filt.displayData.name.to_string().unwrap_or_default()
                                };
                                let desc = if filt.displayData.description.is_null() {
                                    String::new()
                                } else {
                                    filt.displayData.description.to_string().unwrap_or_default()
                                };

                                let action_type = match filt.action.r#type {
                                    FWP_ACTION_BLOCK => WfpActionType::Block,
                                    FWP_ACTION_PERMIT => WfpActionType::Permit,
                                    FWP_ACTION_CALLOUT_TERMINATING
                                    | FWP_ACTION_CALLOUT_INSPECTION
                                    | FWP_ACTION_CALLOUT_UNKNOWN => WfpActionType::Callout,
                                    other => WfpActionType::Other(other.0),
                                };

                                let weight_u8 = if filt.weight.r#type == FWP_UINT8 {
                                    filt.weight.Anonymous.uint8
                                } else {
                                    0
                                };

                                let prov_key_opt = if filt.providerKey.is_null() {
                                    None
                                } else {
                                    Some(*filt.providerKey)
                                };

                                let disabled = (filt.flags.0 & FWPM_FILTER_FLAG_DISABLED.0) != 0;
                                let clear_action_right =
                                    (filt.flags.0 & FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT.0) != 0;

                                let conditions = parse_filter_conditions(filt);

                                all_filters.push(WfpFilterSnapshot {
                                    filter_key: filt.filterKey,
                                    display_name: name,
                                    description: desc,
                                    layer_key: filt.layerKey,
                                    sublayer_key: filt.subLayerKey,
                                    provider_key: prov_key_opt,
                                    weight: weight_u8,
                                    action_type,
                                    flags: filt.flags.0,
                                    disabled,
                                    clear_action_right,
                                    conditions,
                                });
                            }
                        }
                        drop(page_guard);

                        if count < PAGE_SIZE {
                            break;
                        }
                    }
                }
            }

            Ok(all_filters)
        }
    }
}

#[cfg(windows)]
pub use imp::*;

#[cfg(not(windows))]
pub struct WindowsWfpEngine;

#[cfg(not(windows))]
impl WindowsWfpEngine {
    pub fn open() -> Result<Self, WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
}

#[cfg(not(windows))]
impl WfpEnginePort for WindowsWfpEngine {
    fn validate_sid(&self, _child_sid: &str) -> Result<(), WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
    fn transaction_begin(&mut self) -> Result<(), WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
    fn transaction_commit(&mut self) -> Result<(), WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
    fn transaction_abort(&mut self) -> Result<(), WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
    fn get_provider(&self, _key: &GUID) -> Result<Option<WfpProviderSnapshot>, WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
    fn add_provider(&mut self, _spec: &WfpProviderSpec) -> Result<(), WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
    fn delete_provider(&mut self, _key: &GUID) -> Result<(), WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
    fn get_sublayer(&self, _key: &GUID) -> Result<Option<WfpSubLayerSnapshot>, WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
    fn add_sublayer(&mut self, _spec: &WfpSubLayerSpec) -> Result<(), WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
    fn delete_sublayer(&mut self, _key: &GUID) -> Result<(), WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
    fn get_filter(&self, _key: &GUID) -> Result<Option<WfpFilterSnapshot>, WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
    fn add_filter(&mut self, _spec: &WfpFilterSpec) -> Result<(), WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
    fn delete_filter(&mut self, _key: &GUID) -> Result<(), WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
    fn enum_filters_by_provider(
        &self,
        _provider_key: &GUID,
    ) -> Result<Vec<WfpFilterSnapshot>, WindowsWfpError> {
        Err(WindowsWfpError::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn test_provider_filter_enum_template_structure() {
        use crate::wfp::{
            FWP_FILTER_ENUM_FLAG_INCLUDE_BOOTTIME, FWP_FILTER_ENUM_FLAG_INCLUDE_DISABLED,
        };
        use windows::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_FILTER_ENUM_FULLY_CONTAINED;

        let mut key = crate::internet_gate::PALKA_WFP_PROVIDER_KEY;
        let tpl = imp::build_provider_filter_enum_template(&mut key);
        assert_eq!(tpl.enumType, FWP_FILTER_ENUM_FULLY_CONTAINED);
        assert_eq!(
            tpl.flags,
            FWP_FILTER_ENUM_FLAG_INCLUDE_DISABLED | FWP_FILTER_ENUM_FLAG_INCLUDE_BOOTTIME
        );
        assert_eq!(tpl.actionMask, 0xFFFFFFFF);
        assert_eq!(tpl.providerKey, &mut key as *mut GUID);
    }

    #[cfg(windows)]
    #[test]
    fn test_classify_filter_enum_page_semantics() {
        use imp::{FilterEnumPageDecision, classify_filter_enum_page};

        // Case A: 0 entries, null pointer -> exhausted normally
        let dec_a = classify_filter_enum_page(0, false).unwrap();
        assert_eq!(dec_a, FilterEnumPageDecision::ExhaustedNormally);

        // Case B: 0 entries, non-null pointer -> free memory and exhausted
        let dec_b = classify_filter_enum_page(0, true).unwrap();
        assert_eq!(dec_b, FilterEnumPageDecision::ExhaustedFreeMemory);

        // Case C: non-zero entries, null pointer -> inconsistent state error
        let err_c = classify_filter_enum_page(10, false).unwrap_err();
        match err_c {
            WindowsWfpError::InconsistentState { details } => {
                assert!(details.contains("null entries pointer"));
            }
            other => panic!("expected InconsistentState, got {:?}", other),
        }

        // Case D: valid non-zero entries, non-null pointer -> process page
        let dec_d = classify_filter_enum_page(42, true).unwrap();
        assert_eq!(dec_d, FilterEnumPageDecision::ProcessPage(42));
    }
}
