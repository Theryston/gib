pub trait FS {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, std::io::Error>;
    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), std::io::Error>;
}
