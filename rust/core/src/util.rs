//! Small helpers shared by the picture modules.

/// `std::clamp` of C++: returns `lo` below it, `hi` above it, and never panics when `lo > hi`
/// (`f64::clamp` does), so a degenerate geometry gives the same number as the C++ code.
pub(crate) fn clamp<T: PartialOrd>(v: T, lo: T, hi: T) -> T {
    if v < lo {
        lo
    } else if hi < v {
        hi
    } else {
        v
    }
}
