use crate::domain::{
    CURRENT_PACK_FORMAT_VERSION, MAX_PACK_SIZE_BYTES, PACK_ALIGNMENT, PACK_ENTRY_HEADER_LENGTH,
    PACK_FOOTER_LENGTH, PACK_HEADER_LENGTH, PackConfiguration, PackEntryInput, PackEntryLocation,
    PackId, PackMetadata, PackMetadataParts, SealedPack,
};
use sha2::{Digest, Sha256};

const PACK_MAGIC: &[u8; 4] = b"GIBP";
const ENTRY_MAGIC: &[u8; 4] = b"ENTR";
const FOOTER_MAGIC: &[u8; 4] = b"GIBF";
const PACK_FLAGS: u16 = 0;
const PACK_ID_DOMAIN_SEPARATOR: &[u8] = b"GIB pack identity\0";

/// A failure while building or validating the private pack representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PackFormatError {
    UnsupportedVersion { version: u16 },
    InvalidMagic,
    InvalidField,
    InvalidLength,
    InvalidChecksum,
    Truncated,
    TrailingData,
    PackTooLarge,
    AllocationFailure,
    InvalidLocation,
    BuilderFinished,
    BuilderAborted,
}

pub(crate) struct PackBuilder {
    configuration: PackConfiguration,
    current: Option<PackAccumulator>,
    state: BuilderState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuilderState {
    Open,
    Finished,
    Aborted,
}

struct PackAccumulator {
    configuration: PackConfiguration,
    bytes: Vec<u8>,
    entry_count: u64,
    payload_bytes: u64,
}

impl PackBuilder {
    pub(crate) const fn new(configuration: PackConfiguration) -> Self {
        Self {
            configuration,
            current: None,
            state: BuilderState::Open,
        }
    }

    pub(crate) const fn configuration(&self) -> PackConfiguration {
        self.configuration
    }

    pub(crate) fn add(
        &mut self,
        entry: PackEntryInput,
    ) -> Result<Option<SealedPack>, PackFormatError> {
        self.ensure_open()?;
        let payload_length =
            u64::try_from(entry.payload().len()).map_err(|_| PackFormatError::PackTooLarge)?;
        let entry_length = entry_frame_length(payload_length)?;
        let single_total = total_length(PACK_HEADER_LENGTH as u64, entry_length)?;
        if single_total > MAX_PACK_SIZE_BYTES {
            return Err(PackFormatError::PackTooLarge);
        }

        let split = self
            .current
            .as_ref()
            .filter(|current| current.entry_count > 0)
            .map(|current| {
                current.total_length_with(entry_length).map(|length| {
                    length > self.configuration.target_size()
                        || length > self.configuration.max_size()
                })
            })
            .transpose()?
            .unwrap_or(false);

        let sealed = if split {
            Some(self.seal_current()?)
        } else {
            None
        };

        if self.current.is_none() {
            self.current = Some(PackAccumulator::new(self.configuration)?);
        }
        self.current
            .as_mut()
            .ok_or(PackFormatError::InvalidField)?
            .add_entry(entry, entry_length)?;
        Ok(sealed)
    }

    pub(crate) fn finish(&mut self) -> Result<Option<SealedPack>, PackFormatError> {
        self.ensure_open()?;
        self.state = BuilderState::Finished;
        if self.current.is_none() {
            return Ok(None);
        }
        match self.seal_current() {
            Ok(pack) => Ok(Some(pack)),
            Err(error) => {
                self.state = BuilderState::Aborted;
                Err(error)
            }
        }
    }

    pub(crate) fn abort(&mut self) {
        self.current = None;
        self.state = BuilderState::Aborted;
    }

    fn seal_current(&mut self) -> Result<SealedPack, PackFormatError> {
        let Some(current) = self.current.take() else {
            return Err(PackFormatError::InvalidField);
        };
        match current.seal() {
            Ok(pack) => Ok(pack),
            Err(error) => {
                self.state = BuilderState::Aborted;
                Err(error)
            }
        }
    }

