use gib::{
    BuiltTree, ChunkId, DirectoryNode, EntryName, FileChunkReference, FilePermissions,
    IncrementalTreeBuilder, LazyTree, MemoryStorage, MetadataNamespace, ObjectId, PortableMetadata,
    RegularFileNode, RelativePath, RepositoryTreeStore, SymbolicLinkNode, SymlinkTarget,
    TreeBuilder, TreeEntry, TreeNode, TreeNodeKind, TreeNodeReference, TreeNodeStore,
    TreeObjectPublisher, TreeTraversalError, TreeValidationError, decode_tree_node_object,
    encode_tree_node_object, tree_node_object_id,
};
use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn metadata(mode: u32) -> Result<PortableMetadata, Box<dyn Error>> {
    Ok(PortableMetadata::new(FilePermissions::new(mode)?))
}

fn file_node(content: &[u8], mode: u32) -> Result<TreeNode, Box<dyn Error>> {
    let length = u64::try_from(content.len())?;
    let chunk = FileChunkReference::new(ChunkId::from_content(content), length)?;
    Ok(TreeNode::RegularFile(RegularFileNode::new(
        length,
        [chunk],
        metadata(mode)?,
    )?))
}

fn empty_file_node(mode: u32) -> Result<TreeNode, Box<dyn Error>> {
    Ok(TreeNode::RegularFile(RegularFileNode::new(
        0,
        [],
        metadata(mode)?,
    )?))
}

fn fixture() -> Result<(BuiltTree, TreeNode), Box<dyn Error>> {
    let file = file_node(b"old deep content", 0o644)?;
    let empty = TreeNode::Directory(DirectoryNode::empty(metadata(0o755)?));
    let symlink = TreeNode::SymbolicLink(SymbolicLinkNode::new(
        SymlinkTarget::new(b"../target/file")?,
        metadata(0o777)?,
    ));

    let mut builder = TreeBuilder::new();
    let file_reference = builder.add_node(&file)?;
    let empty_reference = builder.add_node(&empty)?;
    let symlink_reference = builder.add_node(&symlink)?;
    let nested = TreeNode::Directory(DirectoryNode::new(
        metadata(0o755)?,
        [TreeEntry::new("deep.txt", file_reference.clone())?],
    )?);
    let nested_reference = builder.add_node(&nested)?;
    let root = TreeNode::Directory(DirectoryNode::new(
        metadata(0o755)?,
        [
            TreeEntry::new("nested", nested_reference)?,
            TreeEntry::new("empty", empty_reference.clone())?,
            TreeEntry::new("link", symlink_reference)?,
            TreeEntry::new("readme", file_reference)?,
            TreeEntry::new("empty-copy", empty_reference)?,
        ],
    )?);
    let root_reference = builder.add_node(&root)?;
    Ok((builder.finish(root_reference)?, root))
}

fn publish(tree: &BuiltTree, storage: &MemoryStorage) -> Result<(), Box<dyn Error>> {
    TreeObjectPublisher::new(storage.clone()).publish_all(tree)?;
    Ok(())
}

#[derive(Clone)]
struct MapStore {
    nodes: Arc<BTreeMap<ObjectId, TreeNode>>,
    loads: Arc<AtomicUsize>,
}

