//! Small helpers shared by the modules of this crate.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// `lock()` that ignores poisoning: a panic in another thread must not take the connector down
/// (the C++ mutexes have no such state).
pub(crate) trait LockExt<T> {
    fn lk(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lk(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `GetTickCount64()`: milliseconds since boot (the C++ code keeps its deadlines and
/// `LastSendTick` in this unit).
pub fn tick_ms() -> u64 {
    // SAFETY: GetTickCount64 has no preconditions.
    unsafe { windows::Win32::System::SystemInformation::GetTickCount64() }
}

/// C `strtoul` on a byte string: skips leading white space, takes an optional sign, an optional
/// `0x` for base 16, then digits. Returns the value and the number of bytes consumed (0 when there
/// is no number). Overflow clamps like the C library.
pub(crate) fn strtou(s: &[u8], base: u32) -> (u64, usize) {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    if base == 16
        && i + 2 < s.len()
        && s[i] == b'0'
        && (s[i + 1] == b'x' || s[i + 1] == b'X')
        && s[i + 2].is_ascii_hexdigit()
    {
        i += 2;
    }
    let start = i;
    let mut v: u64 = 0;
    let mut overflow = false;
    while i < s.len() {
        let Some(d) = (s[i] as char).to_digit(base) else {
            break;
        };
        match v
            .checked_mul(u64::from(base))
            .and_then(|x| x.checked_add(u64::from(d)))
        {
            Some(n) => v = n,
            None => overflow = true,
        }
        i += 1;
    }
    if i == start {
        return (0, 0);
    }
    if overflow {
        return (u64::MAX, i);
    }
    (if neg { v.wrapping_neg() } else { v }, i)
}

/// C `strtoll(s, nullptr, 10)`.
pub(crate) fn strtoll10(s: &str) -> i64 {
    let b = s.as_bytes();
    let (v, n) = strtou(b, 10);
    if n == 0 {
        return 0;
    }
    let neg = b[..n].iter().find(|c| **c == b'-' || **c == b'+') == Some(&b'-');
    if neg {
        let v = v.wrapping_neg(); // strtou returns the negated value, like strtoul
        if v > i64::MAX as u64 + 1 {
            i64::MIN
        } else {
            (v as i64).wrapping_neg()
        }
    } else if v > i64::MAX as u64 {
        i64::MAX
    } else {
        v as i64
    }
}

/// C `strtod(s, nullptr)` for the plain decimal forms plists carry (longest valid prefix).
pub(crate) fn strtod_prefix(s: &str) -> f64 {
    let t = s.trim_start();
    let b = t.as_bytes();
    let mut best = 0.0;
    for i in 1..=b.len() {
        if !matches!(b[i - 1], b'0'..=b'9' | b'+' | b'-' | b'.' | b'e' | b'E') {
            break;
        }
        if let Ok(v) = t[..i].parse::<f64>() {
            best = v;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strtou_basics() {
        assert_eq!(strtou(b"1f", 16), (31, 2));
        assert_eq!(strtou(b"0x1f", 16), (31, 4));
        assert_eq!(strtou(b"  12abc", 10), (12, 4));
        assert_eq!(strtou(b"zz", 16), (0, 0));
        assert_eq!(strtou(b"0x", 16), (0, 1));
        assert_eq!(strtoll10("-42x"), -42);
        assert_eq!(strtoll10(""), 0);
        assert_eq!(strtod_prefix("1.5e2z"), 150.0);
    }
}
