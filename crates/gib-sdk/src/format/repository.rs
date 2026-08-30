use crate::domain::{
    CURRENT_REPOSITORY_BOOTSTRAP_VERSION, CURRENT_REPOSITORY_DESCRIPTOR_VERSION,
    CURRENT_REPOSITORY_FORMAT_VERSION, DomainError, REPOSITORY_DESCRIPTOR_OBJECT_KEY,
    REPOSITORY_MAGIC, REQUIRED_REPOSITORY_FEATURE, RepositoryDescriptor, RepositoryFeature,
    RepositoryIdentity, RepositoryKey, RepositoryObject, RepositoryRoots,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::Cursor;

pub(crate) const MAX_BOOTSTRAP_BYTES: usize = 1_024;
pub(crate) const MAX_DESCRIPTOR_BYTES: usize = 4_096;

const MAX_MESSAGEPACK_DEPTH: usize = 16;
const MAX_MESSAGEPACK_COLLECTION_ITEMS: u32 = 32;
const MAX_MESSAGEPACK_STRING_BYTES: u32 = 512;
const MAX_REQUIRED_FEATURES: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FormatError {
    Serialization,
    InvalidEncoding,
    InputTooLarge,
    TrailingBytes,
    InvalidMagic,
    InvalidRootReference,
    InvalidField,
    MissingRequiredFeature,
    UnsupportedRequiredFeature,
    UnsupportedVersion { version: u16 },
    VersionMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedBootstrap {
    pub(crate) format_version: u16,
}

#[derive(Serialize)]
struct BootstrapWire<'a> {
    bootstrap_version: u16,
    magic: &'a str,
    format_version: u16,
    descriptor: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapWireOwned {
    bootstrap_version: u16,
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

pub(crate) fn encode_bootstrap() -> Result<Vec<u8>, FormatError> {
    rmp_serde::to_vec_named(&BootstrapWire {
        bootstrap_version: CURRENT_REPOSITORY_BOOTSTRAP_VERSION,
        magic: REPOSITORY_MAGIC,
        format_version: CURRENT_REPOSITORY_FORMAT_VERSION,
        descriptor: REPOSITORY_DESCRIPTOR_OBJECT_KEY,
    })
    .map_err(|_| FormatError::Serialization)
}

pub(crate) fn decode_bootstrap(bytes: &[u8]) -> Result<ValidatedBootstrap, FormatError> {
    let bootstrap: BootstrapWireOwned = decode_messagepack(bytes, MAX_BOOTSTRAP_BYTES)?;
    if bootstrap.bootstrap_version != CURRENT_REPOSITORY_BOOTSTRAP_VERSION {
        return Err(FormatError::UnsupportedVersion {
            version: bootstrap.bootstrap_version,
        });
    }
    if bootstrap.magic != REPOSITORY_MAGIC {
        return Err(FormatError::InvalidMagic);
    }
    if bootstrap.format_version != CURRENT_REPOSITORY_FORMAT_VERSION {
        return Err(FormatError::UnsupportedVersion {
            version: bootstrap.format_version,
        });
    }
    if bootstrap.descriptor != REPOSITORY_DESCRIPTOR_OBJECT_KEY {
        return Err(FormatError::InvalidRootReference);
    }
    Ok(ValidatedBootstrap {
        format_version: bootstrap.format_version,
    })
}

pub(crate) fn encode_descriptor(descriptor: &RepositoryDescriptor) -> Result<Vec<u8>, FormatError> {
    let required_features = descriptor
        .required_features()
        .iter()
        .map(|feature| feature.as_str())
        .collect();
    rmp_serde::to_vec_named(&DescriptorWire {
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
    bootstrap_version: u16,
) -> Result<RepositoryDescriptor, FormatError> {
    let wire: DescriptorWireOwned = decode_messagepack(bytes, MAX_DESCRIPTOR_BYTES)?;
    if wire.descriptor_version != CURRENT_REPOSITORY_DESCRIPTOR_VERSION {
        return Err(FormatError::UnsupportedVersion {
            version: wire.descriptor_version,
        });
    }
    if wire.magic != REPOSITORY_MAGIC {
        return Err(FormatError::InvalidMagic);
    }
    if wire.format_version != CURRENT_REPOSITORY_FORMAT_VERSION {
        return Err(FormatError::UnsupportedVersion {
            version: wire.format_version,
        });
    }
    if wire.format_version != bootstrap_version {
        return Err(FormatError::VersionMismatch);
    }

    let identity = RepositoryIdentity::new(wire.repository_id).map_err(map_domain_error)?;
    let repository_key = RepositoryKey::new(wire.repository_key).map_err(map_domain_error)?;

    if wire.required_features.len() > MAX_REQUIRED_FEATURES {
        return Err(FormatError::InvalidField);
    }
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
        wire.descriptor_version,
        wire.format_version,
        identity,
        repository_key,
        required_features,
        roots,
    ))
}

fn decode_messagepack<T>(bytes: &[u8], max_bytes: usize) -> Result<T, FormatError>
where
    T: DeserializeOwned,
{
    if bytes.is_empty() {
        return Err(FormatError::InvalidEncoding);
    }
    if bytes.len() > max_bytes {
        return Err(FormatError::InputTooLarge);
    }

    validate_messagepack(bytes)?;
    let mut decoder = rmp_serde::Deserializer::new(Cursor::new(bytes));
    let value = T::deserialize(&mut decoder).map_err(|_| FormatError::InvalidEncoding)?;
    if decoder.position() != bytes.len() as u64 {
        return Err(FormatError::TrailingBytes);
    }
    Ok(value)
}

fn validate_messagepack(bytes: &[u8]) -> Result<(), FormatError> {
    let mut scanner = MessagePackScanner { bytes, position: 0 };
    scanner.scan_value(0)?;
    if scanner.position != bytes.len() {
        return Err(FormatError::TrailingBytes);
    }
    Ok(())
}

struct MessagePackScanner<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl MessagePackScanner<'_> {
    fn scan_value(&mut self, depth: usize) -> Result<(), FormatError> {
        if depth > MAX_MESSAGEPACK_DEPTH {
            return Err(FormatError::InputTooLarge);
        }
        let marker = self.read_u8()?;
        match marker {
            0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => Ok(()),
            0x80..=0x8f => self.scan_map(u32::from(marker & 0x0f), depth),
            0x90..=0x9f => self.scan_array(u32::from(marker & 0x0f), depth),
            0xa0..=0xbf => self.skip_string(u32::from(marker & 0x1f)),
            0xc1 => Err(FormatError::InvalidEncoding),
            0xc4 => {
                let length = u32::from(self.read_u8()?);
                self.skip_binary(length)
            }
            0xc5 => {
                let length = u32::from(self.read_u16()?);
                self.skip_binary(length)
            }
            0xc6 => {
                let length = self.read_u32()?;
                self.skip_binary(length)
            }
            0xc7 => {
                let length = u32::from(self.read_u8()?);
                self.skip_extension(length)
            }
            0xc8 => {
                let length = u32::from(self.read_u16()?);
                self.skip_extension(length)
            }
            0xc9 => {
                let length = self.read_u32()?;
                self.skip_extension(length)
            }
            0xca => self.skip_exact(4),
            0xcb => self.skip_exact(8),
            0xcc => self.skip_exact(1),
            0xcd => self.skip_exact(2),
            0xce => self.skip_exact(4),
            0xcf => self.skip_exact(8),
            0xd0 => self.skip_exact(1),
            0xd1 => self.skip_exact(2),
            0xd2 => self.skip_exact(4),
            0xd3 => self.skip_exact(8),
            0xd4 => self.skip_exact(2),
            0xd5 => self.skip_exact(3),
            0xd6 => self.skip_exact(5),
            0xd7 => self.skip_exact(9),
            0xd8 => self.skip_exact(17),
            0xd9 => {
                let length = u32::from(self.read_u8()?);
                self.skip_string(length)
            }
            0xda => {
                let length = u32::from(self.read_u16()?);
                self.skip_string(length)
            }
            0xdb => {
                let length = self.read_u32()?;
                self.skip_string(length)
            }
            0xdc => {
                let length = u32::from(self.read_u16()?);
                self.scan_array(length, depth)
            }
            0xdd => {
                let length = self.read_u32()?;
                self.scan_array(length, depth)
            }
            0xde => {
                let length = u32::from(self.read_u16()?);
                self.scan_map(length, depth)
            }
            0xdf => {
                let length = self.read_u32()?;
                self.scan_map(length, depth)
            }
        }
    }

    fn scan_array(&mut self, length: u32, depth: usize) -> Result<(), FormatError> {
        if length > MAX_MESSAGEPACK_COLLECTION_ITEMS {
            return Err(FormatError::InputTooLarge);
        }
        for _ in 0..length {
            self.scan_value(depth + 1)?;
        }
        Ok(())
    }

    fn scan_map(&mut self, length: u32, depth: usize) -> Result<(), FormatError> {
        if length > MAX_MESSAGEPACK_COLLECTION_ITEMS {
            return Err(FormatError::InputTooLarge);
        }
        for _ in 0..length {
            self.scan_value(depth + 1)?;
            self.scan_value(depth + 1)?;
        }
        Ok(())
    }

    fn skip_string(&mut self, length: u32) -> Result<(), FormatError> {
        if length > MAX_MESSAGEPACK_STRING_BYTES {
            return Err(FormatError::InputTooLarge);
        }
        self.skip_exact(usize::try_from(length).map_err(|_| FormatError::InputTooLarge)?)
    }

    fn skip_binary(&mut self, length: u32) -> Result<(), FormatError> {
        self.skip_string(length)
    }

    fn skip_extension(&mut self, length: u32) -> Result<(), FormatError> {
        let length = length.checked_add(1).ok_or(FormatError::InputTooLarge)?;
        self.skip_binary(length)
    }

    fn skip_exact(&mut self, length: usize) -> Result<(), FormatError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(FormatError::InvalidEncoding)?;
        if end > self.bytes.len() {
            return Err(FormatError::InvalidEncoding);
        }
        self.position = end;
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8, FormatError> {
        let value = *self
            .bytes
            .get(self.position)
            .ok_or(FormatError::InvalidEncoding)?;
        self.position += 1;
        Ok(value)
    }

    fn read_u16(&mut self) -> Result<u16, FormatError> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(&mut self) -> Result<u32, FormatError> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_bytes(&mut self, length: usize) -> Result<&[u8], FormatError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(FormatError::InvalidEncoding)?;
        if end > self.bytes.len() {
            return Err(FormatError::InvalidEncoding);
        }
        let bytes = &self.bytes[self.position..end];
        self.position = end;
        Ok(bytes)
    }
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

        let bootstrap = encode_bootstrap();
        let encoded = encode_descriptor(&descriptor);
        assert!(bootstrap.is_ok());
        assert!(encoded.is_ok());
        let bootstrap_bytes = bootstrap.unwrap_or_default();
        let descriptor_bytes = encoded.unwrap_or_default();
        assert_eq!(bootstrap_bytes.first().copied(), Some(0x84));
        assert_eq!(descriptor_bytes.first().copied(), Some(0x87));
        assert_ne!(bootstrap_bytes.first().copied(), Some(b'{'));
        assert_ne!(descriptor_bytes.first().copied(), Some(b'{'));

        let bootstrap = decode_bootstrap(&bootstrap_bytes);
        assert!(bootstrap.is_ok());
        let decoded = decode_descriptor(
            &descriptor_bytes,
            bootstrap.map_or(0, |bootstrap| bootstrap.format_version),
        );
        assert_eq!(decoded, Ok(descriptor.clone()));
        assert_eq!(encode_bootstrap().unwrap_or_default(), bootstrap_bytes);
        assert_eq!(
            encode_descriptor(&descriptor).unwrap_or_default(),
            descriptor_bytes
        );
    }

    #[test]
    fn an_unknown_bootstrap_version_is_not_treated_as_a_legacy_descriptor() {
        let bytes = rmp_serde::to_vec_named(&BootstrapWire {
            bootstrap_version: 99,
            magic: REPOSITORY_MAGIC,
            format_version: CURRENT_REPOSITORY_FORMAT_VERSION,
            descriptor: REPOSITORY_DESCRIPTOR_OBJECT_KEY,
        });
        assert!(bytes.is_ok());
        assert_eq!(
            decode_bootstrap(&bytes.unwrap_or_default()),
            Err(FormatError::UnsupportedVersion { version: 99 })
        );
    }

    #[test]
    fn malformed_messagepack_is_bounded_and_trailing_bytes_are_rejected() {
        let bootstrap = encode_bootstrap().unwrap_or_default();
        let mut trailing = bootstrap.clone();
        trailing.push(0);
        assert_eq!(decode_bootstrap(&trailing), Err(FormatError::TrailingBytes));
        assert_eq!(
            decode_bootstrap(&vec![0; MAX_BOOTSTRAP_BYTES + 1]),
            Err(FormatError::InputTooLarge)
        );
        assert!(decode_bootstrap(b"\x81").is_err());
    }

    #[test]
    fn duplicate_bootstrap_fields_are_rejected() {
        let mut bytes = vec![0x85];
        append_string(&mut bytes, "magic");
        append_string(&mut bytes, REPOSITORY_MAGIC);
        append_string(&mut bytes, "magic");
        append_string(&mut bytes, REPOSITORY_MAGIC);
        append_string(&mut bytes, "bootstrap_version");
        bytes.push(1);
        append_string(&mut bytes, "format_version");
        bytes.push(1);
        append_string(&mut bytes, "descriptor");
        append_string(&mut bytes, REPOSITORY_DESCRIPTOR_OBJECT_KEY);

        assert_eq!(decode_bootstrap(&bytes), Err(FormatError::InvalidEncoding));
    }

    fn append_string(bytes: &mut Vec<u8>, value: &str) {
        assert!(value.len() < 32);
        bytes.push(0xa0 | value.len() as u8);
        bytes.extend_from_slice(value.as_bytes());
    }
}
