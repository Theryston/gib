use super::envelope::{
    decode_object_envelope, decode_object_envelope_with_encryption, encode_object_envelope,
};
use super::repository::{FormatError, decode_messagepack_with_limits};
use super::transform::EncryptionContext;
use crate::domain::{
    CURRENT_TREE_METADATA_VERSION, CURRENT_TREE_NODE_VERSION, DirectoryNode, FileChunkReference,
    FilePermissions, MAX_FILE_CHUNK_REFERENCES, MAX_METADATA_EXTENSION_BYTES,
    MAX_METADATA_EXTENSIONS, MAX_METADATA_NAMESPACE_BYTES, MAX_SYMLINK_TARGET_BYTES,
    MAX_TREE_ENTRIES, MAX_TREE_NAME_BYTES, MetadataNamespace, ObjectId, ObjectKind,
    PortableMetadata, RegularFileNode, SymbolicLinkNode, SymlinkTarget, TreeEntry, TreeNode,
    TreeNodeKind, TreeNodeReference, TreeValidationError,
};
use rmp_serde::config::BytesMode;
use serde::{Deserialize, Deserializer, Serialize};

const MAX_TREE_PAYLOAD_BYTES: usize = crate::domain::MAX_IMMUTABLE_OBJECT_PAYLOAD_BYTES;
const MAX_TREE_COLLECTION_ITEMS: u32 = MAX_TREE_ENTRIES as u32;
const MAX_TREE_STRING_BYTES: u32 = if MAX_TREE_NAME_BYTES > MAX_METADATA_NAMESPACE_BYTES {
    MAX_TREE_NAME_BYTES as u32
} else {
    MAX_METADATA_NAMESPACE_BYTES as u32
};
const MAX_TREE_BINARY_BYTES: u32 = if MAX_METADATA_EXTENSION_BYTES > MAX_SYMLINK_TARGET_BYTES {
    MAX_METADATA_EXTENSION_BYTES as u32
} else {
    MAX_SYMLINK_TARGET_BYTES as u32
};
const MAX_TREE_MESSAGEPACK_DEPTH: usize = 8;

#[derive(Serialize)]
struct TreePayloadWire<'a> {
    tree_version: u16,
    magic: &'a str,
    node_kind: &'a str,
    metadata: MetadataWire<'a>,
    entries: Vec<DirectoryEntryWire<'a>>,
    file_size: u64,
    chunks: Vec<FileChunkWire>,
    symlink_target: &'a [u8],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TreePayloadWireOwned {
    tree_version: u16,
    magic: String,
    node_kind: String,
    metadata: MetadataWireOwned,
    entries: Vec<DirectoryEntryWireOwned>,
    file_size: u64,
    chunks: Vec<FileChunkWireOwned>,
    symlink_target: Vec<u8>,
}

#[derive(Serialize)]
struct MetadataWire<'a> {
    metadata_version: u16,
    permissions: u32,
    modified_at: Option<i64>,
    extensions: Vec<MetadataExtensionWire<'a>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MetadataWireOwned {
    metadata_version: u16,
    permissions: u32,
    modified_at: RequiredOption<i64>,
    extensions: Vec<MetadataExtensionWireOwned>,
}

#[derive(Serialize)]
struct MetadataExtensionWire<'a> {
    namespace: &'a str,
    version: u16,
    value: &'a [u8],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MetadataExtensionWireOwned {
    namespace: String,
    version: u16,
    value: Vec<u8>,
}

#[derive(Serialize)]
struct DirectoryEntryWire<'a> {
    name: &'a str,
    node_kind: &'a str,
    node_id: &'a [u8],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryEntryWireOwned {
    name: String,
    node_kind: String,
    node_id: Vec<u8>,
}

#[derive(Serialize)]
struct FileChunkWire {
    chunk_id: Vec<u8>,
    size: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileChunkWireOwned {
    chunk_id: Vec<u8>,
    size: u64,
}

struct RequiredOption<T>(Option<T>);

impl<'de, T> Deserialize<'de> for RequiredOption<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Self)
    }
}

pub(crate) fn encode_tree_node(node: &TreeNode) -> Result<Vec<u8>, FormatError> {
    encode_tree_node_with_id(node).map(|(_, bytes)| bytes)
}

pub(crate) fn encode_tree_node_with_id(
    node: &TreeNode,
) -> Result<(ObjectId, Vec<u8>), FormatError> {
    let payload = encode_tree_payload(node)?;
    let object_id =
        super::envelope::calculate_object_id(ObjectKind::Tree, CURRENT_TREE_NODE_VERSION, &payload);
    let encoded = encode_object_envelope(
        ObjectKind::Tree,
        CURRENT_TREE_NODE_VERSION,
        crate::domain::ObjectCodec::None,
        crate::domain::ObjectEncryption::None,
        &payload,
    )?;
    Ok((object_id, encoded))
}

