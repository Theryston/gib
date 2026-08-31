use gib::{
    LocalStorage, ObjectKey, ObjectListRequest, ObjectPrefix, ObjectRange, ObjectWriteOptions,
    RepositoryStorage, StorageError,
};
use std::error::Error;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    let root = arguments
        .next()
        .map(PathBuf::from)
        .ok_or("missing storage root")?;
    let command = arguments.next().unwrap_or_else(|| String::from("smoke"));

    match command.as_str() {
        "smoke" => smoke(&root),
        "hold-write" => {
            let size = parse_argument(&mut arguments, "byte count")?;
            let delay_ms = parse_argument(&mut arguments, "delay in milliseconds")?;
            hold_write(&root, size, delay_ms)
        }
        "conflict" => conflict(&root),
        "symlink" => symlink_check(&root),
        _ => Err(
            format!("unknown command {command}; use smoke, hold-write, conflict, or symlink")
                .into(),
        ),
    }
}

fn parse_argument<T>(
    arguments: &mut impl Iterator<Item = String>,
    name: &str,
) -> Result<T, Box<dyn Error>>
where
    T: std::str::FromStr,
    T::Err: Error + 'static,
{
    arguments
        .next()
        .ok_or_else(|| -> Box<dyn Error> { format!("missing {name}").into() })?
        .parse::<T>()
        .map_err(|error| -> Box<dyn Error> { format!("invalid {name}: {error}").into() })
}

fn smoke(root: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let storage = LocalStorage::new(root)?;
    let key = ObjectKey::new("manual/object")?;
    match storage.delete(&key) {
        Ok(()) | Err(StorageError::NotFound) => {}
        Err(error) => return Err(error.into()),
    }

    let mut source = Cursor::new(b"local storage smoke test".to_vec());
    let metadata = storage.write_stream(&key, &mut source, ObjectWriteOptions::if_absent())?;
    println!("uploaded {} ({} bytes)", metadata.key(), metadata.size());

    let listed = storage.list_page(&ObjectListRequest::new(ObjectPrefix::new("manual")?))?;
    println!("listed: {:?}", listed.objects());

    let mut object = storage.read_stream(&key)?;
    let mut contents = Vec::new();
    object.read_to_end(&mut contents)?;
    println!("read: {}", String::from_utf8_lossy(&contents));

    let mut range = storage.read_range(&key, ObjectRange::new(6, 7)?)?;
    let mut range_contents = Vec::new();
    range.read_to_end(&mut range_contents)?;
    println!(
        "range [6, 13): {}",
        String::from_utf8_lossy(&range_contents)
    );

    storage.delete(&key)?;
    println!("deleted {}", key);
    Ok(())
}

fn hold_write(root: &std::path::Path, size: usize, delay_ms: u64) -> Result<(), Box<dyn Error>> {
    let storage = LocalStorage::new(root)?;
    let key = ObjectKey::new("manual/kill-me")?;
    let mut source = SlowPatternReader {
        length: size,
        position: 0,
        delay: Duration::from_millis(delay_ms),
    };
    storage.write_stream(
        &key,
        &mut source,
        ObjectWriteOptions::if_absent().with_expected_size(size as u64),
    )?;
    println!("completed {} ({} bytes)", key, size);
    Ok(())
}

fn conflict(root: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let first_storage = LocalStorage::new(root)?;
    let second_storage = LocalStorage::new(root)?;
    let key = ObjectKey::new("manual/conflict")?;
    match first_storage.delete(&key) {
        Ok(()) | Err(StorageError::NotFound) => {}
        Err(error) => return Err(error.into()),
    }
    let mut initial_source = Cursor::new(b"initial".to_vec());
    let initial =
        first_storage.write_stream(&key, &mut initial_source, ObjectWriteOptions::if_absent())?;
    let version = initial
        .version()
        .cloned()
        .ok_or("local storage did not return a version")?;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

    let first_barrier = barrier.clone();
    let first_key = key.clone();
    let first_version = version.clone();
    let first = thread::spawn(move || {
        first_barrier.wait();
        let mut source = Cursor::new(b"winner-one".to_vec());
        first_storage.write_stream(
            &first_key,
            &mut source,
            ObjectWriteOptions::if_version(first_version),
        )
    });
    let second_barrier = barrier;
    let second_key = key.clone();
    let second = thread::spawn(move || {
        second_barrier.wait();
        let mut source = Cursor::new(b"winner-two".to_vec());
        second_storage.write_stream(
            &second_key,
            &mut source,
            ObjectWriteOptions::if_version(version),
        )
    });

    let first_result = first.join().map_err(|_| "first writer panicked")?;
    let second_result = second.join().map_err(|_| "second writer panicked")?;
    println!("writer one: {first_result:?}");
    println!("writer two: {second_result:?}");
    if (first_result.is_ok() as u8) + (second_result.is_ok() as u8) != 1 {
        return Err("conditional writers did not produce exactly one winner".into());
    }
    if !matches!(first_result, Err(StorageError::Conflict))
        && !matches!(second_result, Err(StorageError::Conflict))
    {
        return Err("losing writer did not report a conflict".into());
    }
    Ok(())
}

fn symlink_check(root: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let storage = LocalStorage::new(root)?;
    let key = ObjectKey::new("linked-parent/outside-object")?;
    if !matches!(
        storage.read_stream(&key),
        Err(StorageError::InvalidObjectKey)
    ) {
        return Err("symlinked parent was not rejected".into());
    }
    println!("rejected symlinked object path {}", key);
    Ok(())
}

struct SlowPatternReader {
    length: usize,
    position: usize,
    delay: Duration,
}

impl Read for SlowPatternReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.position == self.length || buffer.is_empty() {
            return Ok(0);
        }
        let amount = (self.length - self.position).min(buffer.len());
        for (offset, byte) in buffer[..amount].iter_mut().enumerate() {
            *byte = ((self.position + offset) % 251) as u8;
        }
        self.position += amount;
        if !self.delay.is_zero() {
            thread::sleep(self.delay);
        }
        Ok(amount)
    }
}
