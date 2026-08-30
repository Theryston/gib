use std::fmt;

/// The repository format supported by this SDK release.
pub const CURRENT_REPOSITORY_FORMAT_VERSION: u16 = 1;

/// The version of the persisted repository descriptor schema.
pub const CURRENT_REPOSITORY_DESCRIPTOR_VERSION: u16 = 1;

/// Magic value written to every 0.1 repository root object.
pub const REPOSITORY_MAGIC: &str = "GIB";

/// Logical object containing the repository format marker.
pub const FORMAT_OBJECT_KEY: &str = "format";

/// Logical object containing the repository descriptor.
pub const REPOSITORY_DESCRIPTOR_OBJECT_KEY: &str = "config/repository";

/// Required feature flag for the first repository lifecycle format.
pub const REQUIRED_REPOSITORY_FEATURE: &str = "repository.lifecycle.v1";

/// A validation failure for a pure repository-domain value.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DomainError {
    /// The repository identity is not a valid stable identifier.
    InvalidRepositoryIdentity {
        /// The stable reason for rejecting the identity.
        reason: &'static str,
    },
    /// The repository key is not a valid namespace identifier.
    InvalidRepositoryKey {
        /// The stable reason for rejecting the key.
        reason: &'static str,
    },
    /// A persisted object reference is not a safe relative object key.
    InvalidRepositoryObject {
        /// The stable reason for rejecting the object reference.
        reason: &'static str,
    },
}

impl DomainError {
    /// Returns the stable validation reason.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::InvalidRepositoryIdentity { reason }
            | Self::InvalidRepositoryKey { reason }
            | Self::InvalidRepositoryObject { reason } => reason,
        }
    }
}

impl fmt::Display for DomainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRepositoryIdentity { reason } => {
                write!(formatter, "invalid repository identity: {reason}")
            }
            Self::InvalidRepositoryKey { reason } => {
                write!(formatter, "invalid repository key: {reason}")
            }
            Self::InvalidRepositoryObject { reason } => {
                write!(formatter, "invalid repository object reference: {reason}")
            }
        }
    }
}

impl std::error::Error for DomainError {}

/// A validated identity for one durable repository.
///
/// The identity is an ASCII, path-safe identifier. It is persisted in the
/// descriptor and is distinct from the repository key, which identifies the
/// caller's namespace within the repository.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepositoryIdentity(String);

impl RepositoryIdentity {
    /// The largest accepted identity length in UTF-8 bytes.
    pub const MAX_LENGTH: usize = 128;

    /// Creates an identity after validating its stable representation.
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        validate_identifier(
            &value,
            Self::MAX_LENGTH,
            DomainError::InvalidRepositoryIdentity {
                reason: "must contain 1 to 128 ASCII identifier bytes",
            },
        )?;
        Ok(Self(value))
    }

    /// Creates an identity from 16 bytes using lowercase hexadecimal form.
    pub fn from_bytes(value: [u8; 16]) -> Self {
        Self(hex_encode(&value))
    }

    /// Parses a lowercase or uppercase hexadecimal identity.
    pub fn from_hex(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(DomainError::InvalidRepositoryIdentity {
                reason: "hex identity must contain exactly 32 hexadecimal bytes",
            });
        }
        Self::new(value.to_ascii_lowercase())
    }

    /// Returns the canonical identifier representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the identity and returns its canonical representation.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl Default for RepositoryIdentity {
    fn default() -> Self {
        Self(String::from("default"))
    }
}

