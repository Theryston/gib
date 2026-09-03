use std::fmt;

use super::configuration::MAX_BACKUP_CONCURRENCY;

/// The default request memory budget for a backup pipeline.
pub const DEFAULT_BACKUP_MEMORY_BYTES: usize = 512 * 1024 * 1024;

/// The default total number of CPU-bound workers in a backup pipeline.
pub const DEFAULT_BACKUP_CPU_WORKERS: usize = 4;

/// The default number of simultaneously owned filesystem descriptors.
pub const DEFAULT_BACKUP_FILE_DESCRIPTORS: usize = 32;

/// The default number of concurrent storage requests.
pub const DEFAULT_BACKUP_NETWORK_REQUESTS: usize = 4;

/// The default capacity of every inter-stage queue.
pub const DEFAULT_BACKUP_QUEUE_CAPACITY: usize = 8;

/// The largest queue capacity accepted by a backup request.
pub const MAX_BACKUP_QUEUE_CAPACITY: usize = 1_024;

/// The minimum number of concurrent CPU permits required by a backup.
pub const MIN_BACKUP_CPU_WORKERS: usize = 1;

/// The minimum number of file descriptors required by the pipeline scheduler.
pub const MIN_BACKUP_FILE_DESCRIPTORS: usize = 4;

/// The minimum number of network requests required to upload immutable data.
pub const MIN_BACKUP_NETWORK_REQUESTS: usize = 1;

/// A bounded request-level resource policy for a backup.
///
/// The memory value is a resident-byte budget shared by all stage messages,
/// worker scratch buffers, tree accumulators, pack builders, and index shards.
/// CPU workers are shared permits acquired around bounded CPU-heavy units;
/// they are not a per-file or per-chunk allowance. File descriptors and
/// network requests are permits held for the lifetime of the corresponding
/// filesystem or storage operation. Queue capacity applies to every
/// inter-stage channel, so increasing one resource does not silently create an
/// unbounded queue elsewhere.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BackupBudgets {
    memory_bytes: usize,
    cpu_workers: usize,
    file_descriptors: usize,
    network_requests: usize,
    queue_capacity: usize,
}

impl BackupBudgets {
    /// Creates an explicit resource policy.
    pub fn new(
        memory_bytes: usize,
        cpu_workers: usize,
        file_descriptors: usize,
        network_requests: usize,
    ) -> Result<Self, BackupBudgetError> {
        Self::with_queue_capacity(
            memory_bytes,
            cpu_workers,
            file_descriptors,
            network_requests,
            DEFAULT_BACKUP_QUEUE_CAPACITY,
        )
    }

    /// Creates an explicit resource policy including inter-stage queue size.
    pub fn with_queue_capacity(
        memory_bytes: usize,
        cpu_workers: usize,
        file_descriptors: usize,
        network_requests: usize,
        queue_capacity: usize,
    ) -> Result<Self, BackupBudgetError> {
        if memory_bytes == 0 {
            return Err(BackupBudgetError::MemoryMustBePositive);
        }
        if cpu_workers < MIN_BACKUP_CPU_WORKERS {
            return Err(BackupBudgetError::CpuWorkersBelowMinimum {
                minimum: MIN_BACKUP_CPU_WORKERS,
            });
        }
        if cpu_workers > MAX_BACKUP_CONCURRENCY {
            return Err(BackupBudgetError::CpuWorkersExceedLimit {
                maximum: MAX_BACKUP_CONCURRENCY,
            });
        }
        if file_descriptors < MIN_BACKUP_FILE_DESCRIPTORS {
            return Err(BackupBudgetError::FileDescriptorsBelowMinimum {
                minimum: MIN_BACKUP_FILE_DESCRIPTORS,
            });
        }
        if network_requests < MIN_BACKUP_NETWORK_REQUESTS {
            return Err(BackupBudgetError::NetworkRequestsMustBePositive);
        }
        if network_requests > MAX_BACKUP_CONCURRENCY {
            return Err(BackupBudgetError::NetworkRequestsExceedLimit {
                maximum: MAX_BACKUP_CONCURRENCY,
            });
        }
        if queue_capacity == 0 {
            return Err(BackupBudgetError::QueueCapacityMustBePositive);
        }
        if queue_capacity > MAX_BACKUP_QUEUE_CAPACITY {
            return Err(BackupBudgetError::QueueCapacityExceedsLimit {
                maximum: MAX_BACKUP_QUEUE_CAPACITY,
            });
        }
        Ok(Self {
            memory_bytes,
            cpu_workers,
            file_descriptors,
            network_requests,
            queue_capacity,
        })
    }

