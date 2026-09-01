use super::{DomainError, RepositoryObject};
use std::fmt;
use std::str::FromStr;

/// The version of the common immutable-object envelope.
pub const CURRENT_OBJECT_ENVELOPE_VERSION: u16 = 1;

/// The current payload version for tree objects.
pub const CURRENT_TREE_OBJECT_VERSION: u16 = 1;

/// The current payload version for pack objects.
pub const CURRENT_PACK_OBJECT_VERSION: u16 = 1;

/// The current payload version for pack-index objects.
pub const CURRENT_INDEX_OBJECT_VERSION: u16 = 1;

/// The length of a SHA-256 object identifier in hexadecimal bytes.
pub const OBJECT_ID_HEX_LENGTH: usize = 64;

/// The largest canonical plaintext accepted by the immutable-object format.
pub const MAX_IMMUTABLE_OBJECT_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// The largest complete immutable object accepted by the format decoder.
pub const MAX_IMMUTABLE_OBJECT_BYTES: usize = MAX_IMMUTABLE_OBJECT_PAYLOAD_BYTES + 4 * 1024;

/// The kind discriminator used by an immutable repository object.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ObjectKind {
    /// A compact snapshot header.
    Snapshot,
    /// A directory tree.
    Tree,
    /// A pack containing chunk payloads.
    Pack,
    /// An index for a pack.
    Index,
}

impl ObjectKind {
    /// Returns the stable wire discriminator.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Tree => "tree",
            Self::Pack => "pack",
            Self::Index => "index",
        }
    }

    /// Parses a stable wire discriminator.
    pub fn parse(value: &str) -> Option<Self> {
        Self::parse_wire(value)
    }

    /// Parses a stable wire discriminator.
    fn parse_wire(value: &str) -> Option<Self> {
        match value {
            "snapshot" => Some(Self::Snapshot),
            "tree" => Some(Self::Tree),
            "pack" => Some(Self::Pack),
            "index" => Some(Self::Index),
            _ => None,
        }
    }

    /// Returns the payload version understood for this kind by this release.
    pub const fn current_version(self) -> u16 {
        match self {
            Self::Snapshot => crate::domain::CURRENT_SNAPSHOT_VERSION,
            Self::Tree => CURRENT_TREE_OBJECT_VERSION,
            Self::Pack => CURRENT_PACK_OBJECT_VERSION,
            Self::Index => CURRENT_INDEX_OBJECT_VERSION,
        }
    }

    /// Returns the logical storage prefix for this kind.
    pub const fn storage_prefix(self) -> &'static str {
        match self {
            Self::Snapshot => crate::domain::SNAPSHOT_OBJECT_PREFIX,
            Self::Tree => "trees",
            Self::Pack => "packs",
            Self::Index => "indexes",
        }
    }

    /// Builds the conventional immutable object reference for an ID.
    pub fn object_reference(self, id: &ObjectId) -> Result<RepositoryObject, DomainError> {
        RepositoryObject::new(format!("{}/{}", self.storage_prefix(), id.as_str()))
    }
}

impl FromStr for ObjectKind {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse_wire(value).ok_or(())
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The codec recorded in an immutable-object envelope.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ObjectCodec {
    /// The payload is already canonical plaintext.
    None,
    /// Zstandard-compressed payloads reserved for a future decoder.
    Zstd,
}

impl ObjectCodec {
    /// Returns the stable wire discriminator.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Zstd => "zstd",
        }
    }

    /// Parses a stable wire discriminator.
    pub fn parse(value: &str) -> Option<Self> {
        Self::parse_wire(value)
    }

    /// Parses a stable wire discriminator.
    fn parse_wire(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "zstd" => Some(Self::Zstd),
            _ => None,
        }
    }
}

impl FromStr for ObjectCodec {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse_wire(value).ok_or(())
    }
}

impl fmt::Display for ObjectCodec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The encryption scheme recorded in an immutable-object envelope.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ObjectEncryption {
    /// The payload is not encrypted.
    None,
    /// XChaCha20-Poly1305 encrypted payloads reserved for a future decoder.
    XChaCha20Poly1305,
}

impl ObjectEncryption {
    /// Returns the stable wire discriminator.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::XChaCha20Poly1305 => "xchacha20-poly1305",
        }
    }

    /// Parses a stable wire discriminator.
    pub fn parse(value: &str) -> Option<Self> {
        Self::parse_wire(value)
    }

    /// Parses a stable wire discriminator.
    fn parse_wire(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "xchacha20-poly1305" => Some(Self::XChaCha20Poly1305),
            _ => None,
        }
    }
}

impl FromStr for ObjectEncryption {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse_wire(value).ok_or(())
    }
}

impl fmt::Display for ObjectEncryption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A canonical SHA-256 content identifier for an immutable object.
///
/// The hexadecimal representation is always lowercase. An object ID is a
/// domain value, not a storage version token and not an encrypted payload
/// digest. Its digest input is the fixed UTF-8 domain separator
/// `GIB immutable object identity`, a NUL byte, the kind discriminator, a NUL
/// byte, the object version in big-endian bytes, and the canonical plaintext
/// payload. Transport metadata is excluded.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectId {
    hex: String,
    digest: [u8; 32],
}