pub(crate) fn tree_node_id(node: &TreeNode) -> Result<ObjectId, FormatError> {
    encode_tree_payload(node).map(|payload| {
        super::envelope::calculate_object_id(ObjectKind::Tree, CURRENT_TREE_NODE_VERSION, &payload)
    })
}

pub(crate) fn decode_tree_node(bytes: &[u8]) -> Result<TreeNode, FormatError> {
    let object = decode_object_envelope(bytes)?;
    decode_tree_object(object)
}

pub(crate) fn decode_tree_node_with_encryption(
    bytes: &[u8],
    context: &EncryptionContext,
) -> Result<TreeNode, FormatError> {
    let object = decode_object_envelope_with_encryption(bytes, context)?;
    decode_tree_object(object)
}

fn decode_tree_object(object: crate::domain::ImmutableObject) -> Result<TreeNode, FormatError> {
    if object.kind() != ObjectKind::Tree {
        return Err(FormatError::InvalidObjectKind);
    }
    if object.version() != CURRENT_TREE_NODE_VERSION {
        return Err(FormatError::UnsupportedObjectVersion {
            version: object.version(),
        });
    }
    let node = decode_tree_payload(object.payload())?;
    if encode_tree_payload(&node)? != object.payload() {
        return Err(FormatError::InvalidEncoding);
    }
    Ok(node)
}

fn encode_tree_payload(node: &TreeNode) -> Result<Vec<u8>, FormatError> {
    let metadata = encode_metadata(node.metadata())?;
    let mut entries = Vec::new();
    let mut file_size = 0_u64;
    let mut chunks = Vec::new();
    let mut symlink_target = &[][..];

    match node {
        TreeNode::Directory(directory) => {
            entries.reserve(directory.entries().len());
            for entry in directory.entries() {
                entries.push(DirectoryEntryWire {
                    name: entry.name().as_str(),
                    node_kind: entry.kind().as_str(),
                    node_id: entry.node_id().as_digest(),
                });
            }
        }
        TreeNode::RegularFile(file) => {
            file_size = file.size();
            chunks.reserve(file.chunks().len());
            for chunk in file.chunks() {
                chunks.push(FileChunkWire {
                    chunk_id: chunk.id().as_bytes().to_vec(),
                    size: chunk.size(),
                });
            }
        }
        TreeNode::SymbolicLink(link) => {
            symlink_target = link.target().as_bytes();
        }
    }

    encode_wire(&TreePayloadWire {
        tree_version: CURRENT_TREE_NODE_VERSION,
        magic: crate::domain::REPOSITORY_MAGIC,
        node_kind: node.kind().as_str(),
        metadata,
        entries,
        file_size,
        chunks,
        symlink_target,
    })
}

fn encode_metadata(metadata: &PortableMetadata) -> Result<MetadataWire<'_>, FormatError> {
    let extensions = metadata
        .extensions()
        .iter()
        .map(|extension| MetadataExtensionWire {
            namespace: extension.namespace().as_str(),
            version: extension.version(),
            value: extension.value(),
        })
        .collect();
    Ok(MetadataWire {
        metadata_version: CURRENT_TREE_METADATA_VERSION,
        permissions: metadata.permissions().mode(),
        modified_at: metadata.modified_at(),
        extensions,
    })
}

