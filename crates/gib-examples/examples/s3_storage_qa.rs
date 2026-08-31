#[cfg(feature = "s3")]
mod qa {
    use gib::{
        CancellationHandle, DEFAULT_S3_MULTIPART_PART_SIZE, DEFAULT_S3_MULTIPART_THRESHOLD,
        ObjectKey, ObjectListRequest, ObjectPrefix, ObjectRange, ObjectWriteOptions,
        RepositoryStorage, S3Storage, S3StorageConfig, StorageError,
    };
    use std::error::Error;
    use std::io::{self, Cursor, Read};
    use std::thread;
    use std::time::Duration;

    pub fn run() -> Result<(), Box<dyn Error>> {
        let mut arguments = std::env::args().skip(1);
        let command = arguments.next().unwrap_or_else(|| String::from("all"));
        let storage = configured_storage()?;
        match command.as_str() {
            "all" => {
                capabilities(&storage)?;
                smoke(&storage)?;
                multipart(&storage)?;
                paginate(&storage)?;
                cancel(&storage)
            }
            "capabilities" => capabilities(&storage),
            "reprobe" => reprobe(&storage),
            "atomic" => atomic(&storage),
            "smoke" => smoke(&storage),
            "multipart" => multipart(&storage),
            "paginate" => paginate(&storage),
            "cancel" => cancel(&storage),
            _ => Err(format!(
                "unknown command {command}; use all, capabilities, reprobe, atomic, smoke, multipart, paginate, or cancel"
            )
            .into()),
        }
    }

    fn configured_storage() -> Result<S3Storage, Box<dyn Error>> {
        let region = required_environment("GIB_S3_REGION")?;
        let bucket = required_environment("GIB_S3_BUCKET")?;
        let access_key = required_environment("GIB_S3_ACCESS_KEY")?;
        let secret_key = required_environment("GIB_S3_SECRET_KEY")?;
        let mut config = S3StorageConfig::new(region, bucket, access_key, secret_key)?;
        if let Ok(endpoint) = std::env::var("GIB_S3_ENDPOINT") {
            config = config.with_endpoint(endpoint);
        }
        if let Ok(session_token) = std::env::var("GIB_S3_SESSION_TOKEN") {
            config = config.with_session_token(session_token);
        }
        if let Ok(path) = std::env::var("GIB_S3_CAPABILITY_CACHE_PATH") {
            config = config.with_capability_cache_path(path);
        }
        let threshold =
            optional_environment("GIB_S3_MULTIPART_THRESHOLD", DEFAULT_S3_MULTIPART_THRESHOLD)?;
        let part_size =
            optional_environment("GIB_S3_MULTIPART_PART_SIZE", DEFAULT_S3_MULTIPART_PART_SIZE)?;
        let concurrency = optional_environment("GIB_S3_MAX_CONCURRENCY", 4_usize)?;
        Ok(S3Storage::new(
            config
                .with_multipart_threshold(threshold)
                .with_multipart_part_size(part_size)
                .with_max_concurrency(concurrency),
        )?)
    }

    fn capabilities(storage: &S3Storage) -> Result<(), Box<dyn Error>> {
        if let Some(path) = storage.config().capability_cache_path() {
            println!("capability cache: {}", path.display());
        }
        let initial = storage.conditional_write_capabilities();
        let loaded_from_cache = !matches!(
            initial.create_if_absent(),
            gib::S3ConditionalWriteStatus::Inconclusive
        ) && !matches!(
            initial.replace_if_version(),
            gib::S3ConditionalWriteStatus::Inconclusive
        );
        let first = storage.probe_conditional_write_capabilities()?;
        println!(
            "conditional capabilities: create_if_absent={}, replace_if_version={}",
            first.create_if_absent(),
            first.replace_if_version()
        );
        if loaded_from_cache {
            println!("loaded the capability result from the persistent cache");
        } else {
            println!("performed a provider capability probe and populated the cache");
        }
        Ok(())
    }

    fn reprobe(storage: &S3Storage) -> Result<(), Box<dyn Error>> {
        let capabilities = storage.reprobe_conditional_write_capabilities()?;
        println!(
            "re-probed capabilities: create_if_absent={}, replace_if_version={}",
            capabilities.create_if_absent(),
            capabilities.replace_if_version()
        );
        Ok(())
    }

