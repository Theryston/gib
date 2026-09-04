use super::error::{SdkError, SdkResult};
use crate::application::ports::{ObjectKey, ObjectWriteOptions, StorageError};
use crate::domain::ObjectId;
use crate::format::{
    decode_tree_node, decode_tree_node_with_encryption, encode_tree_node, encode_tree_node_with_id,
    tree_node_id,
};
use std::collections::BTreeMap;
use std::io::{Cursor, Read};

pub use crate::domain::{
    CURRENT_TREE_METADATA_VERSION, ChunkReference, ChunkReferenceError, DirectoryEntry,
    DirectoryNode, EntryName, EntryNameError, FileChunkReference, FileNode, FilePermissions,
    LazyTree, MAX_FILE_CHUNK_REFERENCES, MAX_METADATA_EXTENSION_BYTES, MAX_METADATA_EXTENSIONS,
    MAX_METADATA_NAMESPACE_BYTES, MAX_SYMLINK_TARGET_BYTES, MAX_TREE_ENTRIES, MAX_TREE_NAME_BYTES,
    MAX_TREE_PATH_BYTES, MetadataError, MetadataExtension, MetadataNamespace,
    MetadataNamespaceError, Name, NodeKind, NodeReference, NormalizedRelativePath, PermissionError,
    PortableMetadata, RegularFileNode, RelativePath, RelativePathError, SymbolicLinkNode,
    SymlinkNode, SymlinkTarget, SymlinkTargetError, TreeEntry, TreeNode, TreeNodeId, TreeNodeKind,
    TreeNodeReference, TreeNodeStore, TreeTraversalError, TreeValidationError, TreeWalkEntry,
    TreeWalker, ValidatedName,
};

/// The current version of the canonical tree-node payload.
pub const CURRENT_TREE_NODE_VERSION: u16 = crate::domain::CURRENT_TREE_NODE_VERSION;

/// The largest complete encoded tree object accepted by the lazy repository
/// adapter.
pub const MAX_TREE_OBJECT_BYTES: usize = crate::domain::MAX_IMMUTABLE_OBJECT_BYTES;

/// Encodes one validated node as a content-addressed tree object.
pub fn encode_tree_node_object(node: &TreeNode) -> SdkResult<Vec<u8>> {
    encode_tree_node(node).map_err(super::repository::map_object_format_error)
}

/// Alias for [`encode_tree_node_object`].
pub fn encode_tree(node: &TreeNode) -> SdkResult<Vec<u8>> {
    encode_tree_node_object(node)
}

/// Decodes and validates one content-addressed tree object.
pub fn decode_tree_node_object(bytes: &[u8]) -> SdkResult<TreeNode> {
    decode_tree_node(bytes).map_err(super::repository::map_object_format_error)
}

/// Alias for [`decode_tree_node_object`].
pub fn decode_tree(bytes: &[u8]) -> SdkResult<TreeNode> {
    decode_tree_node_object(bytes)
}

/// Calculates the content ID of a tree node's canonical payload.
pub fn tree_node_object_id(node: &TreeNode) -> SdkResult<ObjectId> {
    tree_node_id(node).map_err(super::repository::map_object_format_error)
}

/// Alias for [`tree_node_object_id`].
pub fn tree_node_id_for_content(node: &TreeNode) -> SdkResult<ObjectId> {
    tree_node_object_id(node)
}

impl TreeNode {
    /// Encodes this node as a versioned immutable tree object.
    pub fn to_bytes(&self) -> SdkResult<Vec<u8>> {
        encode_tree_node_object(self)
    }

    /// Alias for [`Self::to_bytes`].
    pub fn encode(&self) -> SdkResult<Vec<u8>> {
        self.to_bytes()
    }

    /// Returns this node's content-addressed tree object ID.
    pub fn object_id(&self) -> SdkResult<ObjectId> {
        tree_node_object_id(self)
    }

    /// Returns this node's typed tree reference.
    pub fn reference(&self) -> SdkResult<TreeNodeReference> {
        TreeNodeReference::from_node(self)
    }
}

impl TreeNodeReference {
    /// Calculates a typed reference for a validated node.
    pub fn from_node(node: &TreeNode) -> SdkResult<Self> {
        tree_node_object_id(node).map(|id| Self::new(id, node.kind()))
    }
}

/// One encoded immutable tree object ready for publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedTreeNode {
    reference: TreeNodeReference,
    bytes: Vec<u8>,
}

