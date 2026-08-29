//! Read-only host and llama.cpp capability detection for AI runtime selection.
//!
//! The detector intentionally has a small surface. It does not invoke a shell,
//! install drivers, or mutate any host state. Memory is reported in bytes and
//! the timestamp is an RFC 3339 UTC sample time so the value can be persisted
//! in structured status events without platform-specific formatting.

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::fs;

pub(crate) const HARDWARE_DETECTOR_VERSION: u32 = 1;
pub(crate) const MEMORY_UNIT: &str = "bytes";

/// The small set of native runtime observations needed by profile selection.
/// The values are supplied by llama.cpp rather than inferred from the host
/// operating system or from compile-time feature flags alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeRuntimeCapabilities {
    pub(crate) cpu: bool,
    pub(crate) gpu_offload: bool,
    pub(crate) mmap: bool,
    pub(crate) mlock: bool,
    pub(crate) gpu_memory_total_bytes: Option<u64>,
    pub(crate) gpu_memory_free_bytes: Option<u64>,
    pub(crate) accelerator_backends: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeCapabilitySnapshot {
    pub(crate) cpu: bool,
    pub(crate) gpu_offload: bool,
    pub(crate) mmap: bool,
    pub(crate) mlock: bool,
    pub(crate) gpu_memory_total_bytes: Option<u64>,
    pub(crate) gpu_memory_free_bytes: Option<u64>,
    pub(crate) accelerator_backends: Vec<String>,
    pub(crate) source: String,
}

/// A capability claim is deliberately split into compile-time availability and
/// runtime usability. A compiled backend is not automatically usable on the
/// current machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceleratorCapability {
    pub(crate) name: String,
    pub(crate) compiled: bool,
    pub(crate) usable: bool,
    pub(crate) source: String,
}

impl AcceleratorCapability {
    fn new(name: impl Into<String>, compiled: bool, usable: bool, source: &str) -> Self {
        Self {
            name: name.into(),
            compiled,
            usable,
            source: source.to_string(),
        }
    }
}

/// Process resource limits relevant to deciding whether a model can be loaded.
/// Unsupported fields remain explicit instead of being guessed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProcessLimits {
    pub(crate) max_address_space_bytes: Option<u64>,
    pub(crate) max_open_files: Option<u64>,
    pub(crate) unavailable: Vec<String>,
    pub(crate) source: String,
}

/// A point-in-time, serializable description of the host relevant to AI
/// runtime selection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct HardwareSnapshot {
    pub(crate) detector_version: u32,
    pub(crate) sampled_at: String,
    pub(crate) memory_unit: String,
    pub(crate) total_memory_bytes: Option<u64>,
    pub(crate) available_memory_bytes: Option<u64>,
    pub(crate) logical_cpu_count: Option<u32>,
    pub(crate) physical_cpu_count: Option<u32>,
    pub(crate) architecture: String,
    pub(crate) operating_system: String,
    pub(crate) runtime_capabilities: RuntimeCapabilitySnapshot,
    pub(crate) accelerators: Vec<AcceleratorCapability>,
    pub(crate) process_limits: ProcessLimits,
}

impl HardwareSnapshot {
    /// Detect the host once at startup. Callers should retain the resulting
    /// snapshot for the lifetime of the runtime instead of repeating detection
    /// in the decode loop.
    pub(crate) fn detect(native: NativeRuntimeCapabilities) -> Self {
        let (total_memory_bytes, available_memory_bytes) = sample_memory();
        let logical_cpu_count = nonzero_cpu_count(num_cpus::get());
        let physical_cpu_count = nonzero_cpu_count(num_cpus::get_physical());

        Self {
            detector_version: HARDWARE_DETECTOR_VERSION,
            sampled_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            memory_unit: MEMORY_UNIT.to_string(),
            total_memory_bytes,
            available_memory_bytes,
            logical_cpu_count,
            physical_cpu_count,
            architecture: std::env::consts::ARCH.to_string(),
            operating_system: std::env::consts::OS.to_string(),
            runtime_capabilities: RuntimeCapabilitySnapshot {
                cpu: native.cpu,
                gpu_offload: native.gpu_offload,
                mmap: native.mmap,
                mlock: native.mlock,
                gpu_memory_total_bytes: native.gpu_memory_total_bytes,
                gpu_memory_free_bytes: native.gpu_memory_free_bytes,
                accelerator_backends: native.accelerator_backends.clone(),
                source: "llama.cpp native runtime capability query".to_string(),
            },
            accelerators: accelerator_capabilities(native),
            process_limits: detect_process_limits(),
        }
    }

