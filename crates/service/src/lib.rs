//! Service daemon library for PALKA.

pub mod persistence;
pub mod state_store;

pub use persistence::{
    InternetRetry, OutboxEntryId, PersistenceError, PersistentState, STATE_SCHEMA_VERSION_V1,
    TelegramOutboxEntry, TelegramPayload, decode_state_json, decode_state_json_bytes,
    encode_state_json, encode_state_json_pretty,
};
pub use state_store::{StateFileStore, StateStoreError};