    fn atomic(storage: &S3Storage) -> Result<(), Box<dyn Error>> {
        let key = ObjectKey::new(format!("manual/s3-atomic-{}", std::process::id()))?;
        let mut source = Cursor::new(b"atomic capability QA".to_vec());
        match storage.write_stream(&key, &mut source, ObjectWriteOptions::if_absent()) {
            Ok(metadata) => {
                println!(
                    "atomic publication accepted; endpoint supports create-if-absent ({})",
                    metadata.key()
                );
                storage.delete(&key)?;
                Ok(())
            }
            Err(StorageError::UnsupportedCapability) => {
                println!("atomic publication refused: conditional writes are unsupported");
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn required_environment(name: &str) -> Result<String, Box<dyn Error>> {
        std::env::var(name).map_err(|_| format!("missing {name}").into())
    }

    fn optional_environment<T>(name: &str, default: T) -> Result<T, Box<dyn Error>>
    where
        T: std::str::FromStr,
        T::Err: Error + 'static,
    {
        match std::env::var(name) {
            Ok(value) => value
                .parse()
                .map_err(|error| format!("invalid {name}: {error}").into()),
            Err(std::env::VarError::NotPresent) => Ok(default),
            Err(error) => Err(format!("could not read {name}: {error}").into()),
        }
    }

    fn remove_if_present(storage: &S3Storage, key: &ObjectKey) -> Result<(), Box<dyn Error>> {
        match storage.delete(key) {
            Ok(()) | Err(StorageError::NotFound) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn smoke(storage: &S3Storage) -> Result<(), Box<dyn Error>> {
        let key = ObjectKey::new("manual/s3-smoke")?;
        remove_if_present(storage, &key)?;
        let mut source = Cursor::new(b"S3 storage smoke test".to_vec());
        let metadata = storage.write_stream(&key, &mut source, ObjectWriteOptions::if_absent())?;
        println!("uploaded {} ({} bytes)", metadata.key(), metadata.size());

        let page = storage.list_page(&ObjectListRequest::new(ObjectPrefix::new("manual")?))?;
        println!("listed {} object(s)", page.objects().len());

        let mut object = storage.read_stream(&key)?;
        let mut contents = Vec::new();
        object.read_to_end(&mut contents)?;
        println!("read: {}", String::from_utf8_lossy(&contents));

        let mut range = storage.read_range(&key, ObjectRange::new(4, 7)?)?;
        let mut range_contents = Vec::new();
        range.read_to_end(&mut range_contents)?;
        println!(
            "range [4, 11): {}",
            String::from_utf8_lossy(&range_contents)
        );

        storage.delete(&key)?;
        println!("deleted {key}");
        Ok(())
    }

    fn multipart(storage: &S3Storage) -> Result<(), Box<dyn Error>> {
        let key = ObjectKey::new("manual/s3-multipart")?;
        remove_if_present(storage, &key)?;
        let part_size = usize::try_from(storage.config().multipart_part_size())?;
        let base_size = part_size
            .checked_mul(3)
            .and_then(|size| size.checked_add(123))
            .ok_or("multipart QA size overflow")?;
        let threshold_size = usize::try_from(
            storage
                .config()
                .multipart_threshold()
                .checked_add(1)
                .ok_or("multipart QA threshold overflow")?,
        )?;
        let size = base_size.max(threshold_size);
        let mut source = PatternReader {
            length: size,
            position: 0,
        };
        let metadata = storage.write_stream(
            &key,
            &mut source,
            ObjectWriteOptions::if_absent().with_expected_size(size as u64),
        )?;
        println!(
            "uploaded multipart object {} ({} bytes)",
            metadata.key(),
            metadata.size()
        );

        let start = part_size
            .checked_sub(17)
            .ok_or("multipart part is too small")?;
        let range = ObjectRange::new(start as u64, 41)?;
        let mut object = storage.read_range(&key, range)?;
        let mut contents = Vec::new();
        object.read_to_end(&mut contents)?;
        let expected: Vec<u8> = (start..start + 41).map(pattern_byte).collect();
        if contents != expected {
            return Err("multipart boundary range did not match the uploaded bytes".into());
        }
        println!("verified range crossing multipart boundary");
        storage.delete(&key)?;
        Ok(())
    }

    fn paginate(storage: &S3Storage) -> Result<(), Box<dyn Error>> {
        let prefix = ObjectPrefix::new("manual/s3-page")?;
        let mut keys = Vec::new();
        for index in 0..5 {
            let key = ObjectKey::new(format!("manual/s3-page/{index}"))?;
            remove_if_present(storage, &key)?;
            let mut source = Cursor::new(index.to_string().into_bytes());
            storage.write_stream(&key, &mut source, ObjectWriteOptions::if_absent())?;
            keys.push(key);
        }

        let mut request = ObjectListRequest::new(prefix).with_limit(2);
        let mut listed = Vec::new();
        loop {
            let page = storage.list_page(&request)?;
            listed.extend(page.objects().iter().map(|object| object.key().to_string()));
            let Some(cursor) = page.next_cursor().cloned() else {
                break;
            };
            request = request.with_cursor(cursor);
        }
        println!(
            "pagination returned {} object(s) across multiple pages",
            listed.len()
        );
        if listed.len() != keys.len() {
            return Err("pagination did not return every QA object".into());
        }
        for key in keys {
            storage.delete(&key)?;
        }
        Ok(())
    }

    fn cancel(storage: &S3Storage) -> Result<(), Box<dyn Error>> {
        let key = ObjectKey::new("manual/s3-cancelled")?;
        remove_if_present(storage, &key)?;
        let part_size = storage.config().multipart_part_size();
        let base_size = part_size
            .checked_mul(3)
            .and_then(|size| size.checked_add(1))
            .ok_or("cancel QA size overflow")?;
        let size = base_size.max(
            storage
                .config()
                .multipart_threshold()
                .checked_add(1)
                .ok_or("cancel QA threshold overflow")?,
        );
        let cancellation = CancellationHandle::new();
        let writer_storage = storage.clone();
        let writer_key = key.clone();
        let writer_cancellation = cancellation.clone();
        let writer = thread::spawn(move || {
            let length = usize::try_from(size).map_err(|_| StorageError::InvalidRequest)?;
            let mut source = SlowPatternReader {
                length,
                position: 0,
                delay: Duration::from_millis(2),
            };
            writer_storage.write_stream_with_cancellation(
                &writer_key,
                &mut source,
                ObjectWriteOptions::if_absent().with_expected_size(size),
                Some(&writer_cancellation),
            )
        });
        thread::sleep(Duration::from_millis(100));
        cancellation.cancel();
        let result = writer.join().map_err(|_| "cancelled writer panicked")?;
        if result != Err(StorageError::Cancelled) {
            return Err(format!("expected cancellation, got {result:?}").into());
        }
        if storage.metadata(&key) != Err(StorageError::NotFound) {
            return Err("cancelled upload left a usable object".into());
        }
        println!("cancelled upload left no usable object");
        Ok(())
    }

    fn pattern_byte(index: usize) -> u8 {
        (index % 251) as u8
    }

    struct PatternReader {
        length: usize,
        position: usize,
    }

    impl Read for PatternReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.position == self.length || buffer.is_empty() {
                return Ok(0);
            }
            let amount = (self.length - self.position).min(buffer.len());
            for (offset, byte) in buffer[..amount].iter_mut().enumerate() {
                *byte = pattern_byte(self.position + offset);
            }
            self.position += amount;
            Ok(amount)
        }
    }

    struct SlowPatternReader {
        length: usize,
        position: usize,
        delay: Duration,
    }

    impl Read for SlowPatternReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let mut reader = PatternReader {
                length: self.length,
                position: self.position,
            };
            let read = reader.read(buffer)?;
            self.position = reader.position;
            if read != 0 {
                thread::sleep(self.delay);
            }
            Ok(read)
        }
    }
}

#[cfg(feature = "s3")]
fn main() {
    if let Err(error) = qa::run() {
        eprintln!("S3 storage QA failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(feature = "s3"))]
fn main() {
    eprintln!("build this example with `--features s3`");
}
