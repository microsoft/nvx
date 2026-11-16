use std::cell::Cell;

thread_local! {
    /// Number of [`summarize`](super::summarize) calls on this thread.
    pub(crate) static SUMMARIES: Cell<usize> = const { Cell::new(0) };
    /// Number of intervals that [`merge`](super::merge) received on this thread.
    pub(crate) static MERGED_INTERVALS: Cell<usize> = const { Cell::new(0) };
}

/// Resets the counters and returns the result of `operation` with the
/// [`summarize`](super::summarize) calls and merged intervals that it caused.
pub(crate) fn count<T>(operation: impl FnOnce() -> T) -> (T, usize, usize) {
    SUMMARIES.with(|count| count.set(0));
    MERGED_INTERVALS.with(|count| count.set(0));
    let result = operation();
    (
        result,
        SUMMARIES.with(Cell::get),
        MERGED_INTERVALS.with(Cell::get),
    )
}