impl ObjectId {
    /// Creates an ID from a SHA-256 digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self {
            hex: hex_encode(&digest),
            digest,
        }
    }

    /// Parses a hexadecimal SHA-256 ID and normalizes it to lowercase.
    pub fn from_hex(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        if value.len() != OBJECT_ID_HEX_LENGTH || !value.is_ascii() {
            return Err(DomainError::InvalidObjectId {
                reason: "must contain exactly 64 hexadecimal bytes",
            });
        }
        let mut digest = [0u8; 32];
        for (index, slot) in digest.iter_mut().enumerate() {
            let offset = index * 2;
            let pair = &value.as_bytes()[offset..offset + 2];
            let Some(high) = hex_value(pair[0]) else {
                return Err(DomainError::InvalidObjectId {
                    reason: "must contain exactly 64 hexadecimal bytes",
                });
            };
            let Some(low) = hex_value(pair[1]) else {
                return Err(DomainError::InvalidObjectId {
                    reason: "must contain exactly 64 hexadecimal bytes",
                });
            };
            *slot = (high << 4) | low;
        }
        Ok(Self::from_digest(digest))
    }

    /// Returns the canonical lowercase hexadecimal ID.
    pub fn as_str(&self) -> &str {
        &self.hex
    }

    /// Returns the raw SHA-256 digest bytes.
    pub const fn as_digest(&self) -> &[u8; 32] {
        &self.digest
    }

    /// Returns the conventional object reference for this ID and kind.
    pub fn object_reference(&self, kind: ObjectKind) -> Result<RepositoryObject, DomainError> {
        kind.object_reference(self)
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl AsRef<str> for ObjectId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<&str> for ObjectId {
    type Error = DomainError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::from_hex(value)
    }
}

/// A validated immutable object returned after envelope authentication.
///
/// This is a domain value containing canonical plaintext. It is deliberately
/// separate from the private MessagePack wire model used to persist it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImmutableObject {
    kind: ObjectKind,
    version: u16,
    codec: ObjectCodec,
    encryption: ObjectEncryption,
    plaintext_length: u64,
    payload_length: u64,
    object_id: ObjectId,
    payload: Vec<u8>,
}

pub(crate) struct ImmutableObjectParts {
    pub(crate) kind: ObjectKind,
    pub(crate) version: u16,
    pub(crate) codec: ObjectCodec,
    pub(crate) encryption: ObjectEncryption,
    pub(crate) plaintext_length: u64,
    pub(crate) payload_length: u64,
    pub(crate) object_id: ObjectId,
    pub(crate) payload: Vec<u8>,
}

impl ImmutableObject {
    pub(crate) fn from_validated_parts(parts: ImmutableObjectParts) -> Self {
        Self {
            kind: parts.kind,
            version: parts.version,
            codec: parts.codec,
            encryption: parts.encryption,
            plaintext_length: parts.plaintext_length,
            payload_length: parts.payload_length,
            object_id: parts.object_id,
            payload: parts.payload,
        }
    }

    /// Returns the validated object kind.
    pub const fn kind(&self) -> ObjectKind {
        self.kind
    }

    /// Returns the explicit payload format version.
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Returns the recorded codec.
    pub const fn codec(&self) -> ObjectCodec {
        self.codec
    }

    /// Returns the recorded encryption scheme.
    pub const fn encryption(&self) -> ObjectEncryption {
        self.encryption
    }

    /// Returns the canonical plaintext length.
    pub const fn plaintext_length(&self) -> u64 {
        self.plaintext_length
    }

    /// Returns the stored payload length.
    pub const fn payload_length(&self) -> u64 {
        self.payload_length
    }

    /// Returns the authenticated content ID.
    pub fn object_id(&self) -> &ObjectId {
        &self.object_id
    }

    /// Returns the canonical plaintext payload.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Consumes the value and returns its canonical plaintext payload.
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }
}

fn hex_encode(value: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(OBJECT_ID_HEX_LENGTH);
    for byte in value {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_ids_normalize_hex_without_changing_the_digest() {
        let lowercase = ObjectId::from_hex("ab".repeat(32)).expect("hex ID should parse");
        let uppercase = ObjectId::from_hex("AB".repeat(32)).expect("hex ID should parse");
        assert_eq!(lowercase, uppercase);
        assert_eq!(lowercase.as_ref().len(), OBJECT_ID_HEX_LENGTH);
        assert_eq!(lowercase.as_digest(), &[0xab; 32]);
    }

    #[test]
    fn kinds_have_stable_prefixes_and_versions() {
        assert_eq!(ObjectKind::Snapshot.storage_prefix(), "snapshots");
        assert_eq!(ObjectKind::Tree.storage_prefix(), "trees");
        assert_eq!(ObjectKind::Pack.storage_prefix(), "packs");
        assert_eq!(ObjectKind::Index.storage_prefix(), "indexes");
        assert_eq!(ObjectKind::parse("tree"), Some(ObjectKind::Tree));
        assert_eq!(ObjectKind::parse("future"), None);
        assert_eq!(ObjectKind::Tree.current_version(), 1);
    }
}