    fn ensure_open(&self) -> Result<(), PackFormatError> {
        match self.state {
            BuilderState::Open => Ok(()),
            BuilderState::Finished => Err(PackFormatError::BuilderFinished),
            BuilderState::Aborted => Err(PackFormatError::BuilderAborted),
        }
    }
}

impl PackAccumulator {
    fn new(configuration: PackConfiguration) -> Result<Self, PackFormatError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve(PACK_HEADER_LENGTH)
            .map_err(|_| PackFormatError::AllocationFailure)?;
        write_header(&mut bytes, configuration, 0, 0);
        Ok(Self {
            configuration,
            bytes,
            entry_count: 0,
            payload_bytes: 0,
        })
    }

    fn total_length_with(&self, entry_length: u64) -> Result<u64, PackFormatError> {
        total_length(
            u64::try_from(self.bytes.len()).map_err(|_| PackFormatError::PackTooLarge)?,
            entry_length,
        )
    }

    fn add_entry(
        &mut self,
        entry: PackEntryInput,
        entry_length: u64,
    ) -> Result<(), PackFormatError> {
        let total_length = self.total_length_with(entry_length)?;
        if total_length > MAX_PACK_SIZE_BYTES {
            return Err(PackFormatError::PackTooLarge);
        }
        if self.entry_count > 0 && total_length > self.configuration.max_size() {
            return Err(PackFormatError::InvalidLength);
        }

        let (chunk_id, plaintext_length, payload) = entry.into_parts();
        let payload_length =
            u64::try_from(payload.len()).map_err(|_| PackFormatError::PackTooLarge)?;
        let entry_length_usize =
            usize::try_from(entry_length).map_err(|_| PackFormatError::PackTooLarge)?;
        self.bytes
            .try_reserve(entry_length_usize)
            .map_err(|_| PackFormatError::AllocationFailure)?;
        let next_entry_count = self
            .entry_count
            .checked_add(1)
            .ok_or(PackFormatError::PackTooLarge)?;
        let next_payload_bytes = self
            .payload_bytes
            .checked_add(payload_length)
            .ok_or(PackFormatError::PackTooLarge)?;

        let payload_checksum = digest(&payload);
        let mut header = [0_u8; PACK_ENTRY_HEADER_LENGTH];
        header[0..4].copy_from_slice(ENTRY_MAGIC);
        put_u16(&mut header, 4, CURRENT_PACK_FORMAT_VERSION);
        put_u16(&mut header, 6, PACK_FLAGS);
        put_u64(&mut header, 8, entry_length);
        header[16..48].copy_from_slice(&chunk_id.as_bytes());
        put_u64(&mut header, 48, plaintext_length);
        put_u64(&mut header, 56, payload_length);
        header[64..96].copy_from_slice(&payload_checksum);

        self.bytes.extend_from_slice(&header);
        self.bytes.extend_from_slice(&payload);
        let padding = entry_length_usize
            .checked_sub(PACK_ENTRY_HEADER_LENGTH)
            .and_then(|length| length.checked_sub(payload.len()))
            .ok_or(PackFormatError::InvalidLength)?;
        let new_length = self
            .bytes
            .len()
            .checked_add(padding)
            .ok_or(PackFormatError::PackTooLarge)?;
        self.bytes.resize(new_length, 0);
        self.entry_count = next_entry_count;
        self.payload_bytes = next_payload_bytes;
        Ok(())
    }

