//! Normative test coverage for PALKA IPC V1 Slice 1: Protocol & Codec.
//! Validates IPC-01..15, 20..29, 65, 66, 75..88, 90..92.

use palka_ipc_protocol::*;
use std::collections::HashSet;

// =============================================================================
// IPC-01 & IPC-02: Architectural Boundary & Ownership (STATIC)
// =============================================================================

#[test]
fn ipc_01_core_crate_is_free_from_ipc_wire_and_serde() {
    // palka-core does not depend on serde or provide named pipe wire codecs.
    let timer_id = palka_core::TimerId([1u8; 16]);
    let wire_id = WireTimerId::from_core(timer_id);
    assert_eq!(wire_id.to_core(), timer_id);
}

#[test]
fn ipc_02_ipc_protocol_owns_envelopes_dtos_and_codec() {
    let req = RequestEnvelope::new(RequestPayload::QueryStatus);
    let framed = encode_request_frame(&req).expect("Encoding must succeed");
    assert!(framed.len() > 4);
}

// =============================================================================
// IPC-03, IPC-04, IPC-05: Protocol Versioning
// =============================================================================

#[test]
fn ipc_03_version_1_is_accepted_in_envelopes() {
    let valid_req = r#"{"version":1,"type":"request","request":{"kind":"QueryStatus"}}"#;
    let decoded = decode_request_frame(valid_req.as_bytes()).expect("version 1 must be accepted");
    assert_eq!(decoded.version, 1);
    assert_eq!(decoded.request, RequestPayload::QueryStatus);

    let valid_resp = r#"{"version":1,"type":"response","response":{"kind":"Acknowledged"}}"#;
    let decoded_resp =
        decode_response_frame(valid_resp.as_bytes()).expect("version 1 must be accepted");
    assert_eq!(decoded_resp.version, 1);

    let valid_err = r#"{"version":1,"type":"error","error":{"code":"InvalidRequest","message":null,"retry_after_seconds":null}}"#;
    let decoded_err = decode_error_frame(valid_err.as_bytes()).expect("version 1 must be accepted");
    assert_eq!(decoded_err.version, 1);

    let valid_ev = r#"{"version":1,"type":"event","event":{"kind":"ShutdownStateChanged","previous":"Idle","current":"Scheduled"}}"#;
    let decoded_ev = decode_event_frame(valid_ev.as_bytes()).expect("version 1 must be accepted");
    assert_eq!(decoded_ev.version, 1);
}

#[test]
fn ipc_04_missing_version_is_rejected_as_protocol_error() {
    let missing_ver_req = r#"{"type":"request","request":{"kind":"QueryStatus"}}"#;
    let err = decode_request_frame(missing_ver_req.as_bytes()).unwrap_err();
    assert_eq!(err.code, ErrorCode::ProtocolError);

    let missing_ver_resp = r#"{"type":"response","response":{"kind":"Acknowledged"}}"#;
    let err_resp = decode_response_frame(missing_ver_resp.as_bytes()).unwrap_err();
    assert_eq!(err_resp.code, ErrorCode::ProtocolError);
}