    pub(crate) fn usable_gpu_offload(&self) -> bool {
        self.accelerators
            .iter()
            .any(|capability| capability.name == "gpu_offload" && capability.usable)
    }

    pub(crate) fn gpu_can_fit_model(&self, model_bytes: u64) -> Option<bool> {
        match (
            self.runtime_capabilities.gpu_memory_total_bytes,
            self.runtime_capabilities.gpu_memory_free_bytes,
        ) {
            (Some(0) | None, Some(0) | None) => None,
            (_, Some(available)) => Some(available >= model_bytes),
            _ => None,
        }
    }

    pub(crate) fn cpu_count_or_one(&self) -> u32 {
        self.logical_cpu_count.unwrap_or(1).max(1)
    }

    /// A deterministic constructor used by profile tests. Production code
    /// should use [`HardwareSnapshot::detect`].
    #[cfg(test)]
    pub(crate) fn fixture(
        total_memory_bytes: Option<u64>,
        available_memory_bytes: Option<u64>,
        logical_cpu_count: Option<u32>,
        physical_cpu_count: Option<u32>,
        gpu_offload: bool,
    ) -> Self {
        Self {
            detector_version: HARDWARE_DETECTOR_VERSION,
            sampled_at: "2026-01-01T00:00:00.000Z".to_string(),
            memory_unit: MEMORY_UNIT.to_string(),
            total_memory_bytes,
            available_memory_bytes,
            logical_cpu_count,
            physical_cpu_count,
            architecture: "fixture-arch".to_string(),
            operating_system: "fixture-os".to_string(),
            runtime_capabilities: RuntimeCapabilitySnapshot {
                cpu: true,
                gpu_offload,
                mmap: true,
                mlock: false,
                gpu_memory_total_bytes: gpu_offload.then_some(8 * 1024 * 1024 * 1024),
                gpu_memory_free_bytes: gpu_offload.then_some(8 * 1024 * 1024 * 1024),
                accelerator_backends: gpu_offload
                    .then(|| vec!["fixture-gpu".to_string()])
                    .unwrap_or_default(),
                source: "test fixture".to_string(),
            },
            accelerators: vec![AcceleratorCapability::new(
                "gpu_offload",
                gpu_offload,
                gpu_offload,
                "test fixture",
            )],
            process_limits: ProcessLimits {
                max_address_space_bytes: None,
                max_open_files: None,
                unavailable: vec!["fixture".to_string()],
                source: "test fixture".to_string(),
            },
        }
    }
}

fn nonzero_cpu_count(count: usize) -> Option<u32> {
    u32::try_from(count).ok().filter(|count| *count > 0)
}

fn accelerator_capabilities(native: NativeRuntimeCapabilities) -> Vec<AcceleratorCapability> {
    let mut capabilities = vec![
        AcceleratorCapability::new(
            "cpu",
            native.cpu,
            native.cpu,
            "llama.cpp native runtime capability query",
        ),
        AcceleratorCapability::new(
            "gpu_offload",
            native.gpu_offload,
            native.gpu_offload,
            "llama.cpp native supports_gpu_offload query",
        ),
    ];

    // These entries describe what this GIB binary was compiled to try. Their
    // usable flag is still governed by llama.cpp's native runtime result.
    for backend in compiled_accelerator_names() {
        capabilities.push(AcceleratorCapability::new(
            backend,
            true,
            native.gpu_offload,
            "GIB compile feature plus llama.cpp native capability query",
        ));
    }

    capabilities
}

