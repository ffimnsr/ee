//! Leaf, rope metrics, and utf8/utf16 helpers.
use super::*;

impl Leaf for String {
    fn len(&self) -> usize {
        self.len()
    }

    fn is_ok_child(&self) -> bool {
        self.len() >= MIN_LEAF
    }

    fn push_maybe_split(&mut self, other: &String, iv: Interval) -> Option<String> {
        //println!("push_maybe_split [{}] [{}] {:?}", self, other, iv);
        let (start, end) = iv.start_end();
        self.push_str(&other[start..end]);
        if self.len() <= MAX_LEAF {
            None
        } else {
            let splitpoint = find_leaf_split_for_merge(self);
            let right_str = self[splitpoint..].to_owned();
            self.truncate(splitpoint);
            self.shrink_to_fit();
            Some(right_str)
        }
    }
}

#[derive(Clone, Copy)]
pub struct RopeInfo {
    pub(crate) lines: usize,
    pub(crate) utf16_size: usize,
}

impl NodeInfo for RopeInfo {
    type L = String;

    fn accumulate(&mut self, other: &Self) {
        self.lines += other.lines;
        self.utf16_size += other.utf16_size;
    }

    fn compute_info(s: &String) -> Self {
        RopeInfo { lines: count_newlines(s), utf16_size: count_utf16_code_units(s) }
    }

    fn identity() -> Self {
        RopeInfo { lines: 0, utf16_size: 0 }
    }
}

impl DefaultMetric for RopeInfo {
    type DefaultMetric = BaseMetric;
}

/// Byte-offset metric used as rope's base coordinate system.
///
/// Rope metrics convert between measured units and base units. Base units are
/// UTF-8 bytes for every metric in this module. A metric boundary identifies a
/// byte offset representable in that metric; `prev` and `next` walk adjacent
/// boundaries. Metrics that can fragment may split content at internal metric
/// boundaries, while atomic metrics keep each measured element intact.
///
/// `BaseMetric` measures UTF-8 bytes. Valid boundaries are Unicode scalar-value
/// boundaries, so walking this metric advances by one UTF-8 code point even
/// though reported offsets remain byte offsets.
///
/// Offsets that do not correspond to code-point boundaries are _invalid_, and
/// calling functions that assume valid offsets with invalid offsets will panic
/// in debug mode.
///
/// Boundary is atomic and determined by codepoint boundary.  Atomicity is
/// implicit, because offsets between two utf8 code units that form a code
/// point is considered invalid. For example, if a string starts with a
/// 0xC2 byte, then `offset=1` is invalid.
#[derive(Clone, Copy)]
pub struct BaseMetric(());

impl Metric<RopeInfo> for BaseMetric {
    fn measure(_: &RopeInfo, len: usize) -> usize {
        len
    }

    fn to_base_units(s: &String, in_measured_units: usize) -> usize {
        debug_assert!(s.is_char_boundary(in_measured_units));
        in_measured_units
    }

    fn from_base_units(s: &String, in_base_units: usize) -> usize {
        debug_assert!(s.is_char_boundary(in_base_units));
        in_base_units
    }

    fn is_boundary(s: &String, offset: usize) -> bool {
        s.is_char_boundary(offset)
    }

    fn prev(s: &String, offset: usize) -> Option<usize> {
        if offset == 0 {
            // I think it's a precondition that this will never be called
            // with offset == 0, but be defensive.
            None
        } else {
            let mut len = 1;
            while !s.is_char_boundary(offset - len) {
                len += 1;
            }
            Some(offset - len)
        }
    }

    fn next(s: &String, offset: usize) -> Option<usize> {
        if offset == s.len() {
            // I think it's a precondition that this will never be called
            // with offset == s.len(), but be defensive.
            None
        } else {
            let b = s.as_bytes()[offset];
            Some(offset + len_utf8_from_first_byte(b))
        }
    }

    fn can_fragment() -> bool {
        false
    }
}

/// Given the inital byte of a UTF-8 codepoint, returns the number of
/// bytes required to represent the codepoint.
/// RFC reference: <https://tools.ietf.org/html/rfc3629#section-4>
pub fn len_utf8_from_first_byte(b: u8) -> usize {
    match b {
        b if b < 0x80 => 1,
        b if b < 0xe0 => 2,
        b if b < 0xf0 => 3,
        _ => 4,
    }
}

/// Newline-count metric.
///
/// Measured units are newline bytes; base units are UTF-8 bytes. Boundaries are
/// trailing, immediately after each `\n`. This metric can fragment because a
/// rope slice may start or end between line boundaries.
#[derive(Clone, Copy)]
pub struct LinesMetric;

impl Metric<RopeInfo> for LinesMetric {
    fn measure(info: &RopeInfo, _: usize) -> usize {
        info.lines
    }

    fn is_boundary(s: &String, offset: usize) -> bool {
        if offset == 0 {
            // shouldn't be called with this, but be defensive
            false
        } else {
            s.as_bytes()[offset - 1] == b'\n'
        }
    }

