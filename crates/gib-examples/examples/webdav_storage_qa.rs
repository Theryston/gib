#[cfg(feature = "webdav")]
mod qa {
    use gib::{
        CancellationHandle, ObjectKey, ObjectListRequest, ObjectPrefix, ObjectRange,
        ObjectWriteOptions, RepositoryStorage, StorageError, WebDavStorage, WebDavStorageConfig,
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
                smoke(&storage)?;
                unicode(&storage)?;
                ranges(&storage)?;
                paginate(&storage)?;
                cancel(&storage)?;
                redaction(&storage)?;
                authentication()?;
                Ok(())
            }
            "smoke" => smoke(&storage),
            "unicode" => unicode(&storage),
            "ranges" => ranges(&storage),
            "paginate" => paginate(&storage),
            "cancel" => cancel(&storage),
            "redaction" => redaction(&storage),
            "auth" => authentication(),
            _ => Err(format!(
                "unknown command {command}; use all, smoke, unicode, ranges, paginate, cancel, redaction, or auth"
            )
            .into()),
        }
    }

    fn configured_storage() -> Result<WebDavStorage, Box<dyn Error>> {
        let url = required_environment("GIB_WEBDAV_URL")?;
        let username = required_environment("GIB_WEBDAV_USERNAME")?;
        let password = required_environment("GIB_WEBDAV_PASSWORD")?;
        let mut config = WebDavStorageConfig::new(url, username, password)?;
        if environment_is_true("GIB_WEBDAV_ALLOW_HTTP") {
            config = config.with_allow_insecure_http(true);
        }
        if let Ok(value) = std::env::var("GIB_WEBDAV_MAX_CONCURRENCY") {
            config = config.with_max_concurrency(value.parse()?);
        }
        Ok(WebDavStorage::new(config)?)
    }

    fn smoke(storage: &WebDavStorage) -> Result<(), Box<dyn Error>> {
        let key = ObjectKey::new("manual/webdav-smoke/object")?;
        remove_if_present(storage, &key)?;
        let mut source = Cursor::new(b"WebDAV storage smoke test".to_vec());
        let metadata = storage.write_stream(&key, &mut source, ObjectWriteOptions::if_absent())?;
        println!("uploaded {} ({} bytes)", metadata.key(), metadata.size());

        let prefix = ObjectPrefix::new("manual/webdav-smoke")?;
        let listing = storage.list_page(&ObjectListRequest::new(prefix))?;
        require(
            listing.objects().iter().any(|object| object.key() == &key),
            "uploaded WebDAV object was not listed",
        )?;
        println!("listed {} object(s)", listing.objects().len());

        let contents = read_all(storage, &key)?;
        require(
            contents == b"WebDAV storage smoke test",
            "whole-object read mismatch",
        )?;
        let mut range = storage.read_range(&key, ObjectRange::new(7, 7)?)?;
        let mut range_contents = Vec::new();
        range.read_to_end(&mut range_contents)?;
        require(range_contents == b"storage", "range read mismatch")?;
        println!("read whole object and range [7, 14)");

        storage.delete(&key)?;
        require(
            storage.metadata(&key) == Err(StorageError::NotFound),
            "delete failed",
        )?;
        println!("deleted {}", key);
        Ok(())
    }

    fn unicode(storage: &WebDavStorage) -> Result<(), Box<dyn Error>> {
        let key = ObjectKey::new("manual/webdav-unicode/café/雪だるま.txt")?;
        remove_if_present(storage, &key)?;
        let mut source = Cursor::new("Unicode WebDAV object".as_bytes().to_vec());
        storage.write_stream(&key, &mut source, ObjectWriteOptions::if_absent())?;
        let contents = read_all(storage, &key)?;
        require(
            contents == "Unicode WebDAV object".as_bytes(),
            "Unicode object read mismatch",
        )?;
        let listing = storage.list_page(&ObjectListRequest::new(ObjectPrefix::new(
            "manual/webdav-unicode",
        )?))?;
        require(
            listing.objects().iter().any(|object| object.key() == &key),
            "Unicode object was not listed",
        )?;
        storage.delete(&key)?;
        println!("uploaded, listed, read, and deleted Unicode key {}", key);
        Ok(())
    }

    fn ranges(storage: &WebDavStorage) -> Result<(), Box<dyn Error>> {
        let key = ObjectKey::new("manual/webdav-ranges/large.bin")?;
        remove_if_present(storage, &key)?;
        let size = 2 * 1024 * 1024 + 37;
        let mut source = PatternReader::new(size, Duration::ZERO);
        storage.write_stream(
            &key,
            &mut source,
            ObjectWriteOptions::if_absent().with_expected_size(size as u64),
        )?;

        let start = 64 * 1024 - 17;
        let length = 64 * 1024 + 31;
        let range = ObjectRange::new(start as u64, length as u64)?;
        let mut read = storage.read_range(&key, range)?;
        let mut contents = Vec::new();
        read.read_to_end(&mut contents)?;
        let expected = pattern_slice(start, length);
        require(contents == expected, "large cross-boundary range mismatch")?;
        storage.delete(&key)?;
        println!("verified {} bytes across a large-object range", length);
        Ok(())
    }

    fn paginate(storage: &WebDavStorage) -> Result<(), Box<dyn Error>> {
        let prefix = "manual/webdav-pagination";
        let keys = (0..7)
            .map(|index| ObjectKey::new(format!("{prefix}/{index:02}.txt")))
            .collect::<Result<Vec<_>, _>>()?;
        for key in &keys {
            remove_if_present(storage, key)?;
            let mut source = Cursor::new(key.as_str().as_bytes().to_vec());
            storage.write_stream(key, &mut source, ObjectWriteOptions::if_absent())?;
        }

        let mut request = ObjectListRequest::new(ObjectPrefix::new(prefix)?).with_limit(2);
        let mut listed = Vec::new();
        let mut pages = 0;
        loop {
            let page = storage.list_page(&request)?;
            pages += 1;
            listed.extend(page.objects().iter().map(|object| object.key().clone()));
            let Some(cursor) = page.next_cursor().cloned() else {
                break;
            };
            request = request.with_cursor(cursor);
        }
        require(
            listed == keys,
            "paginated listing was incomplete or unordered",
        )?;
        for key in &keys {
            storage.delete(key)?;
        }
        println!("listed {} objects over {} pages", listed.len(), pages);
        Ok(())
    }

    fn cancel(storage: &WebDavStorage) -> Result<(), Box<dyn Error>> {
        let key = ObjectKey::new("manual/webdav-cancelled/large.bin")?;
        remove_if_present(storage, &key)?;
        let cancellation = CancellationHandle::new();
        let writer_storage = storage.clone();
        let writer_key = key.clone();
        let writer_cancellation = cancellation.clone();
        let writer = thread::spawn(move || {
            let mut source = PatternReader::new(64 * 1024 * 1024, Duration::from_millis(2));
            writer_storage.write_stream_with_cancellation(
                &writer_key,
                &mut source,
                ObjectWriteOptions::if_absent().with_expected_size(64 * 1024 * 1024),
                Some(&writer_cancellation),
            )
        });
        thread::sleep(Duration::from_millis(100));
        cancellation.cancel();
        let result = writer
            .join()
            .map_err(|_| "WebDAV cancellation writer panicked")?;
        require(
            result == Err(StorageError::Cancelled),
            "upload was not cancelled",
        )?;
        require(
            storage.metadata(&key) == Err(StorageError::NotFound),
            "cancelled upload left a usable object",
        )?;
        println!("cancelled upload left no usable object");
        Ok(())
    }

    fn redaction(storage: &WebDavStorage) -> Result<(), Box<dyn Error>> {
        let debug = format!("{storage:?}");
        if let Ok(password) = std::env::var("GIB_WEBDAV_PASSWORD")
            && !password.is_empty()
            && debug.contains(&password)
        {
            return Err("WebDAV diagnostics contain the configured password".into());
        }
        require(
            !debug.contains("Authorization") && !debug.contains("Basic "),
            "WebDAV diagnostics contain an authorization header",
        )?;
        println!("diagnostic redaction passed");
        Ok(())
    }

    fn authentication() -> Result<(), Box<dyn Error>> {
        let url = required_environment("GIB_WEBDAV_URL")?;
        let username = required_environment("GIB_WEBDAV_USERNAME")?;
        let password = required_environment("GIB_WEBDAV_PASSWORD")?;
        let mut config = WebDavStorageConfig::new(url, username, format!("{password}-incorrect"))?;
        if environment_is_true("GIB_WEBDAV_ALLOW_HTTP") {
            config = config.with_allow_insecure_http(true);
        }
        match WebDavStorage::new(config) {
            Err(StorageError::Authentication) => {
                println!("invalid credentials were classified as authentication failure");
                Ok(())
            }
            Err(error) => Err(format!(
                "invalid credentials returned {error:?}; expected Authentication"
            )
            .into()),
            Ok(_) => Err("invalid credentials were accepted by the server".into()),
        }
    }

    fn read_all(storage: &WebDavStorage, key: &ObjectKey) -> Result<Vec<u8>, Box<dyn Error>> {
        let mut object = storage.read_stream(key)?;
        let mut contents = Vec::new();
        object.read_to_end(&mut contents)?;
        Ok(contents)
    }

    fn remove_if_present(storage: &WebDavStorage, key: &ObjectKey) -> Result<(), Box<dyn Error>> {
        match storage.delete(key) {
            Ok(()) | Err(StorageError::NotFound) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn required_environment(name: &str) -> Result<String, Box<dyn Error>> {
        std::env::var(name).map_err(|_| format!("missing {name} environment variable").into())
    }

    fn environment_is_true(name: &str) -> bool {
        std::env::var(name)
            .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
    }

    fn require(condition: bool, message: &str) -> Result<(), Box<dyn Error>> {
        if condition {
            Ok(())
        } else {
            Err(message.into())
        }
    }

    struct PatternReader {
        length: usize,
        position: usize,
        delay: Duration,
    }

    impl PatternReader {
        fn new(length: usize, delay: Duration) -> Self {
            Self {
                length,
                position: 0,
                delay,
            }
        }
    }

    impl Read for PatternReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.position == self.length {
                return Ok(0);
            }
            if !self.delay.is_zero() {
                thread::sleep(self.delay);
            }
            let amount = buffer.len().min(self.length - self.position);
            for (offset, byte) in buffer[..amount].iter_mut().enumerate() {
                *byte = pattern_byte(self.position + offset);
            }
            self.position += amount;
            Ok(amount)
        }
    }

    fn pattern_slice(start: usize, length: usize) -> Vec<u8> {
        (start..start + length).map(pattern_byte).collect()
    }

    fn pattern_byte(position: usize) -> u8 {
        ((position as u64).wrapping_mul(31).wrapping_add(17) % 251) as u8
    }
}

#[cfg(feature = "webdav")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    qa::run()
}

#[cfg(not(feature = "webdav"))]
fn main() {
    eprintln!("rebuild with `--features webdav` to run this example");
    std::process::exit(2);
}