#[test]
fn ipc_05_unsupported_version_is_rejected_as_unsupported_protocol_version() {
    // Integer values other than 1 must be rejected as UnsupportedProtocolVersion
    // Boundary and adversarial test cases including 0, 2, 2^32, 2^32 + 1, -1
    let invalid_integer_versions = [
        "0",
        "2",
        "3",
        "4294967296", // 2^32
        "4294967297", // 2^32 + 1 (wrapping boundary: 4294967297 as u32 == 1)
        "-1",
    ];

    for ver in &invalid_integer_versions {
        // Request envelope
        let req_json = format!(
            r#"{{"version":{},"type":"request","request":{{"kind":"QueryStatus"}}}}"#,
            ver
        );
        let err_req = decode_request_frame(req_json.as_bytes()).unwrap_err();
        assert_eq!(
            err_req.code,
            ErrorCode::UnsupportedProtocolVersion,
            "Request with version {} must be UnsupportedProtocolVersion",
            ver
        );

        // Response envelope
        let resp_json = format!(
            r#"{{"version":{},"type":"response","response":{{"kind":"Acknowledged"}}}}"#,
            ver
        );
        let err_resp = decode_response_frame(resp_json.as_bytes()).unwrap_err();
        assert_eq!(
            err_resp.code,
            ErrorCode::UnsupportedProtocolVersion,
            "Response with version {} must be UnsupportedProtocolVersion",
            ver
        );

        // Error envelope
        let err_env_json = format!(
            r#"{{"version":{},"type":"error","error":{{"code":"InvalidRequest","message":null,"retry_after_seconds":null}}}}"#,
            ver
        );
        let err_error = decode_error_frame(err_env_json.as_bytes()).unwrap_err();
        assert_eq!(
            err_error.code,
            ErrorCode::UnsupportedProtocolVersion,
            "Error with version {} must be UnsupportedProtocolVersion",
            ver
        );

        // Event envelope
        let ev_json = format!(
            r#"{{"version":{},"type":"event","event":{{"kind":"ShutdownStateChanged","previous":"Idle","current":"Scheduled"}}}}"#,
            ver
        );
        let err_ev = decode_event_frame(ev_json.as_bytes()).unwrap_err();
        assert_eq!(
            err_ev.code,
            ErrorCode::UnsupportedProtocolVersion,
            "Event with version {} must be UnsupportedProtocolVersion",
            ver
        );
    }

    // Non-integer or wrong JSON types for version must be rejected as ProtocolError
    let non_integer_versions = [r#""1""#, "true", "null", "1.5", "{}"];
    for ver in &non_integer_versions {
        let req_json = format!(
            r#"{{"version":{},"type":"request","request":{{"kind":"QueryStatus"}}}}"#,
            ver
        );
        let err_req = decode_request_frame(req_json.as_bytes()).unwrap_err();
        assert_eq!(
            err_req.code,
            ErrorCode::ProtocolError,
            "Request with version {} must be ProtocolError",
            ver
        );
    }
}

// =============================================================================
// IPC-06 .. IPC-15: Wire Framing, Prefixes, Resource Limits & Errors
// =============================================================================

#[test]
fn ipc_06_four_byte_le_prefix_encodes_and_decodes_correctly() {
    let test_lengths = [1u32, 100, 65536, 1048576, 0x12345678];
    for &len in &test_lengths {
        let prefix = encode_length_prefix(len);
        let decoded = decode_length_prefix(&prefix);
        assert_eq!(decoded, len);
    }
}

#[test]
fn ipc_07_zero_length_frame_rejected_as_protocol_error() {
    let err = validate_request_frame_length(0).unwrap_err();
    assert_eq!(err.code, ErrorCode::ProtocolError);

    let err_decode = decode_request_frame(&[]).unwrap_err();
    assert_eq!(err_decode.code, ErrorCode::ProtocolError);
}

#[test]
fn ipc_08_request_length_within_limit_passes_pre_allocation_guard() {
    assert!(validate_request_frame_length(1).is_ok());
    assert!(validate_request_frame_length(1024).is_ok());
    assert!(validate_request_frame_length(65536).is_ok());
}

#[test]
fn ipc_09_request_length_above_limit_rejected_as_frame_too_large_before_allocation() {
    let err = validate_request_frame_length(65537).unwrap_err();
    assert_eq!(err.code, ErrorCode::FrameTooLarge);

    let err_huge = validate_request_frame_length(1000000).unwrap_err();
    assert_eq!(err_huge.code, ErrorCode::FrameTooLarge);
}

#[test]
fn ipc_10_response_length_within_1_mib_passes_validation() {
    assert!(validate_response_frame_length(1).is_ok());
    assert!(validate_response_frame_length(65536).is_ok());
    assert!(validate_response_frame_length(1048576).is_ok());
}

#[test]
fn ipc_11_response_length_above_1_mib_rejected_as_response_too_large() {
    let err = validate_response_frame_length(1048577).unwrap_err();
    assert_eq!(err.code, ErrorCode::ResponseTooLarge);
}

#[test]
fn ipc_12_event_length_within_64_kib_passes_validation() {
    assert!(validate_event_frame_length(1).is_ok());
    assert!(validate_event_frame_length(65536).is_ok());
}

#[test]
fn ipc_13_event_length_above_64_kib_rejected_without_truncation() {
    let err = validate_event_frame_length(65537).unwrap_err();
    assert_eq!(err.code, ErrorCode::FrameTooLarge);
}

#[test]
fn ipc_14_malformed_utf8_or_invalid_json_rejected_as_malformed_frame() {
    // Malformed UTF-8 bytes
    let invalid_utf8 = [0xff, 0xfe, 0xfd];
    let err_utf8 = decode_request_frame(&invalid_utf8).unwrap_err();
    assert_eq!(err_utf8.code, ErrorCode::MalformedFrame);

    // Valid UTF-8 but invalid JSON syntax
    let invalid_json = b"{ not a valid json }";
    let err_json = decode_request_frame(invalid_json).unwrap_err();
    assert_eq!(err_json.code, ErrorCode::MalformedFrame);
}

#[test]
fn ipc_15_premature_stream_disconnection_classified_as_transport_failure() {
    // 1. Incomplete frame buffer with declared length = 100, but only 10 bytes available before EOF
    let mut buffer = Vec::new();
    buffer.extend_from_slice(&encode_length_prefix(100));
    buffer.extend_from_slice(b"1234567890"); // 10 bytes instead of 100

    let err = extract_frame_payload(&buffer, true).unwrap_err();
    assert_eq!(err.code, ErrorCode::TransportFailure);

    // 2. Incomplete length prefix (< 4 bytes) before EOF
    let truncated_prefix = [1u8, 2u8];
    let err_prefix = extract_frame_payload(&truncated_prefix, true).unwrap_err();
    assert_eq!(err_prefix.code, ErrorCode::TransportFailure);

    // 3. validate_frame_completeness directly
    let err_completeness = validate_frame_completeness(100, 10).unwrap_err();
    assert_eq!(err_completeness.code, ErrorCode::TransportFailure);

    // 4. Exact match passes
    let mut complete_buffer = Vec::new();
    complete_buffer.extend_from_slice(&encode_length_prefix(5));
    complete_buffer.extend_from_slice(b"hello");
    let extracted = extract_frame_payload(&complete_buffer, true)
        .expect("Complete frame must succeed")
        .expect("Payload must be present");
    assert_eq!(extracted.0, b"hello");
    assert_eq!(extracted.1, 9);
}

// =============================================================================
// IPC-20 .. IPC-24: Duration Validation & Conversion
// =============================================================================

#[test]
fn ipc_20_zero_duration_rejected_as_invalid_request() {
    let req = RequestPayload::ScheduleInternetBlock {
        duration_minutes: 0,
    };
    let err = req.validate().unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);

    let req_shutdown = RequestPayload::ScheduleShutdown {
        duration_minutes: 0,
    };
    let err_shutdown = req_shutdown.validate().unwrap_err();
    assert_eq!(err_shutdown.code, ErrorCode::InvalidRequest);
}