impl EncodedTreeNode {
    fn new(reference: TreeNodeReference, bytes: Vec<u8>) -> Self {
        Self { reference, bytes }
    }

    /// Returns the typed node reference.
    pub fn reference(&self) -> &TreeNodeReference {
        &self.reference
    }

    /// Returns the content-addressed node ID.
    pub fn id(&self) -> &ObjectId {
        self.reference.id()
    }

    /// Returns the node kind.
    pub const fn kind(&self) -> TreeNodeKind {
        self.reference.kind()
    }

    /// Returns the complete immutable object bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the object and returns its complete bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Returns the conventional storage key for this object.
    pub fn object_key(&self) -> SdkResult<ObjectKey> {
        ObjectKey::new(format!("trees/{}", self.id())).map_err(|_| SdkError::InvalidRequest {
            field: "tree_object_key",
            reason: "derived tree object key is invalid",
        })
    }
}

/// A root reference and the newly encoded objects produced by one build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltTree {
    root: TreeNodeReference,
    objects: Vec<EncodedTreeNode>,
}

impl BuiltTree {
    fn new(root: TreeNodeReference, objects: Vec<EncodedTreeNode>) -> Self {
        Self { root, objects }
    }

    /// Returns the root directory reference.
    pub fn root(&self) -> &TreeNodeReference {
        &self.root
    }

    /// Returns the root directory ID.
    pub fn root_id(&self) -> &ObjectId {
        self.root.id()
    }

    /// Returns newly encoded objects in ascending object-ID order.
    pub fn objects(&self) -> &[EncodedTreeNode] {
        &self.objects
    }

    /// Consumes the build and returns its root and encoded objects.
    pub fn into_parts(self) -> (TreeNodeReference, Vec<EncodedTreeNode>) {
        (self.root, self.objects)
    }
}

/// A deterministic builder for immutable tree objects.
///
/// The builder only retains nodes explicitly added during this build. Existing
/// child references can be passed directly into a new directory, so unchanged
/// subtrees are structurally shared without being loaded or re-encoded.
pub struct TreeBuilder {
    objects: BTreeMap<ObjectId, EncodedTreeNode>,
}

impl TreeBuilder {
    /// Creates an empty tree-object builder.
    pub fn new() -> Self {
        Self {
            objects: BTreeMap::new(),
        }
    }

    /// Encodes and records one node, returning its typed reference.
    pub fn add_node(&mut self, node: &TreeNode) -> SdkResult<TreeNodeReference> {
        let (id, bytes) =
            encode_tree_node_with_id(node).map_err(super::repository::map_object_format_error)?;
        let reference = TreeNodeReference::new(id.clone(), node.kind());
        if let Some(existing) = self.objects.get(&id) {
            if existing.bytes() != bytes.as_slice() {
                return Err(SdkError::RepositoryMalformed {
                    reason: "two different tree payloads have the same object ID",
                });
            }
        } else {
            self.objects
                .insert(id, EncodedTreeNode::new(reference.clone(), bytes));
        }
        Ok(reference)
    }

    /// Alias for [`Self::add_node`].
    pub fn add(&mut self, node: &TreeNode) -> SdkResult<TreeNodeReference> {
        self.add_node(node)
    }

    /// Creates, encodes, and records one directory node.
    pub fn add_directory<I>(
        &mut self,
        metadata: PortableMetadata,
        entries: I,
    ) -> SdkResult<TreeNodeReference>
    where
        I: IntoIterator<Item = TreeEntry>,
    {
        let directory = DirectoryNode::new(metadata, entries).map_err(map_tree_validation_error)?;
        self.add_node(&TreeNode::Directory(directory))
    }

    /// Creates, encodes, and records one regular-file node.
    pub fn add_file<I>(
        &mut self,
        size: u64,
        chunks: I,
        metadata: PortableMetadata,
    ) -> SdkResult<TreeNodeReference>
    where
        I: IntoIterator<Item = FileChunkReference>,
    {
        let file =
            RegularFileNode::new(size, chunks, metadata).map_err(map_tree_validation_error)?;
        self.add_node(&TreeNode::RegularFile(file))
    }

    /// Creates, encodes, and records one symbolic-link node.
    pub fn add_symlink(
        &mut self,
        target: SymlinkTarget,
        metadata: PortableMetadata,
    ) -> SdkResult<TreeNodeReference> {
        self.add_node(&TreeNode::SymbolicLink(SymbolicLinkNode::new(
            target, metadata,
        )))
    }