    /// Returns the default backup resource policy.
    pub const fn default_policy() -> Self {
        Self {
            memory_bytes: DEFAULT_BACKUP_MEMORY_BYTES,
            cpu_workers: DEFAULT_BACKUP_CPU_WORKERS,
            file_descriptors: DEFAULT_BACKUP_FILE_DESCRIPTORS,
            network_requests: DEFAULT_BACKUP_NETWORK_REQUESTS,
            queue_capacity: DEFAULT_BACKUP_QUEUE_CAPACITY,
        }
    }

    /// Returns the shared resident-memory limit in bytes.
    pub const fn memory_bytes(self) -> usize {
        self.memory_bytes
    }

    /// Returns the total CPU-bound worker limit.
    pub const fn cpu_workers(self) -> usize {
        self.cpu_workers
    }

    /// Returns the filesystem descriptor limit.
    pub const fn file_descriptors(self) -> usize {
        self.file_descriptors
    }

    /// Returns the concurrent storage-request limit.
    pub const fn network_requests(self) -> usize {
        self.network_requests
    }

    /// Returns the capacity of each inter-stage queue.
    pub const fn queue_capacity(self) -> usize {
        self.queue_capacity
    }
}

impl Default for BackupBudgets {
    fn default() -> Self {
        Self::default_policy()
    }
}

/// A validation failure for a backup resource policy.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackupBudgetError {
    /// The resident-memory limit is zero.
    MemoryMustBePositive,
    /// The pipeline needs at least one CPU permit.
    CpuWorkersBelowMinimum {
        /// The smallest supported CPU pool.
        minimum: usize,
    },
    /// The CPU worker pool exceeds the fixed-worker safety limit.
    CpuWorkersExceedLimit {
        /// The largest accepted CPU worker pool.
        maximum: usize,
    },
    /// The descriptor pool cannot reserve scanner, spool, reader, and storage
    /// ownership.
    FileDescriptorsBelowMinimum {
        /// The smallest supported descriptor pool.
        minimum: usize,
    },
    /// At least one network request is required.
    NetworkRequestsMustBePositive,
    /// The network worker pool exceeds the fixed-worker safety limit.
    NetworkRequestsExceedLimit {
        /// The largest accepted network worker pool.
        maximum: usize,
    },
    /// Every inter-stage queue needs at least one slot.
    QueueCapacityMustBePositive,
    /// Queue capacity exceeded the SDK safety limit.
    QueueCapacityExceedsLimit {
        /// The largest accepted queue capacity.
        maximum: usize,
    },
}

impl fmt::Display for BackupBudgetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MemoryMustBePositive => {
                formatter.write_str("backup memory budget must be greater than zero")
            }
            Self::CpuWorkersBelowMinimum { minimum } => write!(
                formatter,
                "backup CPU worker budget must be at least {minimum}"
            ),
            Self::CpuWorkersExceedLimit { maximum } => write!(
                formatter,
                "backup CPU worker budget must not exceed {maximum}"
            ),
            Self::FileDescriptorsBelowMinimum { minimum } => write!(
                formatter,
                "backup file-descriptor budget must be at least {minimum}"
            ),
            Self::NetworkRequestsMustBePositive => {
                formatter.write_str("backup network-request budget must be greater than zero")
            }
            Self::NetworkRequestsExceedLimit { maximum } => write!(
                formatter,
                "backup network-request budget must not exceed {maximum}"
            ),
            Self::QueueCapacityMustBePositive => {
                formatter.write_str("backup queue capacity must be greater than zero")
            }
            Self::QueueCapacityExceedsLimit { maximum } => {
                write!(formatter, "backup queue capacity must not exceed {maximum}")
            }
        }
    }
}

impl std::error::Error for BackupBudgetError {}

/// A stage in the bounded backup pipeline.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BackupStage {
    /// Filesystem discovery and ordering.
    Scan,
    /// Verified file opening and bounded reads.
    Read,
    /// Content-defined chunk assembly.
    Chunk,
    /// Plaintext content hashing and ID verification.
    Hash,
    /// Compression and authenticated encryption.
    Transform,
    /// Ordered pack assembly.
    Pack,
    /// Disk-spooled index sorting and shard encoding.
    Index,
    /// Immutable object writes to storage.
    Upload,
    /// Final snapshot creation and HEAD publication.
    Publish,
    /// Pipeline coordination and worker lifecycle.
    Coordinator,
}

impl fmt::Display for BackupStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Scan => "scan",
            Self::Read => "read",
            Self::Chunk => "chunk",
            Self::Hash => "hash",
            Self::Transform => "transform",
            Self::Pack => "pack",
            Self::Index => "index",
            Self::Upload => "upload",
            Self::Publish => "publish",
            Self::Coordinator => "coordinator",
        };
        formatter.write_str(value)
    }
}