    fn seal(mut self) -> Result<SealedPack, PackFormatError> {
        let entry_count = self.entry_count;
        let body_length =
            u64::try_from(self.bytes.len()).map_err(|_| PackFormatError::PackTooLarge)?;
        write_header(
            &mut self.bytes,
            self.configuration,
            entry_count,
            self.payload_bytes,
        );
        self.bytes
            .try_reserve(PACK_FOOTER_LENGTH)
            .map_err(|_| PackFormatError::AllocationFailure)?;
        let body_checksum = digest(&self.bytes);
        let pack_id = pack_id_for_body(self.configuration.version(), &self.bytes);
        let total_length = total_length(body_length, 0)?;
        let oversized_single_entry =
            entry_count == 1 && total_length > self.configuration.max_size();
        if total_length > MAX_PACK_SIZE_BYTES
            || (total_length > self.configuration.max_size() && !oversized_single_entry)
        {
            return Err(PackFormatError::PackTooLarge);
        }

        let body_end = usize::try_from(body_length).map_err(|_| PackFormatError::PackTooLarge)?;
        let mut locations = Vec::new();
        let mut offset = PACK_HEADER_LENGTH;
        while offset < body_end {
            let entry = parse_entry(&self.bytes, offset, body_end, pack_id)?;
            locations
                .try_reserve(1)
                .map_err(|_| PackFormatError::AllocationFailure)?;
            locations.push(entry.location);
            offset = entry.end_offset;
        }
        if offset != body_end
            || u64::try_from(locations.len()).map_err(|_| PackFormatError::PackTooLarge)?
                != entry_count
        {
            return Err(PackFormatError::InvalidLength);
        }

        let mut footer = [0_u8; PACK_FOOTER_LENGTH];
        footer[0..4].copy_from_slice(FOOTER_MAGIC);
        put_u16(&mut footer, 4, self.configuration.version());
        put_u16(&mut footer, 6, PACK_FLAGS);
        put_u32(&mut footer, 8, PACK_FOOTER_LENGTH as u32);
        put_u64(&mut footer, 12, entry_count);
        put_u64(&mut footer, 20, body_length);
        put_u64(&mut footer, 28, self.payload_bytes);
        footer[36..68].copy_from_slice(&body_checksum);
        footer[68..100].copy_from_slice(&pack_id.as_bytes());
        self.bytes.extend_from_slice(&footer);

        let metadata = PackMetadata::from_parts(PackMetadataParts {
            pack_id,
            version: self.configuration.version(),
            target_size: self.configuration.target_size(),
            max_size: self.configuration.max_size(),
            entry_count,
            payload_bytes: self.payload_bytes,
            body_length,
            total_length,
            oversized_single_entry,
        });
        Ok(SealedPack::new(pack_id, self.bytes, metadata, locations))
    }
}

pub(crate) struct VerifiedPack<'a> {
    bytes: &'a [u8],
    metadata: PackMetadata,
    entries: Vec<PackEntryLocation>,
}