#[derive(Debug, Eq, PartialEq)]
struct TestStoreError(&'static str);

impl std::fmt::Display for TestStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for TestStoreError {}

impl TreeNodeStore for MapStore {
    type Error = TestStoreError;

    fn load(&self, reference: &TreeNodeReference) -> Result<TreeNode, Self::Error> {
        self.loads.fetch_add(1, Ordering::Relaxed);
        self.nodes
            .get(reference.id())
            .cloned()
            .ok_or(TestStoreError("tree node is missing"))
    }
}

fn map_store(tree: &BuiltTree) -> Result<MapStore, Box<dyn Error>> {
    let mut nodes = BTreeMap::new();
    for object in tree.objects() {
        nodes.insert(
            object.id().clone(),
            decode_tree_node_object(object.bytes())?,
        );
    }
    Ok(MapStore {
        nodes: Arc::new(nodes),
        loads: Arc::new(AtomicUsize::new(0)),
    })
}

#[test]
fn equivalent_directory_trees_have_identical_bytes_and_root_ids() -> Result<(), Box<dyn Error>> {
    let file = file_node(b"same", 0o644)?;
    let link = TreeNode::SymbolicLink(SymbolicLinkNode::new(
        SymlinkTarget::new(b"../same")?,
        metadata(0o777)?,
    ));
    let empty = TreeNode::Directory(DirectoryNode::empty(metadata(0o755)?));
    let mut builder = TreeBuilder::new();
    let references = [
        ("zeta", builder.add_node(&file)?),
        ("alpha", builder.add_node(&empty)?),
        ("café", builder.add_node(&link)?),
        ("🙂", builder.add_node(&file)?),
    ];
    let entries: Vec<_> = references
        .iter()
        .map(|(name, reference)| TreeEntry::new(*name, reference.clone()))
        .collect::<Result<_, _>>()?;
    let expected = TreeNode::Directory(DirectoryNode::new(metadata(0o755)?, entries)?);
    let expected_bytes = encode_tree_node_object(&expected)?;
    let expected_id = tree_node_object_id(&expected)?;

    let mut order = vec![0_usize, 1, 2, 3];
    let mut observed = 0_usize;
    permute(
        &mut order,
        0,
        &references,
        &expected_id,
        &expected_bytes,
        &mut observed,
    )?;
    assert_eq!(observed, 24);
    Ok(())
}

fn permute(
    order: &mut [usize],
    start: usize,
    references: &[(&str, TreeNodeReference); 4],
    expected_id: &ObjectId,
    expected_bytes: &[u8],
    observed: &mut usize,
) -> Result<(), Box<dyn Error>> {
    if start == order.len() {
        let entries: Vec<_> = order
            .iter()
            .map(|index| {
                let (name, reference) = &references[*index];
                TreeEntry::new(*name, reference.clone())
            })
            .collect::<Result<_, _>>()?;
        let node = TreeNode::Directory(DirectoryNode::new(metadata(0o755)?, entries)?);
        assert_eq!(tree_node_object_id(&node)?, *expected_id);
        assert_eq!(encode_tree_node_object(&node)?, expected_bytes);
        *observed += 1;
        return Ok(());
    }
    for index in start..order.len() {
        order.swap(start, index);
        permute(
            order,
            start + 1,
            references,
            expected_id,
            expected_bytes,
            observed,
        )?;
        order.swap(start, index);
    }
    Ok(())
}

#[test]
fn empty_directories_and_symlinks_round_trip_without_following_targets()
-> Result<(), Box<dyn Error>> {
    let (tree, root) = fixture()?;
    for object in tree.objects() {
        let decoded = decode_tree_node_object(object.bytes())?;
        assert_eq!(decoded.object_id()?, *object.id());
        assert_eq!(decoded.kind(), object.kind());
    }
    let root = root
        .as_directory()
        .ok_or("fixture root must be a directory")?;
    let empty = root
        .entry(&EntryName::new("empty")?)
        .ok_or("empty directory entry is missing")?;
    assert_eq!(empty.kind(), TreeNodeKind::Directory);
    let link = root
        .entry(&EntryName::new("link")?)
        .ok_or("symlink entry is missing")?;
    assert_eq!(link.kind(), TreeNodeKind::SymbolicLink);

    let symlink = TreeNode::SymbolicLink(SymbolicLinkNode::new(
        SymlinkTarget::new(b"/outside/../target")?,
        metadata(0o777)?,
    ));
    let decoded = decode_tree_node_object(&encode_tree_node_object(&symlink)?)?;
    assert_eq!(
        decoded
            .as_symbolic_link()
            .ok_or("decoded node is not a symlink")?
            .target()
            .as_bytes(),
        b"/outside/../target"
    );
    Ok(())
}

#[test]
fn one_leaf_change_rewrites_only_its_ancestor_chain() -> Result<(), Box<dyn Error>> {
    let (tree, _) = fixture()?;
    let storage = MemoryStorage::new();
    publish(&tree, &storage)?;
    let store = RepositoryTreeStore::new(storage.clone());
    let changed = file_node(b"new deep content", 0o644)?;
    let path = RelativePath::new("nested/deep.txt")?;
    let rebuilt = IncrementalTreeBuilder::new(store.clone()).replace_leaf(
        tree.root().clone(),
        &path,
        &changed,
    )?;
    assert_ne!(rebuilt.root_id(), tree.root_id());
    assert_eq!(rebuilt.objects().len(), 3);
    publish(&rebuilt, &storage)?;

    let old_root = store.load(tree.root())?;
    let new_root = store.load(rebuilt.root())?;
    let old_root = old_root
        .as_directory()
        .ok_or("old root is not a directory")?;
    let new_root = new_root
        .as_directory()
        .ok_or("new root is not a directory")?;
    for name in ["empty", "empty-copy", "link", "readme"] {
        let name = EntryName::new(name)?;
        let old_entry = old_root
            .entry(&name)
            .ok_or("old unrelated entry is missing")?;
        let new_entry = new_root
            .entry(&name)
            .ok_or("new unrelated entry is missing")?;
        assert_eq!(old_entry.reference(), new_entry.reference());
    }
    let old_nested = old_root
        .entry(&EntryName::new("nested")?)
        .ok_or("old nested directory is missing")?;
    let new_nested = new_root
        .entry(&EntryName::new("nested")?)
        .ok_or("new nested directory is missing")?;
    assert_ne!(old_nested.reference(), new_nested.reference());

    let new_tree = LazyTree::new(rebuilt.root_id().clone(), store);
    let node = new_tree
        .lookup(&path)
        .map_err(|error| error.to_string())?
        .ok_or("changed leaf is missing")?;
    assert_eq!(
        node.as_regular_file()
            .ok_or("changed node is not a file")?
            .size(),
        16
    );
    Ok(())
}

#[test]
fn incremental_update_supports_add_delete_and_type_replacement() -> Result<(), Box<dyn Error>> {
    let (tree, _) = fixture()?;
    let storage = MemoryStorage::new();
    publish(&tree, &storage)?;
    let store = RepositoryTreeStore::new(storage.clone());
    let builder = IncrementalTreeBuilder::new(store.clone());

    let deleted = builder.update(tree.root().clone(), &RelativePath::new("readme")?, None)?;
    assert_eq!(deleted.objects().len(), 1);
    publish(&deleted, &storage)?;
    let deleted_tree = LazyTree::new(deleted.root_id().clone(), store.clone());
    assert!(
        deleted_tree
            .lookup(&RelativePath::new("readme")?)?
            .is_none()
    );

    let added = builder.update(
        tree.root().clone(),
        &RelativePath::new("added")?,
        Some(&file_node(b"added content", 0o644)?),
    )?;
    assert_eq!(added.objects().len(), 2);
    publish(&added, &storage)?;
    assert!(
        LazyTree::new(added.root_id().clone(), store.clone())
            .lookup(&RelativePath::new("added")?)?
            .is_some()
    );

    let replaced = builder.update(
        tree.root().clone(),
        &RelativePath::new("empty")?,
        Some(&file_node(b"directory became a file", 0o644)?),
    )?;
    publish(&replaced, &storage)?;
    let node = LazyTree::new(replaced.root_id().clone(), store.clone())
        .lookup(&RelativePath::new("empty")?)?
        .ok_or("replaced node is missing")?;
    assert_eq!(node.kind(), TreeNodeKind::RegularFile);
    assert_eq!(replaced.objects().len(), 2);

    let replacement_link = TreeNode::SymbolicLink(SymbolicLinkNode::new(
        SymlinkTarget::new(b"../replacement")?,
        metadata(0o777)?,
    ));
    let link_replaced = builder.update(
        tree.root().clone(),
        &RelativePath::new("link")?,
        Some(&replacement_link),
    )?;
    publish(&link_replaced, &storage)?;
    let node = LazyTree::new(link_replaced.root_id().clone(), store)
        .lookup(&RelativePath::new("link")?)?
        .ok_or("replaced link is missing")?;
    assert_eq!(node.kind(), TreeNodeKind::SymbolicLink);
    assert_eq!(link_replaced.objects().len(), 2);
    Ok(())
}

#[test]
fn lazy_lookup_and_walk_materialize_only_the_current_path() -> Result<(), Box<dyn Error>> {
    let (tree, _) = fixture()?;
    let store = map_store(&tree)?;
    let loads = store.loads.clone();
    let lazy = LazyTree::new(tree.root_id().clone(), store);
    let path = RelativePath::new("nested/deep.txt")?;
    let node = lazy.lookup(&path)?.ok_or("leaf is missing")?;
    assert!(node.as_regular_file().is_some());
    assert_eq!(loads.load(Ordering::Relaxed), 3);

    let store = map_store(&tree)?;
    let lazy = LazyTree::new(tree.root_id().clone(), store);
    let paths: Vec<_> = lazy
        .walk()
        .map(|item| item.map(|entry| entry.path().as_str().to_owned()))
        .collect::<Result<_, _>>()
        .map_err(|error| error.to_string())?;
    assert_eq!(paths.first().map(String::as_str), Some(""));
    assert!(paths.iter().any(|path| path == "nested/deep.txt"));
    assert!(paths.iter().any(|path| path == "empty"));
    assert!(paths.iter().any(|path| path == "link"));
    assert_eq!(paths.len(), 7);
    Ok(())
}

#[test]
fn invalid_names_paths_sizes_duplicates_and_metadata_are_rejected() -> Result<(), Box<dyn Error>> {
    for invalid in [
        "", ".", "..", "a/b", "a\\b", "a\0b", "CON", "name.", "name ",
    ] {
        assert!(
            EntryName::new(invalid).is_err(),
            "{invalid:?} should be invalid"
        );
    }
    for invalid in ["/absolute", "a//b", "a\\b", "a/../b", "a/./b", "a/"] {
        assert!(
            RelativePath::new(invalid).is_err(),
            "{invalid:?} should be invalid"
        );
    }
    let name = EntryName::new("duplicate")?;
    let reference = TreeNodeReference::directory(ObjectId::from_digest([1; 32]));
    let first = TreeEntry::new(name.as_str(), reference.clone())?;
    let second = TreeEntry::new(name.as_str(), reference)?;
    assert_eq!(
        DirectoryNode::new(metadata(0o755)?, [first, second]).unwrap_err(),
        TreeValidationError::DuplicateEntryName
    );
    let chunk = FileChunkReference::new(ChunkId::from_content(b"chunk"), 5)?;
    assert!(RegularFileNode::new(4, [chunk], metadata(0o644)?).is_err());
    let namespace = MetadataNamespace::new("posix")?;
    let metadata = PortableMetadata::new(FilePermissions::new(0o644)?)
        .with_extension(namespace.clone(), 2, b"new")?
        .with_extension(namespace, 1, b"old")?;
    assert_eq!(metadata.extensions()[0].version(), 1);
    Ok(())
}

#[test]
fn corruption_and_inconsistent_node_kinds_fail_closed() -> Result<(), Box<dyn Error>> {
    let node = TreeNode::Directory(DirectoryNode::empty(metadata(0o755)?));
    let bytes = encode_tree_node_object(&node)?;
    let mut corrupted = bytes.clone();
    let last = corrupted.last_mut().ok_or("encoded tree is empty")?;
    *last ^= 1;
    assert!(decode_tree_node_object(&corrupted).is_err());

    let storage = MemoryStorage::new();
    let id = tree_node_object_id(&node)?;
    let key = format!("trees/{id}");
    storage.put(&key, &bytes)?;
    let store = RepositoryTreeStore::new(storage.clone());
    let wrong_kind = TreeNodeReference::new(id.clone(), TreeNodeKind::RegularFile);
    assert!(store.load(&wrong_kind).is_err());
    let wrong_id = ObjectId::from_digest([9; 32]);
    let wrong_key = format!("trees/{wrong_id}");
    storage.put(&wrong_key, &bytes)?;
    assert!(store.load(&TreeNodeReference::directory(wrong_id)).is_err());

    let cycle_id = ObjectId::from_digest([7; 32]);
    let cycle_node = TreeNode::Directory(DirectoryNode::new(
        metadata(0o755)?,
        [TreeEntry::new(
            "self",
            TreeNodeReference::directory(cycle_id.clone()),
        )?],
    )?);
    #[derive(Clone)]
    struct CycleStore(TreeNode);
    impl TreeNodeStore for CycleStore {
        type Error = TestStoreError;

        fn load(&self, _reference: &TreeNodeReference) -> Result<TreeNode, Self::Error> {
            Ok(self.0.clone())
        }
    }
    let lazy = LazyTree::new(cycle_id, CycleStore(cycle_node));
    let mut walk = lazy.walk();
    assert!(matches!(walk.next(), Some(Ok(_))));
    assert_eq!(walk.next(), Some(Err(TreeTraversalError::Cycle)));
    Ok(())
}

#[test]
fn very_deep_tree_walk_is_iterative() -> Result<(), Box<dyn Error>> {
    const DEPTH: usize = 1_000;
    let mut builder = TreeBuilder::new();
    let leaf = empty_file_node(0o644)?;
    let mut child = builder.add_node(&leaf)?;
    for _ in 0..DEPTH {
        let directory = TreeNode::Directory(DirectoryNode::new(
            metadata(0o755)?,
            [TreeEntry::new("d", child)?],
        )?);
        child = builder.add_node(&directory)?;
    }
    let tree = builder.finish(child)?;
    let store = map_store(&tree)?;
    let lazy = LazyTree::new(tree.root_id().clone(), store);
    let count = lazy
        .walk()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?
        .len();
    assert_eq!(count, DEPTH + 1);
    Ok(())
}

#[test]
fn golden_tree_encoding_and_id_are_stable() -> Result<(), Box<dyn Error>> {
    const GOLDEN_TREE_ID: &str = "3dd8652873ad4f0a8b77e88a565edbc17078cc9ede49eda9bd26a748b73f3f67";
    let metadata = PortableMetadata::new(FilePermissions::new(0o755)?)
        .with_modified_at(-123_456_789)
        .with_extension(MetadataNamespace::new("posix")?, 1, b"uid=1000")?;
    let node = TreeNode::Directory(DirectoryNode::empty(metadata));
    let bytes = encode_tree_node_object(&node)?;
    let expected = decode_hex(include_str!(
        "../../../tests/fixtures/repository/v1/objects/tree-node-envelope.hex"
    ))?;
    assert_eq!(bytes, expected);
    assert_eq!(tree_node_object_id(&node)?.as_str(), GOLDEN_TREE_ID);
    assert_eq!(decode_tree_node_object(&bytes)?, node);
    Ok(())
}

fn decode_hex(value: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let value: String = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if !value.len().is_multiple_of(2) {
        return Err("fixture hex has an odd number of digits".into());
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for index in 0..(value.len() / 2) {
        bytes.push(u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)?);
    }
    Ok(bytes)
}
