mod configuration;
mod envelope;
mod identity;
mod repository;
mod storage_configuration;

pub(crate) use configuration::{
    ConfigurationDocumentError, ConfigurationDocumentErrorKind, MAX_CONFIGURATION_BYTES,
    PersistedConfiguration, parse_configuration_document,
};
pub(crate) use envelope::{
    calculate_object_id, decode_object_envelope, decode_object_envelope_from_reader,
    encode_object_envelope,
};
pub(crate) use identity::{
    CURRENT_IDENTITY_CONFIGURATION_VERSION, MAX_IDENTITY_CONFIGURATION_BYTES,
    decode_identity_configuration, encode_identity_configuration,
};
pub(crate) use repository::{
    FormatError, decode_bootstrap, decode_descriptor, decode_head, decode_history_record,
    decode_snapshot, encode_bootstrap, encode_descriptor, encode_head, encode_history_record,
    encode_snapshot, snapshot_object_id,
};
pub(crate) use storage_configuration::{
    DecodedStorageConfiguration, PersistedStorageBackend, decode_storage_configuration,
    encode_storage_configuration,
};