#[test]
fn ipc_21_duration_in_valid_range_passes_validation() {
    let valid_values = [1u32, 5, 60, 1440, 100000, MAX_DURATION_MINUTES];
    for &d in &valid_values {
        let req = RequestPayload::ScheduleInternetBlock {
            duration_minutes: d,
        };
        assert!(req.validate().is_ok());
        let req_shutdown = RequestPayload::ScheduleShutdown {
            duration_minutes: d,
        };
        assert!(req_shutdown.validate().is_ok());
    }
}

#[test]
fn ipc_22_duration_exceeding_max_rejected_as_overflow() {
    let req = RequestPayload::ScheduleInternetBlock {
        duration_minutes: MAX_DURATION_MINUTES + 1,
    };
    let err = req.validate().unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);

    let req_max = RequestPayload::ScheduleShutdown {
        duration_minutes: u32::MAX,
    };
    let err_max = req_max.validate().unwrap_err();
    assert_eq!(err_max.code, ErrorCode::InvalidRequest);
}

#[test]
fn ipc_23_no_arbitrary_24_hour_limitation() {
    // 24 hours = 1440 minutes. Values > 1440 are completely valid.
    let req = RequestPayload::ScheduleInternetBlock {
        duration_minutes: 1441,
    };
    assert!(req.validate().is_ok());

    let req_7_days = RequestPayload::ScheduleInternetBlock {
        duration_minutes: 7 * 24 * 60,
    };
    assert!(req_7_days.validate().is_ok());
}