impl<'a> VerifiedPack<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Result<Self, PackFormatError> {
        let input_length = u64::try_from(bytes.len()).map_err(|_| PackFormatError::PackTooLarge)?;
        if input_length > MAX_PACK_SIZE_BYTES {
            return Err(PackFormatError::PackTooLarge);
        }
        let minimum_length = PACK_HEADER_LENGTH
            .checked_add(PACK_FOOTER_LENGTH)
            .ok_or(PackFormatError::PackTooLarge)?;
        if bytes.len() < minimum_length {
            return Err(PackFormatError::Truncated);
        }
        let header = parse_header(bytes)?;
        let footer_offset = bytes
            .len()
            .checked_sub(PACK_FOOTER_LENGTH)
            .ok_or(PackFormatError::Truncated)?;
        let footer = parse_footer(bytes, footer_offset)?;
        if footer.version != header.configuration.version() {
            return Err(PackFormatError::InvalidField);
        }
        if footer.body_length
            != u64::try_from(footer_offset).map_err(|_| PackFormatError::InvalidLength)?
        {
            return Err(PackFormatError::InvalidLength);
        }
        if footer.body_length < PACK_HEADER_LENGTH as u64
            || !footer.body_length.is_multiple_of(PACK_ALIGNMENT)
        {
            return Err(PackFormatError::InvalidLength);
        }

        let body = bytes
            .get(..footer_offset)
            .ok_or(PackFormatError::Truncated)?;
        if digest(body) != footer.body_checksum {
            return Err(PackFormatError::InvalidChecksum);
        }
        let calculated_id = pack_id_for_body(header.configuration.version(), body);
        if calculated_id != footer.pack_id {
            return Err(PackFormatError::InvalidChecksum);
        }

        let mut entries = Vec::new();
        let mut offset = PACK_HEADER_LENGTH;
        let mut payload_bytes = 0_u64;
        while offset < footer_offset {
            let remaining = footer_offset
                .checked_sub(offset)
                .ok_or(PackFormatError::InvalidLength)?;
            if remaining < PACK_ENTRY_HEADER_LENGTH {
                return Err(PackFormatError::Truncated);
            }
            let entry = parse_entry(bytes, offset, footer_offset, footer.pack_id)?;
            payload_bytes = payload_bytes
                .checked_add(entry.payload_length)
                .ok_or(PackFormatError::InvalidLength)?;
            entries
                .try_reserve(1)
                .map_err(|_| PackFormatError::AllocationFailure)?;
            entries.push(entry.location);
            offset = entry.end_offset;
        }
        if offset != footer_offset {
            return Err(PackFormatError::TrailingData);
        }

        let entry_count =
            u64::try_from(entries.len()).map_err(|_| PackFormatError::InvalidLength)?;
        if entry_count != header.entry_count
            || entry_count != footer.entry_count
            || payload_bytes != header.payload_bytes
            || payload_bytes != footer.payload_bytes
        {
            return Err(PackFormatError::InvalidLength);
        }
        let total_length = input_length;
        let oversized_single_entry =
            entry_count == 1 && total_length > header.configuration.max_size();
        if total_length > header.configuration.max_size() && !oversized_single_entry {
            return Err(PackFormatError::InvalidLength);
        }
        let metadata = PackMetadata::from_parts(PackMetadataParts {
            pack_id: footer.pack_id,
            version: header.configuration.version(),
            target_size: header.configuration.target_size(),
            max_size: header.configuration.max_size(),
            entry_count,
            payload_bytes,
            body_length: footer.body_length,
            total_length,
            oversized_single_entry,
        });
        Ok(Self {
            bytes,
            metadata,
            entries,
        })
    }

    pub(crate) const fn metadata(&self) -> PackMetadata {
        self.metadata
    }

    pub(crate) fn entries(&self) -> &[PackEntryLocation] {
        &self.entries
    }

    pub(crate) fn payload(
        &self,
        location: &PackEntryLocation,
    ) -> Result<&'a [u8], PackFormatError> {
        if location.pack_id() != self.metadata.pack_id()
            || !self.entries.iter().any(|candidate| candidate == location)
        {
            return Err(PackFormatError::InvalidLocation);
        }
        let start = usize::try_from(location.payload_offset())
            .map_err(|_| PackFormatError::InvalidLocation)?;
        let length = usize::try_from(location.payload_length())
            .map_err(|_| PackFormatError::InvalidLocation)?;
        let end = start
            .checked_add(length)
            .ok_or(PackFormatError::InvalidLocation)?;
        self.bytes
            .get(start..end)
            .ok_or(PackFormatError::InvalidLocation)
    }
}

struct ParsedHeader {
    configuration: PackConfiguration,
    entry_count: u64,
    payload_bytes: u64,
}

struct ParsedFooter {
    version: u16,
    entry_count: u64,
    body_length: u64,
    payload_bytes: u64,
    body_checksum: [u8; 32],
    pack_id: PackId,
}

struct ParsedEntry {
    location: PackEntryLocation,
    payload_length: u64,
    end_offset: usize,
}

