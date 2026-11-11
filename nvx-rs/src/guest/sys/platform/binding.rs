//! The approved development identity record, distinct from approval of any CPU profile.

use std::fmt;

/// A structurally valid identity, not proof of profile admission. The VMM admits the effective
/// CPU/time contract before product entry; ordinary guest conformance follows readiness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TimeBinding {
    /// The cold-boot processor capacity.
    pub(crate) capacity: u32,
    /// Declared architected-counter frequency, not an inferred CPU core frequency.
    pub(crate) counter_hz: u64,
    /// The selected profile's lowercase ASCII identifier.
    pub(crate) profile_id: String,
    /// Digest of that same canonical CPU-profile document.
    pub(crate) profile_sha256: [u8; 32],
}

/// The malformed field, with the observed value, not a success-shaped default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BindingError(String);

impl fmt::Display for BindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BindingError {}

impl TimeBinding {
    /// Decodes exactly the approved 96-byte a8 record; this does not approve its profile.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, BindingError> {
        let bytes: &[u8; 96] = bytes.try_into().map_err(|_| {
            BindingError(format!(
                "ARM time identity length {}, expected 96",
                bytes.len()
            ))
        })?;
        if &bytes[..4] != b"NVT\0" || bytes[4] != 1 || bytes[5..8] != [0; 3] {
            return Err(BindingError(format!(
                "ARM time identity header {:02x?}, expected NVT/1 with zero flags/reserved",
                &bytes[..8]
            )));
        }
        let major = u16::from_le_bytes(bytes[8..10].try_into().expect("checked record"));
        let minor = u16::from_le_bytes(bytes[10..12].try_into().expect("checked record"));
        if (major, minor) != (1, 0) {
            return Err(BindingError(format!(
                "ARM time binding {major}.{minor}, expected 1.0"
            )));
        }
        let word = |offset| {
            u32::from_le_bytes(
                bytes[offset..offset + 4]
                    .try_into()
                    .expect("checked record"),
            )
        };
        let capacity = word(12);
        if !matches!(capacity, 1 | 2 | 4 | 8) {
            return Err(BindingError(format!(
                "ARM time capacity {capacity}, expected 1/2/4/8"
            )));
        }
        let counter_hz = u64::from_le_bytes(bytes[16..24].try_into().expect("checked record"));
        if counter_hz == 0 || counter_hz > u64::from(u32::MAX) {
            return Err(BindingError(format!(
                "ARM counter frequency {counter_hz}, expected nonzero CNTFRQ value"
            )));
        }
        if word(24) != 20 {
            return Err(BindingError(format!(
                "ARM timer PPI {}, expected INTID 20",
                word(24)
            )));
        }
        let length = word(28) as usize;
        if !(1..=32).contains(&length) {
            return Err(BindingError(format!(
                "ARM CPU profile ID length {length}, expected 1..32"
            )));
        }
        let id = &bytes[32..32 + length];
        if !id
            .iter()
            .all(|byte| byte.is_ascii_graphic() && !byte.is_ascii_uppercase())
            || bytes[32 + length..64].iter().any(|&byte| byte != 0)
        {
            return Err(BindingError(
                "ARM CPU profile ID must be lowercase ASCII with zero padding".into(),
            ));
        }
        Ok(Self {
            capacity,
            counter_hz,
            profile_id: String::from_utf8(id.to_vec()).expect("validated ASCII"),
            profile_sha256: bytes[64..96].try_into().expect("checked record"),
        })
    }
}

#[cfg(test)]
mod tests;