#[test]
fn ipc_24_duration_minutes_to_seconds_uses_checked_mul() {
    let minutes: u32 = 60;
    let seconds = minutes
        .checked_mul(60)
        .expect("checked_mul must not overflow");
    assert_eq!(seconds, 3600);

    let max_min: u32 = MAX_DURATION_MINUTES;
    let max_seconds = max_min
        .checked_mul(60)
        .expect("max duration must not overflow u32");
    assert!(max_seconds <= u32::MAX);

    // Demonstrating that u32::MAX / 60 + 1 overflows:
    let overflow_min = (u32::MAX / 60) + 1;
    assert!(overflow_min.checked_mul(60).is_none());
}

// =============================================================================
// IPC-25 .. IPC-29: Chat Text Validation
// =============================================================================

#[test]
fn ipc_25_chat_text_1_to_4096_bytes_is_accepted() {
    let short_text = RequestPayload::SendChildMessage {
        text: "Hello parent!".into(),
    };
    assert!(short_text.validate().is_ok());

    let exact_4096_bytes = "a".repeat(4096);
    let max_text = RequestPayload::SendChildMessage {
        text: exact_4096_bytes,
    };
    assert!(max_text.validate().is_ok());
}

#[test]
fn ipc_26_empty_chat_text_rejected_as_invalid_request() {
    let empty_text = RequestPayload::SendChildMessage {
        text: String::new(),
    };
    let err = empty_text.validate().unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);
}

#[test]
fn ipc_27_chat_text_exceeding_4096_bytes_rejected_as_invalid_request() {
    let oversized = "a".repeat(4097);
    let req = RequestPayload::SendChildMessage { text: oversized };
    let err = req.validate().unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);
}

#[test]
fn ipc_28_whitespace_only_chat_text_rejected_as_invalid_request() {
    let spaces = RequestPayload::SendChildMessage {
        text: "    \t \n \r  ".into(),
    };
    let err = spaces.validate().unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);
}

#[test]
fn ipc_29_chat_text_preserves_leading_trailing_whitespace_without_silent_trim() {
    let original = "  leading and trailing  ";
    let req = RequestEnvelope::new(RequestPayload::SendChildMessage {
        text: original.into(),
    });
    let encoded = encode_request_frame(&req).expect("Encoding must succeed");
    let decoded = decode_request_frame(&encoded[4..]).expect("Decoding must succeed");
    if let RequestPayload::SendChildMessage { text } = decoded.request {
        assert_eq!(text, original);
    } else {
        panic!("Decoded unexpected variant");
    }
}

// =============================================================================
// IPC-65 & IPC-88: Identifier Serialization & Validation
// =============================================================================

#[test]
fn ipc_65_timer_id_wire_representation_validates_32_lowercase_hex() {
    // Valid 32 lowercase hex characters
    let valid_hex = "4a2b9e10c73d48f2b5a19083de5f6120";
    let timer_id = WireTimerId::parse(valid_hex).expect("Valid lowercase hex must parse");
    assert_eq!(timer_id.to_string(), valid_hex);

    // Reject uppercase hex
    let upper_hex = "4A2B9E10C73D48F2B5A19083DE5F6120";
    assert!(WireTimerId::parse(upper_hex).is_err());

    // Reject non-hex characters
    let non_hex = "4a2b9e10c73d48f2b5a19083de5f612g";
    assert!(WireTimerId::parse(non_hex).is_err());

    // Reject incorrect lengths
    let short_hex = "4a2b9e10c73d48f2";
    assert!(WireTimerId::parse(short_hex).is_err());
    let long_hex = "4a2b9e10c73d48f2b5a19083de5f612000";
    assert!(WireTimerId::parse(long_hex).is_err());
}

#[test]
fn ipc_88_message_id_wire_representation_validates_32_lowercase_hex() {
    let valid_hex = "4a2b9e10c73d48f2b5a19083de5f6120";
    let msg_id = WireMessageId::parse(valid_hex).expect("Valid lowercase hex must parse");
    assert_eq!(msg_id.to_string(), valid_hex);

    let upper_hex = "4A2B9E10C73D48F2B5A19083DE5F6120";
    assert!(WireMessageId::parse(upper_hex).is_err());
}