fn parse_header(bytes: &[u8]) -> Result<ParsedHeader, PackFormatError> {
    let header = bytes
        .get(..PACK_HEADER_LENGTH)
        .ok_or(PackFormatError::Truncated)?;
    if header[0..4] != *PACK_MAGIC {
        return Err(PackFormatError::InvalidMagic);
    }
    let version = get_u16(header, 4)?;
    if version != CURRENT_PACK_FORMAT_VERSION {
        return Err(PackFormatError::UnsupportedVersion { version });
    }
    if get_u16(header, 6)? != PACK_FLAGS
        || get_u32(header, 8)? != PACK_HEADER_LENGTH as u32
        || get_u32(header, 12)? != PACK_ALIGNMENT as u32
    {
        return Err(PackFormatError::InvalidField);
    }
    let target_size = get_u64(header, 16)?;
    let max_size = get_u64(header, 24)?;
    let entries_offset = get_u64(header, 32)?;
    if entries_offset != PACK_HEADER_LENGTH as u64 {
        return Err(PackFormatError::InvalidLength);
    }
    let entry_count = get_u64(header, 40)?;
    let payload_bytes = get_u64(header, 48)?;
    if header[56..64].iter().any(|byte| *byte != 0) {
        return Err(PackFormatError::InvalidField);
    }
    let configuration = PackConfiguration::from_parts(version, target_size, max_size).map_err(
        |error| match error {
            crate::domain::PackConfigurationError::UnsupportedVersion => {
                PackFormatError::UnsupportedVersion { version }
            }
            crate::domain::PackConfigurationError::TargetMustBePositive
            | crate::domain::PackConfigurationError::MaximumMustBePositive
            | crate::domain::PackConfigurationError::TargetExceedsMaximum
            | crate::domain::PackConfigurationError::SizeExceedsLimit
            | crate::domain::PackConfigurationError::SizeExceedsPlatformLimit => {
                PackFormatError::InvalidField
            }
        },
    )?;
    Ok(ParsedHeader {
        configuration,
        entry_count,
        payload_bytes,
    })
}

fn parse_footer(bytes: &[u8], offset: usize) -> Result<ParsedFooter, PackFormatError> {
    let end = offset
        .checked_add(PACK_FOOTER_LENGTH)
        .ok_or(PackFormatError::Truncated)?;
    let footer = bytes.get(offset..end).ok_or(PackFormatError::Truncated)?;
    if footer[0..4] != *FOOTER_MAGIC {
        return Err(PackFormatError::InvalidMagic);
    }
    let version = get_u16(footer, 4)?;
    if version != CURRENT_PACK_FORMAT_VERSION {
        return Err(PackFormatError::UnsupportedVersion { version });
    }
    if get_u16(footer, 6)? != PACK_FLAGS || get_u32(footer, 8)? != PACK_FOOTER_LENGTH as u32 {
        return Err(PackFormatError::InvalidField);
    }
    if footer[100..104].iter().any(|byte| *byte != 0) {
        return Err(PackFormatError::InvalidField);
    }
    let body_checksum = read_array::<32>(footer, 36)?;
    let pack_id = PackId::from_digest(read_array::<32>(footer, 68)?);
    Ok(ParsedFooter {
        version,
        entry_count: get_u64(footer, 12)?,
        body_length: get_u64(footer, 20)?,
        payload_bytes: get_u64(footer, 28)?,
        body_checksum,
        pack_id,
    })
}

