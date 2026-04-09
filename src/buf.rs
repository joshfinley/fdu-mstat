//! Fixed-size stack-allocated byte buffer.
//!
//! `Buf<N>` is a zero-allocation alternative to `String` that stores up to N
//! bytes on the stack. It supports UTF-8 string operations and numeric
//! formatting without touching the heap.

/// A fixed-capacity byte buffer on the stack. Silently truncates if full.
#[derive(Clone, Copy)]
pub struct Buf<const N: usize> {
    d: [u8; N],
    len: usize,
}

impl<const N: usize> Buf<N> {
    pub const fn new() -> Self {
        Self {
            d: [0u8; N],
            len: 0,
        }
    }

    #[inline(always)]
    pub fn push_byte(&mut self, b: u8) {
        if self.len < N {
            self.d[self.len] = b;
            self.len += 1;
        }
    }

    #[inline]
    pub fn push_bytes(&mut self, s: &[u8]) {
        let avail = N - self.len;
        let n = s.len().min(avail);
        self.d[self.len..self.len + n].copy_from_slice(&s[..n]);
        self.len += n;
    }

    #[inline]
    pub fn push_str(&mut self, s: &str) {
        self.push_bytes(s.as_bytes());
    }

    /// Format a `u64` as decimal digits (no leading zeros).
    pub fn push_u64(&mut self, mut n: u64) {
        if n == 0 {
            self.push_byte(b'0');
            return;
        }
        let start = self.len;
        while n > 0 && self.len < N {
            self.d[self.len] = b'0' + (n % 10) as u8;
            self.len += 1;
            n /= 10;
        }
        self.d[start..self.len].reverse();
    }

    /// Format an `f64` with exactly 2 decimal places (e.g. "12.34").
    pub fn push_f64_2dp(&mut self, val: f64) {
        if val < 0.0 {
            self.push_byte(b'-');
            self.push_f64_2dp(-val);
            return;
        }
        let int_part = val as u64;
        let frac = ((val - int_part as f64) * 100.0 + 0.5) as u64;
        if frac >= 100 {
            self.push_u64(int_part + 1);
            self.push_bytes(b".00");
        } else {
            self.push_u64(int_part);
            self.push_byte(b'.');
            if frac < 10 {
                self.push_byte(b'0');
            }
            self.push_u64(frac);
        }
    }

    /// Interpret contents as UTF-8 (caller must ensure validity).
    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: all push methods only accept valid UTF-8 sources.
        unsafe { std::str::from_utf8_unchecked(&self.d[..self.len]) }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.d[..self.len]
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Count Unicode characters (display width for ASCII/BMP).
    pub fn char_count(&self) -> usize {
        let mut count = 0;
        let mut i = 0;
        while i < self.len {
            if self.d[i] < 0x80 {
                i += 1;
            } else if self.d[i] < 0xE0 {
                i += 2;
            } else if self.d[i] < 0xF0 {
                i += 3;
            } else {
                i += 4;
            }
            count += 1;
        }
        count
    }
}