fn compiled_accelerator_names() -> Vec<&'static str> {
    let mut names = Vec::new();
    if cfg!(feature = "ai-cuda") || cfg!(feature = "ai-cuda-no-vmm") {
        names.push("cuda");
    }
    if cfg!(feature = "ai-metal") {
        names.push("metal");
    }
    if cfg!(feature = "ai-vulkan") {
        names.push("vulkan");
    }
    if cfg!(feature = "ai-opencl") {
        names.push("opencl");
    }
    if cfg!(feature = "ai-rocm") {
        names.push("rocm");
    }
    if cfg!(feature = "ai-mkl") {
        names.push("mkl");
    }
    names
}

#[cfg(target_os = "linux")]
fn sample_memory() -> (Option<u64>, Option<u64>) {
    let Ok(contents) = fs::read_to_string("/proc/meminfo") else {
        return (None, None);
    };
    let values = parse_proc_meminfo(&contents);
    (
        values.get("MemTotal").copied(),
        values.get("MemAvailable").copied(),
    )
}

#[cfg(target_os = "linux")]
fn detect_process_limits() -> ProcessLimits {
    let Ok(contents) = fs::read_to_string("/proc/self/limits") else {
        return ProcessLimits {
            max_address_space_bytes: None,
            max_open_files: None,
            unavailable: vec![
                "max_address_space_bytes".to_string(),
                "max_open_files".to_string(),
            ],
            source: "/proc/self/limits unavailable".to_string(),
        };
    };

    let mut max_address_space_bytes = None;
    let mut max_open_files = None;
    for line in contents.lines() {
        if line.starts_with("Max address space") {
            max_address_space_bytes = parse_limit_value(line, 0);
        } else if line.starts_with("Max open files") {
            max_open_files = parse_limit_value(line, 0);
        }
    }
    let mut unavailable = Vec::new();
    if max_address_space_bytes.is_none() {
        unavailable.push("max_address_space_bytes".to_string());
    }
    if max_open_files.is_none() {
        unavailable.push("max_open_files".to_string());
    }
    ProcessLimits {
        max_address_space_bytes,
        max_open_files,
        unavailable,
        source: "/proc/self/limits".to_string(),
    }
}

#[cfg(target_os = "linux")]
fn parse_proc_meminfo(contents: &str) -> BTreeMap<String, u64> {
    let mut values = BTreeMap::new();
    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next().and_then(|value| value.strip_suffix(':')) else {
            continue;
        };
        let Some(value) = parts.next().and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        let multiplier = match parts.next() {
            Some("kB") => 1024,
            Some("MB") => 1024 * 1024,
            Some("GB") => 1024 * 1024 * 1024,
            _ => 1,
        };
        values.insert(name.to_string(), value.saturating_mul(multiplier));
    }
    values
}

#[cfg(target_os = "linux")]
fn parse_limit_value(line: &str, index: usize) -> Option<u64> {
    line.split_whitespace()
        .skip(3 + index)
        .next()
        .and_then(|value| (value != "unlimited").then_some(value))
        .and_then(|value| value.parse::<u64>().ok())
}

#[cfg(target_os = "windows")]
#[repr(C)]
struct MemoryStatusEx {
    dw_length: u32,
    dw_memory_load: u32,
    ull_total_phys: u64,
    ull_avail_phys: u64,
    ull_total_page_file: u64,
    ull_avail_page_file: u64,
    ull_total_virtual: u64,
    ull_avail_virtual: u64,
    ull_avail_extended_virtual: u64,
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
}