fn parse_entry(
    bytes: &[u8],
    offset: usize,
    body_end: usize,
    pack_id: PackId,
) -> Result<ParsedEntry, PackFormatError> {
    let header_end = offset
        .checked_add(PACK_ENTRY_HEADER_LENGTH)
        .ok_or(PackFormatError::Truncated)?;
    let header = bytes
        .get(offset..header_end)
        .ok_or(PackFormatError::Truncated)?;
    if header[0..4] != *ENTRY_MAGIC {
        return Err(PackFormatError::InvalidMagic);
    }
    let version = get_u16(header, 4)?;
    if version != CURRENT_PACK_FORMAT_VERSION {
        return Err(PackFormatError::UnsupportedVersion { version });
    }
    if get_u16(header, 6)? != PACK_FLAGS {
        return Err(PackFormatError::InvalidField);
    }
    let entry_length = get_u64(header, 8)?;
    if entry_length < PACK_ENTRY_HEADER_LENGTH as u64
        || !entry_length.is_multiple_of(PACK_ALIGNMENT)
    {
        return Err(PackFormatError::InvalidLength);
    }
    let entry_length_usize =
        usize::try_from(entry_length).map_err(|_| PackFormatError::InvalidLength)?;
    let end_offset = offset
        .checked_add(entry_length_usize)
        .ok_or(PackFormatError::InvalidLength)?;
    if end_offset > body_end {
        return Err(PackFormatError::Truncated);
    }
    let payload_length = get_u64(header, 56)?;
    let unpadded_length = (PACK_ENTRY_HEADER_LENGTH as u64)
        .checked_add(payload_length)
        .ok_or(PackFormatError::InvalidLength)?;
    if align_up(unpadded_length)? != entry_length {
        return Err(PackFormatError::InvalidLength);
    }
    let payload_offset = offset
        .checked_add(PACK_ENTRY_HEADER_LENGTH)
        .ok_or(PackFormatError::InvalidLength)?;
    let payload_length_usize =
        usize::try_from(payload_length).map_err(|_| PackFormatError::InvalidLength)?;
    let payload_end = payload_offset
        .checked_add(payload_length_usize)
        .ok_or(PackFormatError::InvalidLength)?;
    if payload_end > end_offset {
        return Err(PackFormatError::InvalidLength);
    }
    let payload = bytes
        .get(payload_offset..payload_end)
        .ok_or(PackFormatError::Truncated)?;
    if digest(payload) != read_array::<32>(header, 64)? {
        return Err(PackFormatError::InvalidChecksum);
    }
    if bytes
        .get(payload_end..end_offset)
        .ok_or(PackFormatError::Truncated)?
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(PackFormatError::InvalidField);
    }
    let chunk_id = crate::domain::ChunkId::from_digest(read_array::<32>(header, 16)?);
    let plaintext_length = get_u64(header, 48)?;
    if plaintext_length > crate::domain::MAX_CONTENT_DEFINED_CHUNK_SIZE_BYTES {
        return Err(PackFormatError::InvalidLength);
    }
    let location = PackEntryLocation::new(
        pack_id,
        chunk_id,
        u64::try_from(offset).map_err(|_| PackFormatError::InvalidLength)?,
        u64::try_from(payload_offset).map_err(|_| PackFormatError::InvalidLength)?,
        entry_length,
        payload_length,
        plaintext_length,
    );
    Ok(ParsedEntry {
        location,
        payload_length,
        end_offset,
    })
}

fn write_header(
    bytes: &mut Vec<u8>,
    configuration: PackConfiguration,
    entry_count: u64,
    payload_bytes: u64,
) {
    let mut header = [0_u8; PACK_HEADER_LENGTH];
    header[0..4].copy_from_slice(PACK_MAGIC);
    put_u16(&mut header, 4, configuration.version());
    put_u16(&mut header, 6, PACK_FLAGS);
    put_u32(&mut header, 8, PACK_HEADER_LENGTH as u32);
    put_u32(&mut header, 12, PACK_ALIGNMENT as u32);
    put_u64(&mut header, 16, configuration.target_size());
    put_u64(&mut header, 24, configuration.max_size());
    put_u64(&mut header, 32, PACK_HEADER_LENGTH as u64);
    put_u64(&mut header, 40, entry_count);
    put_u64(&mut header, 48, payload_bytes);
    if bytes.is_empty() {
        bytes.extend_from_slice(&header);
    } else {
        bytes[0..PACK_HEADER_LENGTH].copy_from_slice(&header);
    }
}