/// A resource category protected by a backup budget.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BackupResource {
    /// Resident heap and owned byte-buffer capacity.
    Memory,
    /// Fixed CPU worker slots.
    CpuWorkers,
    /// Open filesystem handles, index spool, and storage-call ownership.
    FileDescriptors,
    /// Concurrent storage calls.
    NetworkRequests,
    /// Inter-stage queue slots.
    Queue,
}

impl fmt::Display for BackupResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Memory => "memory",
            Self::CpuWorkers => "cpu_workers",
            Self::FileDescriptors => "file_descriptors",
            Self::NetworkRequests => "network_requests",
            Self::Queue => "queue",
        };
        formatter.write_str(value)
    }
}

/// Immutable counters and observed maxima returned by a completed backup.
///
/// Peak fields are collected at permit acquisition time, so they describe
/// bounded ownership or reservation maxima rather than the number of
/// configured workers. The values are suitable for assertions in integration
/// tests and operational telemetry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BackupMetrics {
    scanned_entries: u64,
    files: u64,
    directories: u64,
    bytes_read: u64,
    total_size: u64,
    chunks: u64,
    transformed_chunks: u64,
    packs: u64,
    index_shards: u64,
    uploaded_objects: u64,
    peak_memory_bytes: usize,
    peak_open_file_descriptors: usize,
    peak_network_requests: usize,
    peak_cpu_workers: usize,
    peak_hash_workers: usize,
    peak_transform_workers: usize,
}

impl BackupMetrics {
    /// Returns the number of entries observed by the scanner.
    pub const fn scanned_entries(self) -> u64 {
        self.scanned_entries
    }

    /// Returns the number of regular files captured.
    pub const fn files(self) -> u64 {
        self.files
    }

    /// Returns the number of directory entries captured.
    pub const fn directories(self) -> u64 {
        self.directories
    }

    /// Returns the logical bytes read from regular files.
    pub const fn bytes_read(self) -> u64 {
        self.bytes_read
    }

    /// Returns the total logical size represented by captured regular files.
    pub const fn total_size(self) -> u64 {
        self.total_size
    }

    /// Returns the number of plaintext chunks assembled.
    pub const fn chunks(self) -> u64 {
        self.chunks
    }

    /// Returns the number of transformed chunks handed to the packer.
    pub const fn transformed_chunks(self) -> u64 {
        self.transformed_chunks
    }

    /// Returns the number of sealed packs.
    pub const fn packs(self) -> u64 {
        self.packs
    }

    /// Returns the number of encoded index shards.
    pub const fn index_shards(self) -> u64 {
        self.index_shards
    }

    /// Returns the number of immutable objects successfully uploaded.
    pub const fn uploaded_objects(self) -> u64 {
        self.uploaded_objects
    }

    /// Returns the largest resident-memory ownership observed in bytes.
    pub const fn peak_memory_bytes(self) -> usize {
        self.peak_memory_bytes
    }

    /// Returns the largest simultaneous descriptor ownership observed.
    pub const fn peak_open_file_descriptors(self) -> usize {
        self.peak_open_file_descriptors
    }

    /// Returns the largest simultaneous storage-request ownership observed.
    pub const fn peak_network_requests(self) -> usize {
        self.peak_network_requests
    }

    /// Returns the largest simultaneous CPU permit ownership observed.
    pub const fn peak_cpu_workers(self) -> usize {
        self.peak_cpu_workers
    }

    /// Returns the largest simultaneous hash-worker ownership observed.
    pub const fn peak_hash_workers(self) -> usize {
        self.peak_hash_workers
    }

    /// Returns the largest simultaneous transform-worker ownership observed.
    pub const fn peak_transform_workers(self) -> usize {
        self.peak_transform_workers
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) const fn from_parts(
        scanned_entries: u64,
        files: u64,
        directories: u64,
        bytes_read: u64,
        total_size: u64,
        chunks: u64,
        transformed_chunks: u64,
        packs: u64,
        index_shards: u64,
        uploaded_objects: u64,
        peak_memory_bytes: usize,
        peak_open_file_descriptors: usize,
        peak_network_requests: usize,
        peak_cpu_workers: usize,
        peak_hash_workers: usize,
        peak_transform_workers: usize,
    ) -> Self {
        Self {
            scanned_entries,
            files,
            directories,
            bytes_read,
            total_size,
            chunks,
            transformed_chunks,
            packs,
            index_shards,
            uploaded_objects,
            peak_memory_bytes,
            peak_open_file_descriptors,
            peak_network_requests,
            peak_cpu_workers,
            peak_hash_workers,
            peak_transform_workers,
        }
    }
}