    fn to_base_units(s: &String, in_measured_units: usize) -> usize {
        let mut offset = 0;
        for _ in 0..in_measured_units {
            match memchr(b'\n', &s.as_bytes()[offset..]) {
                Some(pos) => offset += pos + 1,
                _ => panic!("to_base_units called with arg too large"),
            }
        }
        offset
    }

    fn from_base_units(s: &String, in_base_units: usize) -> usize {
        count_newlines(&s[..in_base_units])
    }

    fn prev(s: &String, offset: usize) -> Option<usize> {
        debug_assert!(offset > 0, "caller is responsible for validating input");
        memrchr(b'\n', &s.as_bytes()[..offset - 1]).map(|pos| pos + 1)
    }

    fn next(s: &String, offset: usize) -> Option<usize> {
        memchr(b'\n', &s.as_bytes()[offset..]).map(|pos| offset + pos + 1)
    }

    fn can_fragment() -> bool {
        true
    }
}

/// UTF-16 code-unit metric used by protocols such as LSP.
///
/// Measured units are UTF-16 code units; base units are UTF-8 bytes. Boundaries
/// remain Unicode scalar-value boundaries, preventing offsets from splitting a
/// surrogate pair or UTF-8 code point. This metric is atomic.
#[derive(Clone, Copy)]
pub struct Utf16CodeUnitsMetric;

impl Metric<RopeInfo> for Utf16CodeUnitsMetric {
    fn measure(info: &RopeInfo, _: usize) -> usize {
        info.utf16_size
    }

    fn is_boundary(s: &String, offset: usize) -> bool {
        s.is_char_boundary(offset)
    }

    fn to_base_units(s: &String, in_measured_units: usize) -> usize {
        let mut cur_len_utf16 = 0;
        let mut cur_len_utf8 = 0;
        for u in s.chars() {
            if cur_len_utf16 >= in_measured_units {
                break;
            }
            cur_len_utf16 += u.len_utf16();
            cur_len_utf8 += u.len_utf8();
        }
        cur_len_utf8
    }

    fn from_base_units(s: &String, in_base_units: usize) -> usize {
        count_utf16_code_units(&s[..in_base_units])
    }

    fn prev(s: &String, offset: usize) -> Option<usize> {
        if offset == 0 {
            // I think it's a precondition that this will never be called
            // with offset == 0, but be defensive.
            None
        } else {
            let mut len = 1;
            while !s.is_char_boundary(offset - len) {
                len += 1;
            }
            Some(offset - len)
        }
    }

    fn next(s: &String, offset: usize) -> Option<usize> {
        if offset == s.len() {
            // I think it's a precondition that this will never be called
            // with offset == s.len(), but be defensive.
            None
        } else {
            let b = s.as_bytes()[offset];
            Some(offset + len_utf8_from_first_byte(b))
        }
    }

    fn can_fragment() -> bool {
        false
    }
}

// Low level functions

pub fn count_newlines(s: &str) -> usize {
    bytecount::count(s.as_bytes(), b'\n')
}

pub(crate) fn count_utf16_code_units(s: &str) -> usize {
    let mut utf16_count = 0;
    for &b in s.as_bytes() {
        if (b as i8) >= -0x40 {
            utf16_count += 1;
        }
        if b >= 0xf0 {
            utf16_count += 1;
        }
    }
    utf16_count
}

pub(crate) fn clamp_to_char_boundary(s: &str, splitpoint: usize) -> usize {
    let mut splitpoint = splitpoint.min(s.len());
    while splitpoint > 0 && !s.is_char_boundary(splitpoint) {
        splitpoint -= 1;
    }
    splitpoint
}

pub(crate) fn is_crlf_split_point(s: &str, splitpoint: usize) -> bool {
    splitpoint > 0
        && splitpoint < s.len()
        && s.as_bytes()[splitpoint - 1] == b'\r'
        && s.as_bytes()[splitpoint] == b'\n'
}

pub(crate) fn adjust_splitpoint_for_crlf(
    s: &str,
    minsplit: usize,
    maxsplit: usize,
    splitpoint: usize,
) -> usize {
    if !is_crlf_split_point(s, splitpoint) {
        return splitpoint;
    }
    if splitpoint < maxsplit {
        splitpoint + 1
    } else if splitpoint > minsplit {
        splitpoint - 1
    } else {
        splitpoint
    }
}

pub(crate) fn find_leaf_split_for_bulk(s: &str) -> usize {
    find_leaf_split(s, MIN_LEAF)
}

pub(crate) fn find_leaf_split_for_merge(s: &str) -> usize {
    find_leaf_split(s, max(MIN_LEAF, s.len() - MAX_LEAF))
}

// Try to split at newline boundary (leaning left), if not, then split at codepoint
pub(crate) fn find_leaf_split(s: &str, minsplit: usize) -> usize {
    let maxsplit = min(MAX_LEAF, s.len() - MIN_LEAF);
    let splitpoint = match memrchr(b'\n', &s.as_bytes()[minsplit - 1..maxsplit]) {
        Some(pos) => minsplit + pos,
        None => clamp_to_char_boundary(s, maxsplit),
    };
    adjust_splitpoint_for_crlf(s, minsplit, maxsplit, splitpoint)
}