fn entry_frame_length(payload_length: u64) -> Result<u64, PackFormatError> {
    align_up(
        (PACK_ENTRY_HEADER_LENGTH as u64)
            .checked_add(payload_length)
            .ok_or(PackFormatError::PackTooLarge)?,
    )
}

fn total_length(body_length: u64, entry_length: u64) -> Result<u64, PackFormatError> {
    body_length
        .checked_add(entry_length)
        .and_then(|length| length.checked_add(PACK_FOOTER_LENGTH as u64))
        .ok_or(PackFormatError::PackTooLarge)
}

fn align_up(value: u64) -> Result<u64, PackFormatError> {
    let remainder = value % PACK_ALIGNMENT;
    if remainder == 0 {
        Ok(value)
    } else {
        value
            .checked_add(PACK_ALIGNMENT - remainder)
            .ok_or(PackFormatError::PackTooLarge)
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn pack_id_for_body(version: u16, body: &[u8]) -> PackId {
    let mut hasher = Sha256::new();
    hasher.update(PACK_ID_DOMAIN_SEPARATOR);
    hasher.update(version.to_be_bytes());
    hasher.update(body);
    PackId::from_digest(hasher.finalize().into())
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
}

fn get_u16(bytes: &[u8], offset: usize) -> Result<u16, PackFormatError> {
    Ok(u16::from_be_bytes(read_array(bytes, offset)?))
}

fn get_u32(bytes: &[u8], offset: usize) -> Result<u32, PackFormatError> {
    Ok(u32::from_be_bytes(read_array(bytes, offset)?))
}

fn get_u64(bytes: &[u8], offset: usize) -> Result<u64, PackFormatError> {
    Ok(u64::from_be_bytes(read_array(bytes, offset)?))
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], PackFormatError> {
    let end = offset.checked_add(N).ok_or(PackFormatError::Truncated)?;
    bytes
        .get(offset..end)
        .ok_or(PackFormatError::Truncated)?
        .try_into()
        .map_err(|_| PackFormatError::Truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ChunkId, PackConfiguration};

    fn entry(byte: u8, length: usize) -> PackEntryInput {
        PackEntryInput::new(
            ChunkId::from_content(&vec![byte; length]),
            length as u64,
            vec![byte; length],
        )
        .expect("test entry should be valid")
    }

    #[test]
    fn pack_builder_seals_and_verifies_entries() {
        let configuration = PackConfiguration::new(256, 512).expect("valid configuration");
        let mut builder = PackBuilder::new(configuration);
        assert!(
            builder
                .add(entry(1, 32))
                .expect("entry should fit")
                .is_none()
        );
        let pack = builder
            .finish()
            .expect("finish should succeed")
            .expect("one pack should be produced");
        let verified = VerifiedPack::new(pack.as_bytes()).expect("pack should verify");
        assert_eq!(verified.metadata(), pack.metadata());
        assert_eq!(verified.entries(), pack.entries());
        assert_eq!(
            verified
                .payload(&pack.entries()[0])
                .expect("payload should read"),
            &[1; 32]
        );
    }

    #[test]
    fn builder_does_not_emit_an_empty_pack() {
        let configuration = PackConfiguration::new(256, 512).expect("valid configuration");
        let mut builder = PackBuilder::new(configuration);
        assert!(builder.finish().expect("finish should succeed").is_none());
    }

    #[test]
    fn footer_corruption_is_rejected() {
        let configuration = PackConfiguration::new(256, 512).expect("valid configuration");
        let mut builder = PackBuilder::new(configuration);
        builder.add(entry(1, 32)).expect("entry should fit");
        let pack = builder
            .finish()
            .expect("finish should succeed")
            .expect("one pack should be produced");
        let mut bytes = pack.as_bytes().to_vec();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(matches!(
            VerifiedPack::new(&bytes),
            Err(PackFormatError::InvalidField)
        ));
    }
}