impl fmt::Display for RepositoryIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for RepositoryIdentity {
    type Error = DomainError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for RepositoryIdentity {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Short compatibility name for [`RepositoryIdentity`].
pub type RepositoryId = RepositoryIdentity;

/// A validated namespace key for one repository's logical data set.
///
/// This is an identifier, not an encryption secret. It is safe to persist and
/// display as repository metadata, and it cannot contain path separators or
/// platform-specific path syntax.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepositoryKey(String);

impl RepositoryKey {
    /// The largest accepted repository-key length in UTF-8 bytes.
    pub const MAX_LENGTH: usize = 64;

    /// Creates a repository key after validating its namespace representation.
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        validate_identifier(
            &value,
            Self::MAX_LENGTH,
            DomainError::InvalidRepositoryKey {
                reason: "must contain 1 to 64 ASCII identifier bytes",
            },
        )?;
        Ok(Self(value))
    }

    /// Returns the default namespace key.
    pub fn default_key() -> Self {
        Self(String::from("default"))
    }

    /// Returns the canonical key representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the key and returns its canonical representation.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl Default for RepositoryKey {
    fn default() -> Self {
        Self::default_key()
    }
}

impl fmt::Display for RepositoryKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for RepositoryKey {
    type Error = DomainError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for RepositoryKey {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// A validated relative object reference inside a repository storage.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepositoryObject(String);

impl RepositoryObject {
    /// The largest accepted logical object-key length in UTF-8 bytes.
    pub const MAX_LENGTH: usize = 512;

    /// Creates a safe relative object reference.
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        if value.is_empty() || value.len() > Self::MAX_LENGTH {
            return Err(DomainError::InvalidRepositoryObject {
                reason: "must contain 1 to 512 ASCII path bytes",
            });
        }
        if value.starts_with('/')
            || value.ends_with('/')
            || value.contains("//")
            || value.contains('\\')
            || value.contains(':')
            || value.contains('\0')
        {
            return Err(DomainError::InvalidRepositoryObject {
                reason: "must be a relative slash-separated object key",
            });
        }
        for component in value.split('/') {
            if component.is_empty() || component == "." || component == ".." {
                return Err(DomainError::InvalidRepositoryObject {
                    reason: "must not contain empty, dot, or parent path components",
                });
            }
            if !component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            {
                return Err(DomainError::InvalidRepositoryObject {
                    reason: "components must contain only ASCII letters, digits, dot, underscore, or hyphen",
                });
            }
        }
        Ok(Self(value))
    }

    /// Returns the canonical logical object key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepositoryObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The root object references required by the first repository format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryRoots {
    format: RepositoryObject,
    descriptor: RepositoryObject,
}

impl RepositoryRoots {
    /// Creates root references after validating their object-key syntax.
    pub fn new(
        format: RepositoryObject,
        descriptor: RepositoryObject,
    ) -> Result<Self, DomainError> {
        if format.as_str() != FORMAT_OBJECT_KEY
            || descriptor.as_str() != REPOSITORY_DESCRIPTOR_OBJECT_KEY
        {
            return Err(DomainError::InvalidRepositoryObject {
                reason: "repository roots must reference format and config/repository",
            });
        }
        Ok(Self { format, descriptor })
    }

    pub(crate) fn current() -> Self {
        Self {
            format: RepositoryObject(String::from(FORMAT_OBJECT_KEY)),
            descriptor: RepositoryObject(String::from(REPOSITORY_DESCRIPTOR_OBJECT_KEY)),
        }
    }

    /// Returns the format marker object reference.
    pub fn format(&self) -> &RepositoryObject {
        &self.format
    }

    /// Returns the repository descriptor object reference.
    pub fn descriptor(&self) -> &RepositoryObject {
        &self.descriptor
    }
}

/// A feature required by a repository descriptor.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RepositoryFeature {
    /// The repository lifecycle and descriptor contract introduced in 0.1.0.
    RepositoryLifecycleV1,
}

impl RepositoryFeature {
    /// Returns the persisted feature-flag value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepositoryLifecycleV1 => REQUIRED_REPOSITORY_FEATURE,
        }
    }

    pub(crate) fn from_str(value: &str) -> Option<Self> {
        match value {
            REQUIRED_REPOSITORY_FEATURE => Some(Self::RepositoryLifecycleV1),
            _ => None,
        }
    }
}

