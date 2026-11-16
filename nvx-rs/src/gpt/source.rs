//! The sources that the reader reads: bytes in memory and files.

use std::io;

/// A disk that the parser reads by byte offset: an image file, a block device, or bytes in
/// memory.
///
/// The parser only reads, at the offsets that the disk's checked fields give, and never past the
/// size that its caller states. A source keeps its file position, if it has one, except a
/// [`File`](std::fs::File) on Windows, whose positioned reads move it.
pub(crate) trait ReadAt {
    /// Fills `buffer` with the bytes at `offset`, or fails, with
    /// [`ErrorKind::UnexpectedEof`](io::ErrorKind::UnexpectedEof) if the source ends first.
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()>;
}

impl ReadAt for [u8] {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        let bytes = usize::try_from(offset)
            .ok()
            .and_then(|start| self.get(start..start.checked_add(buffer.len())?))
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        buffer.copy_from_slice(bytes);
        Ok(())
    }
}

#[cfg(unix)]
impl ReadAt for std::fs::File {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(self, buffer, offset)
    }
}

#[cfg(windows)]
impl ReadAt for std::fs::File {
    fn read_exact_at(&self, mut offset: u64, mut buffer: &mut [u8]) -> io::Result<()> {
        use std::os::windows::fs::FileExt;

        while !buffer.is_empty() {
            match self.seek_read(buffer, offset) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(count) => {
                    buffer = &mut buffer[count..];
                    offset = u64::try_from(count)
                        .ok()
                        .and_then(|count| offset.checked_add(count))
                        .ok_or(io::ErrorKind::InvalidInput)?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