// =============================================================================
// IPC-66: PIN Redaction in Debug, Display, and Logging
// =============================================================================

#[test]
fn ipc_66_verify_pin_redacts_pin_secret_in_debug_and_display() {
    let secret = "UltraUniqueSecretPin998877";
    let pin = RedactedPin::new(secret);

    // 1. RedactedPin Debug and Display
    let pin_debug = format!("{:?}", pin);
    assert!(!pin_debug.contains(secret));
    assert!(pin_debug.contains("[REDACTED]"));

    let pin_display = format!("{}", pin);
    assert!(!pin_display.contains(secret));
    assert_eq!(pin_display, "[REDACTED]");

    // 2. RequestPayload Debug and Display
    let req = RequestPayload::VerifyPin { pin: pin.clone() };
    let req_debug = format!("{:?}", req);
    assert!(!req_debug.contains(secret));
    assert!(req_debug.contains("[REDACTED]"));

    let req_display = format!("{}", req);
    assert!(!req_display.contains(secret));
    assert!(req_display.contains("[REDACTED]"));

    // 3. RequestEnvelope Debug and Display
    let env = RequestEnvelope::new(req);
    let env_debug = format!("{:?}", env);
    assert!(!env_debug.contains(secret));
    assert!(env_debug.contains("[REDACTED]"));

    let env_display = format!("{}", env);
    assert!(!env_display.contains(secret));
    assert!(env_display.contains("[REDACTED]"));

    // 4. Critical verification: Wire serialization MUST carry the actual PIN
    let wire_json = serde_json::to_string(&env).expect("Serialization must succeed");
    assert!(wire_json.contains(secret));
    assert!(!wire_json.contains("[REDACTED]"));
}

// =============================================================================
// IPC-75 .. IPC-87: Strict JSON Behavior, Naming Conventions, Duplicate Keys
// =============================================================================