/// The validated domain representation of a repository descriptor.
///
/// This type is intentionally separate from the JSON wire model. Only a
/// descriptor that has passed all format and domain checks can be constructed
/// from persisted bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryDescriptor {
    descriptor_version: u16,
    format_version: u16,
    identity: RepositoryIdentity,
    repository_key: RepositoryKey,
    required_features: Vec<RepositoryFeature>,
    roots: RepositoryRoots,
}

impl RepositoryDescriptor {
    /// Creates the minimum valid 0.1.0 descriptor for an identity and key.
    pub fn new(identity: RepositoryIdentity, repository_key: RepositoryKey) -> Self {
        Self {
            descriptor_version: CURRENT_REPOSITORY_DESCRIPTOR_VERSION,
            format_version: CURRENT_REPOSITORY_FORMAT_VERSION,
            identity,
            repository_key,
            required_features: vec![RepositoryFeature::RepositoryLifecycleV1],
            roots: RepositoryRoots::current(),
        }
    }

    pub(crate) fn from_validated_parts(
        descriptor_version: u16,
        format_version: u16,
        identity: RepositoryIdentity,
        repository_key: RepositoryKey,
        required_features: Vec<RepositoryFeature>,
        roots: RepositoryRoots,
    ) -> Self {
        Self {
            descriptor_version,
            format_version,
            identity,
            repository_key,
            required_features,
            roots,
        }
    }

    /// Returns the descriptor schema version.
    pub const fn descriptor_version(&self) -> u16 {
        self.descriptor_version
    }

    /// Returns the repository format version.
    pub const fn format_version(&self) -> u16 {
        self.format_version
    }

    /// Returns the repository identity.
    pub fn identity(&self) -> &RepositoryIdentity {
        &self.identity
    }

    /// Alias for [`Self::identity`] using the repository-ID terminology.
    pub fn repository_id(&self) -> &RepositoryIdentity {
        self.identity()
    }

    /// Returns the repository namespace key.
    pub fn repository_key(&self) -> &RepositoryKey {
        &self.repository_key
    }

    /// Returns all required feature flags.
    pub fn required_features(&self) -> &[RepositoryFeature] {
        &self.required_features
    }

    /// Returns the required root object references.
    pub fn roots(&self) -> &RepositoryRoots {
        &self.roots
    }

    /// Returns whether the descriptor requires the supplied feature.
    pub fn requires_feature(&self, feature: RepositoryFeature) -> bool {
        self.required_features.contains(&feature)
    }
}

fn validate_identifier(
    value: &str,
    max_length: usize,
    error: DomainError,
) -> Result<(), DomainError> {
    if value.is_empty() || value.len() > max_length || !value.is_ascii() {
        return Err(error);
    }
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return Err(error);
    };
    if !first.is_ascii_alphanumeric()
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(error);
    }
    Ok(())
}

fn hex_encode(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_key_accepts_only_a_safe_namespace_identifier() {
        assert!(RepositoryKey::new("workstation-1").is_ok());
        assert!(RepositoryKey::new("with.dot_and-dash").is_ok());
        assert!(RepositoryKey::new("").is_err());
        assert!(RepositoryKey::new("../outside").is_err());
        assert!(RepositoryKey::new("has/slash").is_err());
        assert!(RepositoryKey::new("has space").is_err());
        assert!(RepositoryKey::new("é").is_err());
    }

    #[test]
    fn repository_identity_from_bytes_is_stable_hex() {
        let identity = RepositoryIdentity::from_bytes([0xab; 16]);
        assert_eq!(identity.as_str(), "abababababababababababababababab");
        assert_eq!(
            RepositoryIdentity::from_hex(identity.as_str()),
            Ok(identity)
        );
    }

    #[test]
    fn repository_object_rejects_traversal_and_platform_syntax() {
        assert!(RepositoryObject::new("config/repository").is_ok());
        assert!(RepositoryObject::new("../repository").is_err());
        assert!(RepositoryObject::new("config\\repository").is_err());
        assert!(RepositoryObject::new("C:repository").is_err());
    }
}