    /// Returns the number of unique objects retained by the build.
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// Iterates over retained objects in ascending object-ID order.
    pub fn objects(&self) -> impl Iterator<Item = &EncodedTreeNode> {
        self.objects.values()
    }

    /// Finishes the build with an explicitly selected root reference.
    pub fn finish(self, root: TreeNodeReference) -> SdkResult<BuiltTree> {
        if !root.kind().is_directory() {
            return Err(SdkError::InvalidRequest {
                field: "tree_root",
                reason: "tree root must reference a directory",
            });
        }
        Ok(BuiltTree::new(root, self.objects.into_values().collect()))
    }

    /// Finishes the build using the supplied root node.
    pub fn finish_with_root(self, root: &TreeNode) -> SdkResult<BuiltTree> {
        let root_reference = TreeNodeReference::from_node(root)?;
        let mut builder = self;
        let root_reference = if builder.objects.contains_key(root_reference.id()) {
            root_reference
        } else {
            builder.add_node(root)?
        };
        builder.finish(root_reference)
    }
}

impl Default for TreeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A repository-backed immutable tree object publisher.
pub struct TreeObjectPublisher {
    storage: super::repository::StorageHandle,
}

impl TreeObjectPublisher {
    /// Wraps a storage backend for create-if-absent tree publication.
    pub fn new<S>(storage: S) -> Self
    where
        S: Into<super::repository::StorageHandle>,
    {
        Self {
            storage: storage.into(),
        }
    }

    /// Returns the retained storage handle.
    pub fn storage(&self) -> super::repository::StorageHandle {
        self.storage.clone()
    }

    /// Publishes one immutable object without overwriting an existing key.
    pub fn publish(&self, object: &EncodedTreeNode) -> SdkResult<()> {
        let key = object.object_key()?;
        let mut source = Cursor::new(object.bytes());
        let expected_size =
            u64::try_from(object.bytes().len()).map_err(|_| SdkError::RepositoryMalformed {
                reason: "encoded tree object size exceeds the supported range",
            })?;
        self.storage
            .as_storage()
            .write_stream(
                &key,
                &mut source,
                ObjectWriteOptions::if_absent().with_expected_size(expected_size),
            )
            .map(|_| ())
            .map_err(|error| map_tree_storage_error(error, "publish_tree"))
    }

    /// Publishes all newly encoded objects in deterministic object-ID order.
    pub fn publish_all(&self, tree: &BuiltTree) -> SdkResult<()> {
        for object in tree.objects() {
            self.publish(object)?;
        }
        Ok(())
    }
}

/// A bounded repository tree source for [`LazyTree`].
#[derive(Clone)]
pub struct RepositoryTreeStore {
    storage: super::repository::StorageHandle,
    encryption: Option<super::repository::RepositoryEncryption>,
}

impl RepositoryTreeStore {
    /// Wraps a storage backend that contains `trees/<object-id>` objects.
    pub fn new<S>(storage: S) -> Self
    where
        S: Into<super::repository::StorageHandle>,
    {
        Self {
            storage: storage.into(),
            encryption: None,
        }
    }

    /// Enables decryption for transformed tree objects.
    pub fn with_encryption(mut self, encryption: super::repository::RepositoryEncryption) -> Self {
        self.encryption = Some(encryption);
        self
    }

    /// Returns the retained storage handle.
    pub fn storage(&self) -> super::repository::StorageHandle {
        self.storage.clone()
    }
}

impl TreeNodeStore for RepositoryTreeStore {
    type Error = SdkError;

    fn load(&self, reference: &TreeNodeReference) -> SdkResult<TreeNode> {
        let key = ObjectKey::new(format!("trees/{}", reference.id())).map_err(|_| {
            SdkError::RepositoryMalformed {
                reason: "tree reference produced an invalid storage key",
            }
        })?;
        let mut object = self
            .storage
            .as_storage()
            .read_stream(&key)
            .map_err(|error| map_tree_storage_error(error, "read_tree"))?;
        let declared_size = object.metadata().size();
        let bytes = read_bounded(object.reader(), declared_size, MAX_TREE_OBJECT_BYTES)?;
        let node = match self.encryption.as_ref() {
            Some(encryption) => decode_tree_node_with_encryption(&bytes, encryption.context()),
            None => decode_tree_node(&bytes),
        }
        .map_err(super::repository::map_object_format_error)?;
        if node.kind() != reference.kind() {
            return Err(SdkError::RepositoryMalformed {
                reason: "tree reference kind does not match decoded node kind",
            });
        }
        let actual_id = tree_node_id(&node).map_err(super::repository::map_object_format_error)?;
        if actual_id != reference.id().clone() {
            return Err(SdkError::RepositoryMalformed {
                reason: "tree object ID does not match its storage reference",
            });
        }
        Ok(node)
    }
}

