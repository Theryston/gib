use std::hint::black_box;
use std::time::Instant;

use gib::{
    CompressionLevel, ObjectCodec, ObjectEncryption, ObjectKind, ObjectTransformOptions,
    RepositoryEncryption, RepositorySalt, SdkResult, encode_immutable_object_with_options,
};

const DEFAULT_SIZE_MIB: u64 = 4;

#[derive(Clone, Copy)]
enum Dataset {
    Repetitive,
    Incompressible,
}

impl Dataset {
    const ALL: [Self; 2] = [Self::Repetitive, Self::Incompressible];

    const fn name(self) -> &'static str {
        match self {
            Self::Repetitive => "repetitive",
            Self::Incompressible => "incompressible",
        }
    }
}

fn main() {
    let size_mib = std::env::var("GIB_TRANSFORM_BENCH_MIB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_SIZE_MIB);
    let Some(length) = size_mib.checked_mul(1024 * 1024) else {
        eprintln!("GIB_TRANSFORM_BENCH_MIB is too large");
        return;
    };
    let Ok(length) = usize::try_from(length) else {
        eprintln!("GIB_TRANSFORM_BENCH_MIB does not fit this platform");
        return;
    };
    let level = match CompressionLevel::new(3) {
        Ok(level) => level,
        Err(error) => {
            eprintln!("could not construct benchmark compression level: {error}");
            return;
        }
    };
    let encryption = match RepositoryEncryption::from_password(
        b"benchmark-password",
        RepositorySalt::from_bytes([7u8; 16]),
    ) {
        Ok(encryption) => encryption,
        Err(error) => {
            eprintln!("could not derive benchmark encryption key: {error}");
            return;
        }
    };

    println!(
        "transform benchmark: size_mib={size_mib} zstd_level={}",
        level.value()
    );
    for dataset in Dataset::ALL {
        let payload = make_payload(dataset, length);
        let plain_options = ObjectTransformOptions::new(ObjectCodec::Zstd, ObjectEncryption::None)
            .with_compression_level(level);
        report(dataset, "zstd", payload.len(), || {
            encode_immutable_object_with_options(ObjectKind::Pack, 1, plain_options, None, &payload)
        });

        let encrypted_options =
            ObjectTransformOptions::new(ObjectCodec::Zstd, ObjectEncryption::XChaCha20Poly1305)
                .with_compression_level(level);
        report(dataset, "zstd+xchacha20-poly1305", payload.len(), || {
            encode_immutable_object_with_options(
                ObjectKind::Pack,
                1,
                encrypted_options,
                Some(&encryption),
                &payload,
            )
        });
    }
}

fn report<F>(dataset: Dataset, transform: &str, input_bytes: usize, run: F)
where
    F: FnOnce() -> SdkResult<Vec<u8>>,
{
    let started = Instant::now();
    match run() {
        Ok(bytes) => {
            let elapsed = started.elapsed();
            let seconds = elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
            let throughput_mib_s = input_bytes as f64 / (1024.0 * 1024.0) / seconds;
            println!(
                "dataset={} transform={transform} input_bytes={input_bytes} output_bytes={} elapsed_ms={} throughput_mib_s={throughput_mib_s:.2}",
                dataset.name(),
                black_box(bytes.len()),
                elapsed.as_secs_f64() * 1000.0,
            );
            black_box(bytes);
        }
        Err(error) => eprintln!(
            "dataset={} transform={transform} input_bytes={input_bytes} failed={error}",
            dataset.name()
        ),
    }
}

fn make_payload(dataset: Dataset, length: usize) -> Vec<u8> {
    let mut payload = Vec::with_capacity(length);
    for index in 0..length {
        let byte = match dataset {
            Dataset::Repetitive => {
                const PATTERN: &[u8] = b"gib-transform-benchmark";
                PATTERN[index % PATTERN.len()]
            }
            Dataset::Incompressible => deterministic_byte(index as u64),
        };
        payload.push(byte);
    }
    payload
}

fn deterministic_byte(position: u64) -> u8 {
    let mut value = position ^ 0x9e37_79b9_7f4a_7c15;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (value ^ (value >> 31)) as u8
}
