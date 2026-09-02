//! Service daemon library for PALKA.

pub mod config_persistence;
pub mod config_store;
pub mod persistence;
pub mod state_store;

pub use config_persistence::{
    CONFIG_SCHEMA_VERSION_V1, ConfigPersistenceError, PersistentConfig, decode_config_json,
    decode_config_json_bytes, encode_config_json, encode_config_json_pretty,
};
pub use config_store::{ConfigFileStore, ConfigStoreError};
pub use persistence::{
    InternetRetry, OutboxEntryId, PersistenceError, PersistentState, STATE_SCHEMA_VERSION_V1,
    TelegramOutboxEntry, TelegramPayload, decode_state_json, decode_state_json_bytes,
    encode_state_json, encode_state_json_pretty,
};
pub use state_store::{StateFileStore, StateStoreError};