/// Rebuilds one changed leaf and only its loaded ancestor chain.
pub struct IncrementalTreeBuilder<S> {
    store: S,
}

impl<S> IncrementalTreeBuilder<S>
where
    S: TreeNodeStore<Error = SdkError>,
{
    /// Creates an incremental builder over an existing tree source.
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// Replaces an existing leaf at `path`, preserving all unrelated subtree
    /// references and returning only newly encoded nodes.
    pub fn replace_leaf(
        &self,
        root: TreeNodeReference,
        path: &RelativePath,
        replacement: &TreeNode,
    ) -> SdkResult<BuiltTree> {
        let components: Vec<_> = path.components().collect();
        if path.is_root() || components.is_empty() {
            return Err(SdkError::InvalidRequest {
                field: "path",
                reason: "incremental leaf replacement requires a non-root path",
            });
        }
        let mut current = root.clone();
        for (index, component) in components.iter().enumerate() {
            let node = self.store.load(&current)?;
            let directory = node.as_directory().ok_or(SdkError::RepositoryMalformed {
                reason: "tree path descends through a non-directory node",
            })?;
            let entry = directory.entry(component).ok_or(SdkError::InvalidRequest {
                field: "path",
                reason: "incremental leaf path does not exist",
            })?;
            let child = entry.reference().clone();
            if index + 1 == components.len() && child.kind().is_directory() {
                return Err(SdkError::InvalidRequest {
                    field: "path",
                    reason: "incremental leaf replacement requires a non-directory target",
                });
            }
            if index + 1 < components.len() && !child.kind().is_directory() {
                return Err(SdkError::RepositoryMalformed {
                    reason: "tree path descends through a non-directory reference",
                });
            }
            current = child;
        }
        self.update(root, path, Some(replacement))
    }

    /// Adds, replaces, or removes a path while retaining unrelated subtree
    /// references. A `None` replacement deletes the final path component; an
    /// absent path is a no-op for deletion.
    pub fn update(
        &self,
        root: TreeNodeReference,
        path: &RelativePath,
        replacement: Option<&TreeNode>,
    ) -> SdkResult<BuiltTree> {
        if !root.kind().is_directory() {
            return Err(SdkError::RepositoryMalformed {
                reason: "tree root reference is not a directory",
            });
        }
        if path.is_root() {
            return Err(SdkError::InvalidRequest {
                field: "path",
                reason: "incremental leaf replacement requires a non-root path",
            });
        }

        let components: Vec<_> = path.components().collect();
        let mut current = root.clone();
        let mut ancestors = Vec::with_capacity(components.len());
        for (index, component) in components.iter().enumerate() {
            let node = self.store.load(&current)?;
            let directory = node.as_directory().ok_or(SdkError::RepositoryMalformed {
                reason: "tree path descends through a non-directory node",
            })?;
            let entry = directory.entry(component);
            if index + 1 < components.len() {
                let child = entry.ok_or(SdkError::InvalidRequest {
                    field: "path",
                    reason: "incremental path ancestor does not exist",
                })?;
                let child = child.reference().clone();
                if !child.kind().is_directory() {
                    return Err(SdkError::RepositoryMalformed {
                        reason: "tree path descends through a non-directory reference",
                    });
                }
                ancestors.push((directory.clone(), component.clone()));
                current = child;
                continue;
            }

            let replacement_reference = replacement.map(TreeNode::reference).transpose()?;
            let Some(existing) = entry else {
                let Some(replacement) = replacement else {
                    return TreeBuilder::new().finish(root);
                };
                let mut builder = TreeBuilder::new();
                let replacement_reference = builder.add_node(replacement)?;
                let mut entries = directory.entries().to_vec();
                entries.push(
                    TreeEntry::new(component.clone(), replacement_reference)
                        .map_err(map_tree_validation_error)?,
                );
                let directory = DirectoryNode::new(directory.metadata().clone(), entries)
                    .map_err(map_tree_validation_error)?;
                let child = builder.add_node(&TreeNode::Directory(directory))?;
                return rebuild_ancestor_chain(builder, child, ancestors);
            };

            let old_reference = existing.reference().clone();
            if replacement_reference.as_ref() == Some(&old_reference) {
                return TreeBuilder::new().finish(root);
            }

            let mut builder = TreeBuilder::new();
            let mut entries = directory.entries().to_vec();
            match replacement {
                Some(replacement) => {
                    let replacement_reference = builder.add_node(replacement)?;
                    let index = entries
                        .binary_search_by(|entry| entry.name().cmp(component))
                        .map_err(|_| SdkError::RepositoryMalformed {
                            reason: "tree directory changed while rebuilding its ancestor chain",
                        })?;
                    entries[index] = TreeEntry::new(component.clone(), replacement_reference)
                        .map_err(map_tree_validation_error)?;
                }
                None => {
                    let index = entries
                        .binary_search_by(|entry| entry.name().cmp(component))
                        .map_err(|_| SdkError::RepositoryMalformed {
                            reason: "tree directory changed while rebuilding its ancestor chain",
                        })?;
                    entries.remove(index);
                }
            }
            let directory = DirectoryNode::new(directory.metadata().clone(), entries)
                .map_err(map_tree_validation_error)?;
            let child = builder.add_node(&TreeNode::Directory(directory))?;
            return rebuild_ancestor_chain(builder, child, ancestors);
        }
        Err(SdkError::InvalidRequest {
            field: "path",
            reason: "incremental path has no final component",
        })
    }

    /// Alias for [`Self::replace_leaf`].
    pub fn replace(
        &self,
        root: TreeNodeReference,
        path: &RelativePath,
        replacement: &TreeNode,
    ) -> SdkResult<BuiltTree> {
        self.replace_leaf(root, path, replacement)
    }
}

