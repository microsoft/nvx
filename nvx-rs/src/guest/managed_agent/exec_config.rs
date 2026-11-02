use crate::guest::sys::{self, Errno, FileDescriptor};
use crate::protocol::{EXEC_ENVIRONMENT_PRESENT, EXEC_INHERIT_DEFAULT_ENV, GuestExecPayload};

/// Decoded sealed configuration consumed by helper mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HelperConfig {
    /// Exact or layered environment entries.
    pub(crate) environment: Option<Vec<Vec<u8>>>,
    /// Whether explicit entries layer over the inherited environment.
    pub(crate) inherit_default_env: bool,
}

/// Encodes the environment of `execution` in an immutable close-on-exec memfd. The direct
/// agent's child enters the working directory itself, before the helper runs.
pub(crate) fn create(execution: &GuestExecPayload) -> Result<FileDescriptor, Errno> {
    let descriptor = sys::create_memfd(c"nvx-exec-config")?;
    let bytes = encode(execution)?;
    descriptor.write_all(&bytes)?;
    descriptor.rewind()?;
    descriptor.seal_immutable()?;
    Ok(descriptor)
}

/// Serializes the private helper configuration format.
fn encode(execution: &GuestExecPayload) -> Result<Vec<u8>, Errno> {
    let mut flags = 0u16;
    if execution.environment.is_some() {
        flags |= EXEC_ENVIRONMENT_PRESENT;
    }
    if execution.inherit_default_env {
        flags |= EXEC_INHERIT_DEFAULT_ENV;
    }
    encode_fields(flags, execution.environment.as_deref().unwrap_or_default())
}

/// Lays out the private helper configuration format with `flags` and the `environment` entries:
/// the little-endian flags and entry count, then each entry after its little-endian length.
/// Tests also lay out configurations that [`encode`] never produces.
pub(super) fn encode_fields(flags: u16, environment: &[Vec<u8>]) -> Result<Vec<u8>, Errno> {
    let count = u16::try_from(environment.len()).map_err(|_| Errno::EOVERFLOW)?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    for entry in environment {
        let length = u32::try_from(entry.len()).map_err(|_| Errno::EOVERFLOW)?;
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(entry);
    }
    Ok(bytes)
}

/// Decodes a helper configuration from its sealed descriptor.
pub(crate) fn read(descriptor: &FileDescriptor) -> Result<HelperConfig, Errno> {
    let mut header = [0u8; 4];
    descriptor.read_exact(&mut header)?;
    let flags = u16::from_le_bytes([header[0], header[1]]);
    let environment_count = usize::from(u16::from_le_bytes([header[2], header[3]]));
    let known = EXEC_ENVIRONMENT_PRESENT | EXEC_INHERIT_DEFAULT_ENV;
    if flags & !known != 0
        || environment_count > crate::protocol::MAX_ENVIRONMENT
        || (flags & EXEC_ENVIRONMENT_PRESENT == 0
            && (environment_count != 0 || flags & EXEC_INHERIT_DEFAULT_ENV != 0))
    {
        return Err(Errno::EINVAL);
    }
    let mut environment = Vec::with_capacity(environment_count);
    for _ in 0..environment_count {
        let mut length = [0u8; 4];
        descriptor.read_exact(&mut length)?;
        let length = u32::from_le_bytes(length) as usize;
        if !(1..=crate::protocol::MAX_ARGUMENT_BYTES).contains(&length) {
            return Err(Errno::EINVAL);
        }
        let mut entry = vec![0; length];
        descriptor.read_exact(&mut entry)?;
        if entry.contains(&0) || entry.first() == Some(&b'=') || !entry.contains(&b'=') {
            return Err(Errno::EINVAL);
        }
        environment.push(entry);
    }
    Ok(HelperConfig {
        environment: (flags & EXEC_ENVIRONMENT_PRESENT != 0).then_some(environment),
        inherit_default_env: flags & EXEC_INHERIT_DEFAULT_ENV != 0,
    })
}

#[cfg(test)]
pub(super) mod tests;
