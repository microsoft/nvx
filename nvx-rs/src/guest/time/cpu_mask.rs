//! Sets of the CPUs that the time ABI covers.

use std::fmt::{self, Display};

use super::MAX_CPUS;
use crate::guest::kernel_text;

// Each CPU that the time ABI covers is one bit of the mask.
const _: () = assert!(MAX_CPUS == u64::BITS as usize);

/// A set of CPUs below `MAX_CPUS`, the CPUs that the time ABI covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct CpuMask(u64);

impl CpuMask {
    /// The set without CPUs.
    pub(crate) const EMPTY: Self = Self(0);

    /// Parses a CPU list in the kernel's syntax, such as `0-3,6`, in which ranges may overlap.
    /// Returns `None` for any other form, and for a list that names a CPU at or above
    /// `MAX_CPUS`.
    pub(crate) fn parse(text: &[u8]) -> Option<Self> {
        kernel_text::parse_cpu_list(text)?
            .into_iter()
            .try_fold(Self::EMPTY, |mask, cpus| {
                (*cpus.end() < MAX_CPUS).then(|| cpus.fold(mask, Self::with))
            })
    }

    /// Parses the CPUs that a restore brought online: `none`, an empty list, or a CPU list.
    pub(crate) fn parse_new(text: &[u8]) -> Option<Self> {
        if text == b"none" {
            Some(Self::EMPTY)
        } else {
            Self::parse(text)
        }
    }

    /// Returns the set with `cpu`, which must be below `MAX_CPUS`, added.
    #[must_use]
    pub(crate) const fn with(self, cpu: usize) -> Self {
        Self(self.0 | 1 << cpu)
    }

    /// Returns whether the set has `cpu`.
    pub(crate) const fn contains(self, cpu: usize) -> bool {
        cpu < MAX_CPUS && self.0 & 1 << cpu != 0
    }

    /// Returns the number of CPUs in the set.
    pub(crate) const fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// Returns whether the set has no CPUs.
    pub(crate) const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns the set's CPUs in ascending order.
    pub(crate) fn iter(self) -> impl Iterator<Item = usize> {
        (0..MAX_CPUS).filter(move |&cpu| self.contains(cpu))
    }
}

impl Display for CpuMask {
    /// Writes the set as a CPU list in the kernel's syntax, with ranges, such as `0-3,6`, or
    /// `none` for the empty set.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return formatter.write_str("none");
        }
        let cpus: Vec<usize> = self.iter().collect();
        let mut separator = "";
        for run in cpus.chunk_by(|cpu, next| cpu + 1 == *next) {
            let (first, last) = (run[0], run[run.len() - 1]);
            if first == last {
                write!(formatter, "{separator}{first}")?;
            } else {
                write!(formatter, "{separator}{first}-{last}")?;
            }
            separator = ",";
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_overlap_and_stop_below_the_abi_limit() {
        let mask = CpuMask::parse(b"0-3,2,5").unwrap();
        assert_eq!(mask.len(), 5);
        assert_eq!(mask.iter().collect::<Vec<_>>(), [0, 1, 2, 3, 5]);
        assert!(mask.contains(5) && !mask.contains(4) && !mask.contains(64));
        assert_eq!(CpuMask::parse(b"63"), Some(CpuMask::EMPTY.with(63)));
        assert_eq!(CpuMask::parse(b"64"), None);
        assert_eq!(CpuMask::parse(b"3-1"), None);
        assert_eq!(CpuMask::parse(b"0-3\n"), None);
        assert_eq!(CpuMask::parse(b""), Some(CpuMask::EMPTY));
    }

    #[test]
    fn new_cpus_may_be_none() {
        assert_eq!(CpuMask::parse_new(b"none"), Some(CpuMask::EMPTY));
        assert_eq!(CpuMask::parse_new(b""), Some(CpuMask::EMPTY));
        assert_eq!(CpuMask::parse_new(b"1-3,5").map(CpuMask::len), Some(4));
        assert_eq!(CpuMask::parse_new(b"64"), None);
    }

    #[test]
    fn masks_print_as_kernel_cpu_lists() {
        let mask = CpuMask::parse(b"4,5,6,9,11-12,63").unwrap();
        assert_eq!(mask.to_string(), "4-6,9,11-12,63");
        assert_eq!(CpuMask::EMPTY.to_string(), "none");
        assert_eq!(CpuMask::parse(&mask.to_string().into_bytes()), Some(mask));
    }
}