fn rebuild_ancestor_chain(
    mut builder: TreeBuilder,
    mut child: TreeNodeReference,
    ancestors: Vec<(DirectoryNode, EntryName)>,
) -> SdkResult<BuiltTree> {
    for (ancestor, name) in ancestors.into_iter().rev() {
        let mut entries = ancestor.entries().to_vec();
        let index = entries
            .binary_search_by(|entry| entry.name().cmp(&name))
            .map_err(|_| SdkError::RepositoryMalformed {
                reason: "tree directory changed while rebuilding its ancestor chain",
            })?;
        entries[index] = TreeEntry::new(name, child).map_err(map_tree_validation_error)?;
        let ancestor = DirectoryNode::new(ancestor.metadata().clone(), entries)
            .map_err(map_tree_validation_error)?;
        child = builder.add_node(&TreeNode::Directory(ancestor))?;
    }
    builder.finish(child)
}

fn map_tree_validation_error(error: crate::domain::TreeValidationError) -> SdkError {
    let _ = error;
    SdkError::InvalidRequest {
        field: "tree",
        reason: "tree node validation failed",
    }
}

fn map_tree_storage_error(error: StorageError, operation: &'static str) -> SdkError {
    match error {
        StorageError::NotFound => SdkError::RepositoryRequiredObjectMissing,
        StorageError::AlreadyExists => SdkError::RepositoryPublicationConflict,
        StorageError::UnsupportedCapability => SdkError::StorageCapabilityUnsupported,
        StorageError::Cancelled => SdkError::OperationCancelled { operation_id: None },
        _ => SdkError::StorageFailure { operation },
    }
}

fn read_bounded(reader: &mut dyn Read, declared_size: u64, max_size: usize) -> SdkResult<Vec<u8>> {
    let max_size_u64 = u64::try_from(max_size).map_err(|_| SdkError::RepositoryMalformed {
        reason: "tree object size limit is not representable",
    })?;
    if declared_size > max_size_u64 {
        return Err(SdkError::RepositoryMalformed {
            reason: "tree object exceeds the supported size limit",
        });
    }
    let capacity = usize::try_from(declared_size).map_err(|_| SdkError::RepositoryMalformed {
        reason: "tree object size is not representable on this platform",
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer).map_err(|error| {
            map_tree_storage_error(StorageError::from_io_error(&error), "read_tree")
        })?;
        if read == 0 {
            break;
        }
        if bytes
            .len()
            .checked_add(read)
            .is_none_or(|size| size > max_size)
        {
            return Err(SdkError::RepositoryMalformed {
                reason: "tree object exceeds the supported size limit",
            });
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    if bytes.len() as u64 != declared_size {
        return Err(SdkError::RepositoryMalformed {
            reason: "tree object length does not match storage metadata",
        });
    }
    Ok(bytes)
}

/// Compatibility alias for [`EncodedTreeNode`].
pub type TreeObject = EncodedTreeNode;

/// Compatibility alias for [`BuiltTree`].
pub type SnapshotTree = BuiltTree;
