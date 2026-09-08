//! Wire framing and resource limit constants for PALKA IPC V1.

/// Normative protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum payload length for client requests: exactly 64 KiB (65536 bytes).
pub const MAX_REQUEST_BYTES: usize = 65536;

/// Maximum payload length for service responses: exactly 1 MiB (1048576 bytes).
pub const MAX_RESPONSE_BYTES: usize = 1048576;

/// Maximum payload length for event stream frames: exactly 64 KiB (65536 bytes).
pub const MAX_EVENT_BYTES: usize = 65536;

/// Maximum length of UTF-8 encoded child chat message text: exactly 4 KiB (4096 bytes).
pub const MAX_CHAT_TEXT_UTF8_BYTES: usize = 4096;

/// Maximum duration in minutes supported by checked 60-second timer conversion:
/// floor((u32::MAX - 60) / 60) = 71582788 minutes (~136.2 years).
pub const MAX_DURATION_MINUTES: u32 = 71582788;
