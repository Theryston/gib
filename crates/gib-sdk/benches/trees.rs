use gib::{
    BuiltTree, ChunkId, DirectoryNode, FileChunkReference, FilePermissions, IncrementalTreeBuilder,
    LazyTree, ObjectId, PortableMetadata, RegularFileNode, RelativePath, SdkError, TreeBuilder,
    TreeEntry, TreeNode, TreeNodeReference, TreeNodeStore, decode_tree_node_object,
};
use std::collections::BTreeMap;
use std::error::Error;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

const DEFAULT_WIDE_ENTRY_COUNT: usize = 8_192;
const DEFAULT_DEEP_LEVELS: usize = 256;
const DEFAULT_REBUILD_RUNS: usize = 32;

#[derive(Clone)]
struct MemoryTreeStore {
    nodes: Arc<BTreeMap<ObjectId, TreeNode>>,
}

impl TreeNodeStore for MemoryTreeStore {
    type Error = SdkError;

    fn load(&self, reference: &TreeNodeReference) -> Result<TreeNode, Self::Error> {
        self.nodes
            .get(reference.id())
            .cloned()
            .ok_or(SdkError::RepositoryRequiredObjectMissing)
    }
}

fn main() {
    let wide_entries = env_usize("GIB_TREE_BENCH_ENTRIES", DEFAULT_WIDE_ENTRY_COUNT);
    let deep_levels = env_usize("GIB_TREE_BENCH_DEPTH", DEFAULT_DEEP_LEVELS);
    let rebuild_runs = env_usize("GIB_TREE_BENCH_REBUILDS", DEFAULT_REBUILD_RUNS);
    if wide_entries == 0 || deep_levels == 0 || rebuild_runs == 0 {
        eprintln!("tree benchmark sizes must be greater than zero");
        return;
    }

    if let Err(error) = run(wide_entries, deep_levels, rebuild_runs) {
        eprintln!("tree benchmark failed: {error}");
    }
}

fn run(wide_entries: usize, deep_levels: usize, rebuild_runs: usize) -> Result<(), Box<dyn Error>> {
    let wide = build_wide_tree(wide_entries)?;
    let wide_store = memory_store(&wide)?;
    let started = Instant::now();
    let mut visited = 0_usize;
    for item in LazyTree::new(wide.root_id().clone(), wide_store.clone()).walk() {
        let item = item?;
        visited += 1;
        black_box(item.reference());
    }
    let traversal_elapsed = started.elapsed();

    let (deep, deep_store, path) = build_deep_tree(deep_levels)?;
    let replacement = regular_file(b"updated tree benchmark leaf")?;
    let started = Instant::now();
    let mut rebuilt_objects = 0_usize;
    let mut rebuilt_root = None;
    for _ in 0..rebuild_runs {
        let rebuilt = IncrementalTreeBuilder::new(deep_store.clone()).replace_leaf(
            deep.root().clone(),
            &path,
            &replacement,
        )?;
        rebuilt_objects = rebuilt.objects().len();
        rebuilt_root = Some(rebuilt.root_id().clone());
        black_box(rebuilt);
    }
    let rebuild_elapsed = started.elapsed();

    println!(
        "tree benchmark wide_entries={} visited={} traversal_ms={:.2} deep_levels={} rebuild_runs={} rebuilt_objects={} rebuild_total_ms={:.2} rebuild_avg_ms={:.2} final_root={}",
        wide_entries,
        black_box(visited),
        traversal_elapsed.as_secs_f64() * 1000.0,
        deep_levels,
        rebuild_runs,
        rebuilt_objects,
        rebuild_elapsed.as_secs_f64() * 1000.0,
        rebuild_elapsed.as_secs_f64() * 1000.0 / rebuild_runs as f64,
        rebuilt_root.as_ref().map_or("<none>", ObjectId::as_str),
    );
    Ok(())
}

fn build_wide_tree(entry_count: usize) -> Result<BuiltTree, Box<dyn Error>> {
    let file = regular_file(b"wide tree benchmark file")?;
    let mut builder = TreeBuilder::new();
    let file_reference = builder.add_node(&file)?;
    let shared_directory = TreeNode::Directory(DirectoryNode::new(
        metadata(0o755)?,
        [TreeEntry::new("file", file_reference)?],
    )?);
    let directory_reference = builder.add_node(&shared_directory)?;
    let mut entries = Vec::with_capacity(entry_count);
    for index in 0..entry_count {
        entries.push(TreeEntry::new(
            format!("directory-{index:08}"),
            directory_reference.clone(),
        )?);
    }
    let root = TreeNode::Directory(DirectoryNode::new(metadata(0o755)?, entries)?);
    builder.finish_with_root(&root).map_err(Into::into)
}

fn build_deep_tree(
    depth: usize,
) -> Result<(BuiltTree, MemoryTreeStore, RelativePath), Box<dyn Error>> {
    let file = regular_file(b"deep tree benchmark file")?;
    let mut builder = TreeBuilder::new();
    let mut child = builder.add_node(&file)?;
    let names: Vec<_> = (0..depth)
        .map(|index| format!("level-{index:04}"))
        .collect();
    for name in names.iter().rev() {
        let directory = TreeNode::Directory(DirectoryNode::new(
            metadata(0o755)?,
            [TreeEntry::new(name, child)?],
        )?);
        child = builder.add_node(&directory)?;
    }
    let tree = builder.finish(child)?;
    let path = RelativePath::new(names.join("/"))?;
    let store = memory_store(&tree)?;
    Ok((tree, store, path))
}

fn memory_store(tree: &BuiltTree) -> Result<MemoryTreeStore, Box<dyn Error>> {
    let mut nodes = BTreeMap::new();
    for object in tree.objects() {
        nodes.insert(
            object.id().clone(),
            decode_tree_node_object(object.bytes())?,
        );
    }
    Ok(MemoryTreeStore {
        nodes: Arc::new(nodes),
    })
}

fn regular_file(content: &[u8]) -> Result<TreeNode, Box<dyn Error>> {
    let size = u64::try_from(content.len())?;
    let chunk = FileChunkReference::new(ChunkId::from_content(content), size)?;
    Ok(TreeNode::RegularFile(RegularFileNode::new(
        size,
        [chunk],
        metadata(0o644)?,
    )?))
}

fn metadata(mode: u32) -> Result<PortableMetadata, Box<dyn Error>> {
    Ok(PortableMetadata::new(FilePermissions::new(mode)?))
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