fn decode_tree_payload(bytes: &[u8]) -> Result<TreeNode, FormatError> {
    let wire: TreePayloadWireOwned = decode_messagepack_with_limits(
        bytes,
        MAX_TREE_PAYLOAD_BYTES,
        MAX_TREE_STRING_BYTES,
        MAX_TREE_BINARY_BYTES,
        MAX_TREE_COLLECTION_ITEMS,
        MAX_TREE_MESSAGEPACK_DEPTH,
    )?;
    if wire.tree_version != CURRENT_TREE_NODE_VERSION {
        return Err(FormatError::UnsupportedObjectVersion {
            version: wire.tree_version,
        });
    }
    if wire.magic != crate::domain::REPOSITORY_MAGIC {
        return Err(FormatError::InvalidMagic);
    }
    let kind = TreeNodeKind::parse(&wire.node_kind).ok_or(FormatError::InvalidField)?;
    let metadata = decode_metadata(wire.metadata)?;
    match kind {
        TreeNodeKind::Directory => {
            if wire.file_size != 0 || !wire.chunks.is_empty() || !wire.symlink_target.is_empty() {
                return Err(FormatError::InvalidField);
            }
            let entries = wire
                .entries
                .into_iter()
                .map(decode_tree_entry)
                .collect::<Result<Vec<_>, _>>()?;
            DirectoryNode::new(metadata, entries)
                .map(TreeNode::Directory)
                .map_err(map_tree_validation_error)
        }
        TreeNodeKind::RegularFile => {
            if !wire.entries.is_empty() || !wire.symlink_target.is_empty() {
                return Err(FormatError::InvalidField);
            }
            if wire.chunks.len() > MAX_FILE_CHUNK_REFERENCES {
                return Err(FormatError::InvalidField);
            }
            let chunks = wire
                .chunks
                .into_iter()
                .map(decode_file_chunk)
                .collect::<Result<Vec<_>, _>>()?;
            RegularFileNode::new(wire.file_size, chunks, metadata)
                .map(TreeNode::RegularFile)
                .map_err(map_tree_validation_error)
        }
        TreeNodeKind::SymbolicLink => {
            if !wire.entries.is_empty() || wire.file_size != 0 || !wire.chunks.is_empty() {
                return Err(FormatError::InvalidField);
            }
            let target =
                SymlinkTarget::new(wire.symlink_target).map_err(|_| FormatError::InvalidField)?;
            Ok(TreeNode::SymbolicLink(SymbolicLinkNode::new(
                target, metadata,
            )))
        }
    }
}

fn decode_metadata(wire: MetadataWireOwned) -> Result<PortableMetadata, FormatError> {
    if wire.metadata_version != CURRENT_TREE_METADATA_VERSION {
        return Err(FormatError::UnsupportedObjectVersion {
            version: wire.metadata_version,
        });
    }
    if wire.extensions.len() > MAX_METADATA_EXTENSIONS {
        return Err(FormatError::InvalidField);
    }
    let permissions =
        FilePermissions::new(wire.permissions).map_err(|_| FormatError::InvalidField)?;
    let mut metadata = PortableMetadata::new(permissions);
    if let Some(modified_at) = wire.modified_at.0 {
        metadata = metadata.with_modified_at(modified_at);
    }
    for extension in wire.extensions {
        if extension.namespace.len() > MAX_METADATA_NAMESPACE_BYTES
            || extension.value.len() > MAX_METADATA_EXTENSION_BYTES
        {
            return Err(FormatError::InvalidField);
        }
        let namespace =
            MetadataNamespace::new(extension.namespace).map_err(|_| FormatError::InvalidField)?;
        metadata = metadata
            .with_extension(namespace, extension.version, extension.value)
            .map_err(|_| FormatError::InvalidField)?;
    }
    Ok(metadata)
}

fn decode_tree_entry(wire: DirectoryEntryWireOwned) -> Result<TreeEntry, FormatError> {
    let kind = TreeNodeKind::parse(&wire.node_kind).ok_or(FormatError::InvalidField)?;
    let node_id = decode_object_id(&wire.node_id)?;
    TreeEntry::new(wire.name, TreeNodeReference::new(node_id, kind))
        .map_err(map_tree_validation_error)
}

fn decode_file_chunk(wire: FileChunkWireOwned) -> Result<FileChunkReference, FormatError> {
    let chunk_id: [u8; 32] = wire
        .chunk_id
        .as_slice()
        .try_into()
        .map_err(|_| FormatError::InvalidDigestLength)?;
    FileChunkReference::new(crate::domain::ChunkId::from_digest(chunk_id), wire.size)
        .map_err(|_| FormatError::InvalidField)
}

fn decode_object_id(bytes: &[u8]) -> Result<ObjectId, FormatError> {
    let digest: [u8; 32] = bytes
        .try_into()
        .map_err(|_| FormatError::InvalidDigestLength)?;
    Ok(ObjectId::from_digest(digest))
}

fn map_tree_validation_error(error: TreeValidationError) -> FormatError {
    let _ = error;
    FormatError::InvalidField
}

fn encode_wire<T: Serialize>(value: &T) -> Result<Vec<u8>, FormatError> {
    let mut bytes = Vec::new();
    let mut serializer = rmp_serde::Serializer::new(&mut bytes)
        .with_struct_map()
        .with_bytes(BytesMode::ForceAll);
    value
        .serialize(&mut serializer)
        .map_err(|_| FormatError::Serialization)?;
    if bytes.len() > MAX_TREE_PAYLOAD_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    Ok(bytes)
}
