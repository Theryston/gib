use crate::domain::{
    CURRENT_REPOSITORY_DESCRIPTOR_VERSION, CURRENT_REPOSITORY_FORMAT_VERSION, DomainError,
    REPOSITORY_DESCRIPTOR_OBJECT_KEY, REPOSITORY_MAGIC, REQUIRED_REPOSITORY_FEATURE,
    RepositoryDescriptor, RepositoryFeature, RepositoryIdentity, RepositoryKey, RepositoryObject,
    RepositoryRoots,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FormatError {
    Serialization,
    InvalidEncoding,
    InvalidMagic,
    InvalidRootReference,
    InvalidField,
    MissingRequiredFeature,
    UnsupportedRequiredFeature,
    UnsupportedVersion { version: u16 },
    VersionMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedFormatMarker {
    pub(crate) version: u16,
}

#[derive(Serialize)]
struct FormatMarkerWire<'a> {
    magic: &'a str,
    format_version: u16,
    descriptor: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FormatMarkerWireOwned {
    magic: String,
    format_version: u16,
    descriptor: String,
}

#[derive(Serialize)]
struct DescriptorWire<'a> {
    descriptor_version: u16,
    magic: &'a str,
    format_version: u16,
    repository_id: &'a str,
    repository_key: &'a str,
    required_features: Vec<&'a str>,
    roots: RootsWire<'a>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DescriptorWireOwned {
    descriptor_version: u16,
    magic: String,
    format_version: u16,
    repository_id: String,
    repository_key: String,
    required_features: Vec<String>,
    roots: RootsWireOwned,
}

#[derive(Serialize)]
struct RootsWire<'a> {
    format: &'a str,
    descriptor: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RootsWireOwned {
    format: String,
    descriptor: String,
}

pub(crate) fn encode_format_marker() -> Result<Vec<u8>, FormatError> {
    serde_json::to_vec(&FormatMarkerWire {
        magic: REPOSITORY_MAGIC,
        format_version: CURRENT_REPOSITORY_FORMAT_VERSION,
        descriptor: REPOSITORY_DESCRIPTOR_OBJECT_KEY,
    })
    .map_err(|_| FormatError::Serialization)
}

pub(crate) fn decode_format_marker(bytes: &[u8]) -> Result<ValidatedFormatMarker, FormatError> {
    if bytes.is_empty() {
        return Err(FormatError::InvalidEncoding);
    }
    let marker: FormatMarkerWireOwned =
        serde_json::from_slice(bytes).map_err(|_| FormatError::InvalidEncoding)?;
    if marker.magic != REPOSITORY_MAGIC {
        return Err(FormatError::InvalidMagic);
    }
    if marker.format_version != CURRENT_REPOSITORY_FORMAT_VERSION {
        return Err(FormatError::UnsupportedVersion {
            version: marker.format_version,
        });
    }
    if marker.descriptor != REPOSITORY_DESCRIPTOR_OBJECT_KEY {
        return Err(FormatError::InvalidRootReference);
    }
    Ok(ValidatedFormatMarker {
        version: marker.format_version,
    })
}

pub(crate) fn encode_descriptor(descriptor: &RepositoryDescriptor) -> Result<Vec<u8>, FormatError> {
    let required_features = descriptor
        .required_features()
        .iter()
        .map(|feature| feature.as_str())
        .collect();
    serde_json::to_vec(&DescriptorWire {
        descriptor_version: descriptor.descriptor_version(),
        magic: REPOSITORY_MAGIC,
        format_version: descriptor.format_version(),
        repository_id: descriptor.identity().as_str(),
        repository_key: descriptor.repository_key().as_str(),
        required_features,
        roots: RootsWire {
            format: descriptor.roots().format().as_str(),
            descriptor: descriptor.roots().descriptor().as_str(),
        },
    })
    .map_err(|_| FormatError::Serialization)
}

pub(crate) fn decode_descriptor(
    bytes: &[u8],
    marker_version: u16,
) -> Result<RepositoryDescriptor, FormatError> {
    if bytes.is_empty() {
        return Err(FormatError::InvalidEncoding);
    }
    let wire: DescriptorWireOwned =
        serde_json::from_slice(bytes).map_err(|_| FormatError::InvalidEncoding)?;
    if wire.descriptor_version != CURRENT_REPOSITORY_DESCRIPTOR_VERSION {
        return Err(FormatError::UnsupportedVersion {
            version: wire.descriptor_version,
        });
    }
    if wire.magic != REPOSITORY_MAGIC {
        return Err(FormatError::InvalidMagic);
    }
    if wire.format_version != CURRENT_REPOSITORY_FORMAT_VERSION
        || wire.format_version != marker_version
    {
        if wire.format_version != CURRENT_REPOSITORY_FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion {
                version: wire.format_version,
            });
        }
        return Err(FormatError::VersionMismatch);
    }

    let identity = RepositoryIdentity::new(wire.repository_id).map_err(map_domain_error)?;
    let repository_key = RepositoryKey::new(wire.repository_key).map_err(map_domain_error)?;

    let mut required_features = Vec::with_capacity(wire.required_features.len());
    for feature in wire.required_features {
        let Some(feature) = RepositoryFeature::from_str(&feature) else {
            return Err(FormatError::UnsupportedRequiredFeature);
        };
        if required_features.contains(&feature) {
            return Err(FormatError::InvalidField);
        }
        required_features.push(feature);
    }
    if !required_features
        .iter()
        .any(|feature| feature.as_str() == REQUIRED_REPOSITORY_FEATURE)
    {
        return Err(FormatError::MissingRequiredFeature);
    }

    let format = RepositoryObject::new(wire.roots.format).map_err(map_domain_error)?;
    let descriptor = RepositoryObject::new(wire.roots.descriptor).map_err(map_domain_error)?;
    let roots = RepositoryRoots::new(format, descriptor).map_err(map_domain_error)?;

    Ok(RepositoryDescriptor::from_validated_parts(
        CURRENT_REPOSITORY_DESCRIPTOR_VERSION,
        CURRENT_REPOSITORY_FORMAT_VERSION,
        identity,
        repository_key,
        required_features,
        roots,
    ))
}

fn map_domain_error(error: DomainError) -> FormatError {
    match error {
        DomainError::InvalidRepositoryIdentity { .. }
        | DomainError::InvalidRepositoryKey { .. }
        | DomainError::InvalidRepositoryObject { .. } => FormatError::InvalidField,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_wire_models_round_trip_into_domain_models() {
        let identity = RepositoryIdentity::new("fixture-repository");
        let key = RepositoryKey::new("default");
        assert!(identity.is_ok());
        assert!(key.is_ok());
        let descriptor =
            RepositoryDescriptor::new(identity.unwrap_or_default(), key.unwrap_or_default());

        let marker = encode_format_marker();
        let encoded = encode_descriptor(&descriptor);
        assert!(marker.is_ok());
        assert!(encoded.is_ok());
        let marker = decode_format_marker(&marker.unwrap_or_default());
        assert!(marker.is_ok());
        let decoded = decode_descriptor(
            &encoded.unwrap_or_default(),
            marker.map_or(0, |m| m.version),
        );
        assert_eq!(decoded, Ok(descriptor));
    }

    #[test]
    fn an_unknown_format_version_is_not_treated_as_a_legacy_descriptor() {
        let bytes = br#"{"magic":"GIB","format_version":99,"descriptor":"config/repository"}"#;
        assert_eq!(
            decode_format_marker(bytes),
            Err(FormatError::UnsupportedVersion { version: 99 })
        );
    }
}