#[cfg(target_os = "windows")]
fn sample_memory() -> (Option<u64>, Option<u64>) {
    let mut status = MemoryStatusEx {
        dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
        dw_memory_load: 0,
        ull_total_phys: 0,
        ull_avail_phys: 0,
        ull_total_page_file: 0,
        ull_avail_page_file: 0,
        ull_total_virtual: 0,
        ull_avail_virtual: 0,
        ull_avail_extended_virtual: 0,
    };
    // SAFETY: The structure is initialized with its documented size and a
    // valid mutable pointer for the lifetime of this call.
    let success = unsafe { GlobalMemoryStatusEx(&mut status) } != 0;
    success
        .then_some((Some(status.ull_total_phys), Some(status.ull_avail_phys)))
        .unwrap_or((None, None))
}

#[cfg(target_os = "windows")]
fn detect_process_limits() -> ProcessLimits {
    ProcessLimits {
        max_address_space_bytes: None,
        max_open_files: None,
        unavailable: vec![
            "max_address_space_bytes".to_string(),
            "max_open_files".to_string(),
        ],
        source: "Windows process limit API not queried".to_string(),
    }
}

#[cfg(target_os = "macos")]
#[link(name = "System")]
unsafe extern "C" {
    fn sysctlbyname(
        name: *const std::ffi::c_char,
        oldp: *mut std::ffi::c_void,
        oldlenp: *mut usize,
        newp: *mut std::ffi::c_void,
        newlen: usize,
    ) -> std::ffi::c_int;
}

#[cfg(target_os = "macos")]
fn macos_sysctl_u64(name: &std::ffi::CStr) -> Option<u64> {
    let mut value = 0_u64;
    let mut length = std::mem::size_of::<u64>();
    // SAFETY: `name` is a NUL-terminated constant and the output buffer is
    // valid for the requested size.
    let result = unsafe {
        sysctlbyname(
            name.as_ptr(),
            (&mut value as *mut u64).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0 && length == std::mem::size_of::<u64>()).then_some(value)
}

#[cfg(target_os = "macos")]
fn sample_memory() -> (Option<u64>, Option<u64>) {
    let name = std::ffi::CString::new("hw.memsize").expect("constant sysctl name is valid");
    (macos_sysctl_u64(name.as_c_str()), None)
}

#[cfg(target_os = "macos")]
fn detect_process_limits() -> ProcessLimits {
    ProcessLimits {
        max_address_space_bytes: None,
        max_open_files: None,
        unavailable: vec![
            "available_memory_bytes".to_string(),
            "max_address_space_bytes".to_string(),
            "max_open_files".to_string(),
        ],
        source: "targeted macOS memory query; process limits not queried".to_string(),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn sample_memory() -> (Option<u64>, Option<u64>) {
    (None, None)
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn detect_process_limits() -> ProcessLimits {
    ProcessLimits {
        max_address_space_bytes: None,
        max_open_files: None,
        unavailable: vec![
            "total_memory_bytes".to_string(),
            "available_memory_bytes".to_string(),
            "max_address_space_bytes".to_string(),
            "max_open_files".to_string(),
        ],
        source: "platform support unavailable".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_meminfo_parser_converts_documented_units_to_bytes() {
        let values = parse_proc_meminfo(
            "MemTotal:       8192 kB\nMemAvailable:   4096 kB\nSwapTotal:      1024 kB\n",
        );
        assert_eq!(values["MemTotal"], 8192 * 1024);
        assert_eq!(values["MemAvailable"], 4096 * 1024);
    }

    #[test]
    fn snapshot_serialization_preserves_missing_fields_and_capability_sources() {
        let snapshot = HardwareSnapshot::fixture(None, None, Some(4), None, false);
        let encoded = serde_json::to_string(&snapshot).expect("snapshot should serialize");
        assert!(encoded.contains("fixture-arch"));
        assert!(encoded.contains("gpu_offload"));
        assert!(encoded.contains("\"available_memory_bytes\":null"));
        assert_eq!(snapshot.usable_gpu_offload(), false);
    }
}