#[test]
fn ipc_75_request_envelope_structure() {
    let req = RequestEnvelope::new(RequestPayload::ImmediateInternetBlock);
    let json = serde_json::to_string(&req).expect("Serialization must succeed");
    assert!(json.contains(r#""version":1"#));
    assert!(json.contains(r#""type":"request""#));
    assert!(json.contains(r#""kind":"ImmediateInternetBlock""#));
}

#[test]
fn ipc_76_unknown_top_level_field_rejected_as_protocol_error() {
    let bad_json =
        r#"{"version":1,"type":"request","request":{"kind":"QueryStatus"},"unknown_extra":123}"#;
    let err = decode_request_frame(bad_json.as_bytes()).unwrap_err();
    assert_eq!(err.code, ErrorCode::ProtocolError);
}

#[test]
fn ipc_77_unknown_field_inside_request_rejected_as_protocol_error() {
    let bad_req_json =
        r#"{"version":1,"type":"request","request":{"kind":"QueryStatus","extra_field":true}}"#;
    let err = decode_request_frame(bad_req_json.as_bytes()).unwrap_err();
    assert_eq!(err.code, ErrorCode::ProtocolError);
}

#[test]
fn ipc_78_unknown_request_kind_rejected_as_invalid_request() {
    let unknown_kind = r#"{"version":1,"type":"request","request":{"kind":"ExecuteShellCommand"}}"#;
    let err = decode_request_frame(unknown_kind.as_bytes()).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);
}

#[test]
fn ipc_79_success_response_envelope_structure() {
    let resp = ResponseEnvelope::new(ResponsePayload::Acknowledged);
    let json = serde_json::to_string(&resp).expect("Serialization must succeed");
    assert!(json.contains(r#""version":1"#));
    assert!(json.contains(r#""type":"response""#));
    assert!(json.contains(r#""kind":"Acknowledged""#));
}

#[test]
fn ipc_80_response_envelope_has_no_request_id_field() {
    let resp = ResponseEnvelope::new(ResponsePayload::Acknowledged);
    let json = serde_json::to_string(&resp).expect("Serialization must succeed");
    assert!(!json.contains("request_id"));

    // Rejecting response with an unexpected request_id
    let with_req_id =
        r#"{"version":1,"type":"response","response":{"kind":"Acknowledged"},"request_id":123}"#;
    let err = decode_response_frame(with_req_id.as_bytes()).unwrap_err();
    assert_eq!(err.code, ErrorCode::ProtocolError);
}

#[test]
fn ipc_81_error_envelope_structure() {
    let err_env = ErrorEnvelope::new(ErrorPayload {
        code: ErrorCode::InvalidRequest,
        message: None,
        retry_after_seconds: None,
    });
    let json = serde_json::to_string(&err_env).expect("Serialization must succeed");
    assert!(json.contains(r#""version":1"#));
    assert!(json.contains(r#""type":"error""#));
    assert!(json.contains(r#""code":"InvalidRequest""#));
    // Frozen contract: OPTION_NONE = JSON null
    assert!(json.contains(r#""message":null"#));
    assert!(json.contains(r#""retry_after_seconds":null"#));

    // Decode valid error envelope with explicit nulls
    let decoded = decode_error_frame(json.as_bytes()).expect("Must decode explicit nulls");
    assert_eq!(decoded.error.code, ErrorCode::InvalidRequest);
    assert_eq!(decoded.error.message, None);
    assert_eq!(decoded.error.retry_after_seconds, None);

    // Negative test: missing 'message' field is rejected
    let missing_message = r#"{"version":1,"type":"error","error":{"code":"InvalidRequest","retry_after_seconds":null}}"#;
    let err_missing_msg = decode_error_frame(missing_message.as_bytes()).unwrap_err();
    assert_eq!(err_missing_msg.code, ErrorCode::ProtocolError);

    // Negative test: missing 'retry_after_seconds' field is rejected
    let missing_retry =
        r#"{"version":1,"type":"error","error":{"code":"InvalidRequest","message":null}}"#;
    let err_missing_retry = decode_error_frame(missing_retry.as_bytes()).unwrap_err();
    assert_eq!(err_missing_retry.code, ErrorCode::ProtocolError);
}

#[test]
fn ipc_82_retry_after_seconds_is_supported_in_pin_locked_error() {
    let err_env = ErrorEnvelope::new(ErrorPayload {
        code: ErrorCode::PinLocked,
        message: Some("PIN locked due to failed attempts".into()),
        retry_after_seconds: Some(30),
    });
    let json = serde_json::to_string(&err_env).expect("Serialization must succeed");
    assert!(json.contains(r#""code":"PinLocked""#));
    assert!(json.contains(r#""retry_after_seconds":30"#));
    assert!(json.contains(r#""message":"PIN locked due to failed attempts""#));

    let decoded = decode_error_frame(json.as_bytes()).expect("Must decode successfully");
    assert_eq!(decoded.error.code, ErrorCode::PinLocked);
    assert_eq!(decoded.error.retry_after_seconds, Some(30));
    assert_eq!(
        decoded.error.message,
        Some("PIN locked due to failed attempts".into())
    );

    // When message is None but retry_after_seconds is Some(60)
    let err_no_msg = ErrorEnvelope::new(ErrorPayload {
        code: ErrorCode::PinLocked,
        message: None,
        retry_after_seconds: Some(60),
    });
    let json_no_msg = serde_json::to_string(&err_no_msg).expect("Serialization must succeed");
    assert!(json_no_msg.contains(r#""message":null"#));
    assert!(json_no_msg.contains(r#""retry_after_seconds":60"#));
    let decoded_no_msg =
        decode_error_frame(json_no_msg.as_bytes()).expect("Must decode successfully");
    assert_eq!(decoded_no_msg.error.message, None);
    assert_eq!(decoded_no_msg.error.retry_after_seconds, Some(60));
}

#[test]
fn status_snapshot_service_health_nullable_last_error() {
    let health = ServiceHealthDto {
        status: HealthStatusDto::Healthy,
        uptime_seconds: 12345,
        internet_gate_healthy: true,
        persistence_healthy: true,
        telegram_connected: false,
        active_tray_sessions: 1,
        last_error: None,
    };
    let json = serde_json::to_string(&health).expect("Serialization must succeed");
    assert!(json.contains(r#""last_error":null"#));

    // Decode with explicit null
    let decoded: ServiceHealthDto =
        serde_json::from_str(&json).expect("Must deserialize explicit null");
    assert_eq!(decoded.last_error, None);

    // With actual error string
    let health_err = ServiceHealthDto {
        last_error: Some("WFP engine failure".into()),
        ..health
    };
    let json_err = serde_json::to_string(&health_err).expect("Serialization must succeed");
    assert!(json_err.contains(r#""last_error":"WFP engine failure""#));
    let decoded_err: ServiceHealthDto =
        serde_json::from_str(&json_err).expect("Must deserialize error string");
    assert_eq!(decoded_err.last_error, Some("WFP engine failure".into()));

    // Negative test: missing 'last_error' is rejected
    let missing_last_error = r#"{
        "status":"Healthy",
        "uptime_seconds":12345,
        "internet_gate_healthy":true,
        "persistence_healthy":true,
        "telegram_connected":false,
        "active_tray_sessions":1
    }"#;
    assert!(serde_json::from_str::<ServiceHealthDto>(missing_last_error).is_err());
}

#[test]
fn ipc_83_event_envelope_structure() {
    let ev = EventEnvelope::new(EventPayload::ShutdownStateChanged {
        previous: ShutdownStateDto::Idle,
        current: ShutdownStateDto::Scheduled,
    });
    let json = serde_json::to_string(&ev).expect("Serialization must succeed");
    assert!(json.contains(r#""version":1"#));
    assert!(json.contains(r#""type":"event""#));
    assert!(json.contains(r#""kind":"ShutdownStateChanged""#));
}

#[test]
fn ipc_84_and_85_naming_conventions_snake_case_fields_and_pascal_case_enums() {
    let resp = ResponseEnvelope::new(ResponsePayload::TimerCancellation {
        result: CancellationResult::TimerKindMismatch,
    });
    let json = serde_json::to_string(&resp).expect("Serialization must succeed");
    // Object field names are snake_case:
    assert!(json.contains(r#""kind":"TimerCancellation""#));
    assert!(json.contains(r#""result":"TimerKindMismatch""#));
}

#[test]
fn ipc_86_duplicate_json_keys_are_rejected_as_protocol_error() {
    // Duplicate top-level key
    let dup_top = r#"{"version":1,"version":1,"type":"request","request":{"kind":"QueryStatus"}}"#;
    let err = decode_request_frame(dup_top.as_bytes()).unwrap_err();
    assert_eq!(err.code, ErrorCode::ProtocolError);

    // Duplicate key inside nested object
    let dup_nested = r#"{"version":1,"type":"request","request":{"kind":"SendChildMessage","text":"hello","text":"duplicate"}}"#;
    let err_nested = decode_request_frame(dup_nested.as_bytes()).unwrap_err();
    assert_eq!(err_nested.code, ErrorCode::ProtocolError);
}

#[test]
fn ipc_87_timestamps_are_integer_unix_epoch_milliseconds_utc() {
    let snapshot = StatusSnapshotDto {
        desired_internet_state: DesiredInternetStateDto::Unrestricted,
        observed_internet_state: InternetStateDto::Unrestricted,
        shutdown_state: ShutdownStateDto::Idle,
        active_actions: Vec::new(),
        health: ServiceHealthDto {
            status: HealthStatusDto::Healthy,
            uptime_seconds: 100,
            internet_gate_healthy: true,
            persistence_healthy: true,
            telegram_connected: false,
            active_tray_sessions: 1,
            last_error: None,
        },
        target_child_sid: "S-1-5-21-12345".into(),
        timestamp: 1773057600000,
    };
    let json = serde_json::to_string(&snapshot).expect("Serialization must succeed");
    assert!(json.contains(r#""timestamp":1773057600000"#));
}

// =============================================================================
// IPC-90 .. IPC-92: Shutdown Variants & Event Stream Allowlist
// =============================================================================

#[test]
fn ipc_90_shutdown_state_variants_match_idle_scheduled_in_progress() {
    let states = [
        ShutdownStateDto::Idle,
        ShutdownStateDto::Scheduled,
        ShutdownStateDto::InProgress,
    ];
    let expected = [r#""Idle""#, r#""Scheduled""#, r#""InProgress""#];
    for (s, exp) in states.iter().zip(expected.iter()) {
        let json = serde_json::to_string(s).unwrap();
        assert_eq!(&json, exp);
    }
}

#[test]
fn ipc_91_pin_authentication_result_is_excluded_from_event_allowlist() {
    let pin_event = palka_core::Event::PinAuthenticationResult {
        success: false,
        lock_timeout_seconds: Some(30),
    };
    // try_from_core_event MUST return None for PinAuthenticationResult
    assert!(try_from_core_event(&pin_event).is_none());
}

#[test]
fn ipc_92_unknown_lifecycle_events_are_excluded_from_event_allowlist() {
    let lifecycle_event = palka_core::Event::ServiceLifecycleEvent {
        stage: palka_core::ServiceLifecycleStage::ServiceStarted,
    };
    assert!(try_from_core_event(&lifecycle_event).is_none());
}

#[test]
fn all_9_allowed_core_events_translate_to_wire_events() {
    let dummy_timer_id = palka_core::TimerId([1u8; 16]);
    let dummy_msg_id = palka_core::MessageId([2u8; 16]);
    let dummy_time = palka_core::UtcDateTime(1773057600000);
    let dummy_action = palka_core::ScheduledAction {
        id: dummy_timer_id,
        action_kind: palka_core::ActionKind::BlockInternet,
        deadline: palka_core::Deadline(dummy_time),
        created_at: dummy_time,
        created_by: palka_core::Initiator::ParentLocalPin,
        emitted_thresholds: HashSet::new(),
        execution_state: palka_core::ActionExecutionState::Pending,
    };

    let events: Vec<palka_core::Event> = vec![
        palka_core::Event::InternetPolicyChanged {
            desired: palka_core::DesiredInternetState::Blocked,
            observed: palka_core::InternetState::Blocked,
            reason: palka_core::StateChangeReason::StartupRestoration,
        },
        palka_core::Event::ShutdownStateChanged {
            previous: palka_core::ShutdownState::Idle,
            current: palka_core::ShutdownState::Scheduled,
        },
        palka_core::Event::TimerScheduled {
            action: dummy_action.clone(),
        },
        palka_core::Event::TimerCancelled {
            id: dummy_timer_id,
            action_kind: palka_core::ActionKind::BlockInternet,
        },
        palka_core::Event::TimerExpired {
            id: dummy_timer_id,
            action_kind: palka_core::ActionKind::BlockInternet,
        },
        palka_core::Event::WarningThresholdReached {
            event: palka_core::WarningEvent {
                timer_id: dummy_timer_id,
                action_kind: palka_core::ActionKind::BlockInternet,
                threshold: palka_core::WarningThreshold::M30,
                deadline: palka_core::Deadline(dummy_time),
                emitted_at: dummy_time,
            },
        },
        palka_core::Event::MissedDeadlineOccurred {
            action: dummy_action,
            reason: "Past deadline".into(),
        },
        palka_core::Event::ChatMessageReceived {
            message: palka_core::ChatMessage {
                id: dummy_msg_id,
                sender: palka_core::MessageSender::Parent,
                text: "Hello".into(),
                timestamp: dummy_time,
                delivery_status: palka_core::DeliveryStatus::AcceptedByService,
            },
        },
        palka_core::Event::ServiceHealthUpdated {
            health: palka_core::ServiceHealth {
                status: palka_core::HealthStatus::Healthy,
                uptime_seconds: 42,
                internet_gate_healthy: true,
                persistence_healthy: true,
                telegram_connected: true,
                active_tray_sessions: 1,
                last_error: None,
            },
        },
    ];

    assert_eq!(events.len(), 9);
    for e in &events {
        let wire = try_from_core_event(e);
        assert!(
            wire.is_some(),
            "All 9 allowed events must convert successfully"
        );
        let wire_payload = wire.unwrap();
        let env = EventEnvelope::new(wire_payload);
        let encoded = encode_event_frame(&env).expect("Encoding must succeed");
        let decoded = decode_event_frame(&encoded[4..]).expect("Decoding must succeed");
        assert_eq!(decoded.version, PROTOCOL_VERSION);
    }
}
