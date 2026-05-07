// Copyright 2016 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Unicode utilities useful for text editing, including a line breaking iterator.
#![no_std]

// Required by `#![no_std]`: provides heap-allocated types (Vec, String, Box)
// via the global allocator without linking to the full `std` library.
extern crate alloc;

mod emoji;
mod tables;

use core::cmp::Ordering;

use crate::emoji::*;
use crate::tables::*;

const LB_CM: u8 = 9;
const LB_AL: u8 = 2;
const LB_OP: u8 = 20;
const LB_SP: u8 = 26;
const LB_HL: u8 = 38;
const LB_VF: u8 = 46;
const LB_QU_PF: u8 = 50;
const LB_OP_EA: u8 = 51;
const LB_AK: u8 = 43;
const LB_AS: u8 = 45;
const LB_DC: u8 = 53;

/// The Unicode line breaking property of the given code point.
///
/// This is given as a numeric value which matches the ULineBreak
/// enum value from ICU.
pub fn linebreak_property(cp: char) -> u8 {
    let cp = cp as usize;
    if cp < 0x800 {
        LINEBREAK_1_2[cp]
    } else if cp < 0x10000 {
        let child = LINEBREAK_3_ROOT[cp >> 6];
        LINEBREAK_3_CHILD[(child as usize) * 0x40 + (cp & 0x3f)]
    } else {
        let mid = LINEBREAK_4_ROOT[cp >> 12];
        let leaf = LINEBREAK_4_MID[(mid as usize) * 0x40 + ((cp >> 6) & 0x3f)];
        LINEBREAK_4_LEAVES[(leaf as usize) * 0x40 + (cp & 0x3f)]
    }
}

/// The Unicode line breaking property of the given code point.
///
/// Look up the line breaking property for the first code point in the
/// string. Return the property as a numeric value, and also the utf-8
/// length of the codepoint, for convenience.
pub fn linebreak_property_str(s: &str, ix: usize) -> (u8, usize) {
    let b = s.as_bytes()[ix];
    if b < 0x80 {
        (LINEBREAK_1_2[b as usize], 1)
    } else if b < 0xe0 {
        // 2 byte UTF-8 sequences
        let cp = ((b as usize) << 6) + (s.as_bytes()[ix + 1] as usize) - 0x3080;
        (LINEBREAK_1_2[cp], 2)
    } else if b < 0xf0 {
        // 3 byte UTF-8 sequences
        let mid_ix = ((b as usize) << 6) + (s.as_bytes()[ix + 1] as usize) - 0x3880;
        let mid = LINEBREAK_3_ROOT[mid_ix];
        (LINEBREAK_3_CHILD[(mid as usize) * 0x40 + (s.as_bytes()[ix + 2] as usize) - 0x80], 3)
    } else {
        // 4 byte UTF-8 sequences
        let mid_ix = ((b as usize) << 6) + (s.as_bytes()[ix + 1] as usize) - 0x3c80;
        let mid = LINEBREAK_4_ROOT[mid_ix];
        let leaf_ix = ((mid as usize) << 6) + (s.as_bytes()[ix + 2] as usize) - 0x80;
        let leaf = LINEBREAK_4_MID[leaf_ix];
        (LINEBREAK_4_LEAVES[(leaf as usize) * 0x40 + (s.as_bytes()[ix + 3] as usize) - 0x80], 4)
    }
}

/// An iterator which produces line breaks according to the UAX 14 line
/// breaking algorithm. For each break, return a tuple consisting of the offset
/// within the source string and a bool indicating whether it's a hard break.
///
/// There is never a break at the beginning of the string (thus, the empty string
/// produces no breaks). For non-empty strings, there is always a break at the
/// end. It is indicated as a hard break when the string is terminated with a
/// newline or other Unicode explicit line-end character.
#[derive(Copy, Clone)]
pub struct LineBreakIterator<'a> {
    s: &'a str,
    ix: usize,
    state: u8,
}

impl<'a> Iterator for LineBreakIterator<'a> {
    type Item = (usize, bool);

    // return break pos and whether it's a hard break
    fn next(&mut self) -> Option<(usize, bool)> {
        loop {
            match self.ix.cmp(&self.s.len()) {
                Ordering::Greater => {
                    return None;
                }
                Ordering::Equal => {
                    // LB3, break at EOT
                    self.ix += 1;
                    let row = LINEBREAK_STATE_MAP[self.state as usize];
                    let i = (row as usize) * N_LINEBREAK_CATEGORIES;
                    let new = LINEBREAK_STATE_MACHINE[i];
                    return Some((self.s.len(), new >= 0xc0));
                }
                Ordering::Less => {
                    let (lb, len) = linebreak_property_str(self.s, self.ix);
                    let row = LINEBREAK_STATE_MAP[self.state as usize];
                    let i = (row as usize) * N_LINEBREAK_CATEGORIES + (lb as usize);
                    let new = LINEBREAK_STATE_MACHINE[i];
                    //println!("{:?}[{}], state {} + lb {} -> {}", &self.s[self.ix..], self.ix, self.state, lb, new);
                    let result = self.ix;
                    self.ix += len;
                    if (new as i8) < 0 {
                        if suppress_break(self.s, result, lb) {
                            self.state = lb;
                            continue;
                        }
                        // break found
                        self.state = new & 0x3f;
                        return Some((result, new >= 0xc0));
                    } else if force_break(self.s, result, lb) {
                        self.state = lb;
                        return Some((result, false));
                    } else {
                        self.state = new;
                    }
                }
            }
        }
    }
}

fn suppress_break(s: &str, ix: usize, lb: u8) -> bool {
    is_brahmic_core(lb)
        && prev_non_cm(s, ix).is_some_and(is_brahmic_core)
        && next_non_cm(s, ix) == Some(LB_VF)
}

fn force_break(s: &str, ix: usize, lb: u8) -> bool {
    lb == LB_QU_PF
        && prev_lb(s, ix) == Some(LB_SP)
        && matches!(next_lb(s, ix), Some(LB_AL | LB_HL | LB_OP | LB_OP_EA))
}

fn is_brahmic_core(lb: u8) -> bool {
    matches!(lb, LB_AK | LB_AS | LB_DC)
}

fn prev_lb(s: &str, ix: usize) -> Option<u8> {
    s[..ix].char_indices().next_back().map(|(_, ch)| linebreak_property(ch))
}

fn prev_non_cm(s: &str, ix: usize) -> Option<u8> {
    s[..ix].char_indices().rev().map(|(_, ch)| linebreak_property(ch)).find(|&lb| lb != LB_CM)
}

fn next_lb(s: &str, ix: usize) -> Option<u8> {
    let len = s[ix..].chars().next()?.len_utf8();
    (ix + len < s.len()).then(|| linebreak_property_str(s, ix + len).0)
}

fn next_non_cm(s: &str, ix: usize) -> Option<u8> {
    let len = s[ix..].chars().next()?.len_utf8();
    let mut next_ix = ix + len;
    while next_ix < s.len() {
        let (lb, len) = linebreak_property_str(s, next_ix);
        if lb != LB_CM {
            return Some(lb);
        }
        next_ix += len;
    }
    None
}

impl<'a> LineBreakIterator<'a> {
    /// Create a new iterator for the given string slice.
    pub fn new(s: &str) -> LineBreakIterator<'_> {
        if s.is_empty() {
            LineBreakIterator {
                s,
                ix: 1, // LB2, don't break; sot takes priority for empty string
                state: 0,
            }
        } else {
            let (lb, len) = linebreak_property_str(s, 0);
            LineBreakIterator { s, ix: len, state: lb }
        }
    }
}

/// A struct useful for computing line breaks in a rope or other non-contiguous
/// string representation. This is a trickier problem than iterating in a string
/// for a few reasons, the trickiest of which is that in the general case,
/// line breaks require an indeterminate amount of look-behind.
///
/// This is something of an "expert-level" interface, and should only be used if
/// the caller is prepared to respect all the invariants. Otherwise, you might
/// get inconsistent breaks depending on start position and leaf boundaries.
#[derive(Copy, Clone)]
pub struct LineBreakLeafIter {
    ix: usize,
    state: u8,
}

#[allow(clippy::derivable_impls)]
impl Default for LineBreakLeafIter {
    // A default value. No guarantees on what happens when next() is called
    // on this. Intended to be useful for empty ropes.
    fn default() -> LineBreakLeafIter {
        LineBreakLeafIter { ix: 0, state: 0 }
    }
}

impl LineBreakLeafIter {
    /// Create a new line break iterator suitable for leaves in a rope.
    /// Precondition: ix is at a code point boundary within s.
    pub fn new(s: &str, ix: usize) -> LineBreakLeafIter {
        let (lb, len) = if ix == s.len() { (0, 0) } else { linebreak_property_str(s, ix) };
        LineBreakLeafIter { ix: ix + len, state: lb }
    }

    /// Return break pos and whether it's a hard break. Note: hard break
    /// indication may go away, this may not be useful in actual application.
    /// If end of leaf is found, return leaf's len. This does not indicate
    /// a break, as that requires at least one more codepoint of context.
    /// If it is a break, then subsequent next call will return an offset of 0.
    /// EOT is always a break, so in the EOT case it's up to the caller
    /// to figure that out.
    ///
    /// For consistent results, always supply same `s` until end of leaf is
    /// reached (and initially this should be the same as in the `new` call).
    pub fn next(&mut self, s: &str) -> (usize, bool) {
        loop {
            if self.ix == s.len() {
                self.ix = 0; // in preparation for next leaf
                return (s.len(), false);
            }
            let (lb, len) = linebreak_property_str(s, self.ix);
            let row = LINEBREAK_STATE_MAP[self.state as usize];
            let i = (row as usize) * N_LINEBREAK_CATEGORIES + (lb as usize);
            let new = LINEBREAK_STATE_MACHINE[i];
            //println!("\"{}\"[{}], state {} + lb {} -> {}", &s[self.ix..], self.ix, self.state, lb, new);
            let result = self.ix;
            self.ix += len;
            if (new as i8) < 0 {
                // break found
                self.state = new & 0x3f;
                return (result, new >= 0xc0);
            } else {
                self.state = new;
            }
        }
    }
}

fn is_in_asc_list<T: core::cmp::Ord>(c: T, list: &[T]) -> bool {
    list.binary_search(&c).is_ok()
}

pub fn is_variation_selector(c: char) -> bool {
    ('\u{FE00}'..='\u{FE0F}').contains(&c) || ('\u{E0100}'..='\u{E01EF}').contains(&c)
}

#[allow(clippy::wrong_self_convention)] // clippy wants &self for all of these
pub trait EmojiExt {
    fn is_regional_indicator_symbol(self) -> bool;
    fn is_emoji_modifier(self) -> bool;
    fn is_emoji_combining_enclosing_keycap(self) -> bool;
    fn is_emoji(self) -> bool;
    fn is_emoji_modifier_base(self) -> bool;
    fn is_tag_spec_char(self) -> bool;
    fn is_emoji_cancel_tag(self) -> bool;
    fn is_zwj(self) -> bool;
}

impl EmojiExt for char {
    fn is_regional_indicator_symbol(self) -> bool {
        ('\u{1F1E6}'..='\u{1F1FF}').contains(&self)
    }
    fn is_emoji_modifier(self) -> bool {
        ('\u{1F3FB}'..='\u{1F3FF}').contains(&self)
    }
    fn is_emoji_combining_enclosing_keycap(self) -> bool {
        self == '\u{20E3}'
    }
    fn is_emoji(self) -> bool {
        is_in_asc_list(self, &EMOJI_TABLE)
    }
    fn is_emoji_modifier_base(self) -> bool {
        is_in_asc_list(self, &EMOJI_MODIFIER_BASE_TABLE)
    }
    fn is_tag_spec_char(self) -> bool {
        ('\u{E0020}'..='\u{E007E}').contains(&self)
    }
    fn is_emoji_cancel_tag(self) -> bool {
        self == '\u{E007F}'
    }
    fn is_zwj(self) -> bool {
        self == '\u{200D}'
    }
}

pub fn is_keycap_base(c: char) -> bool {
    c.is_ascii_digit() || c == '#' || c == '*'
}

#[cfg(test)]
mod tests {
    use crate::LineBreakIterator;
    use crate::linebreak_property;
    use crate::linebreak_property_str;
    use alloc::vec;
    use alloc::vec::*;

    #[test]
    fn linebreak_prop() {
        // autogenerated from LineBreak-17.0.0.txt by mk_tables.py --tests
        assert_eq!(9, linebreak_property('\u{0001}'));
        assert_eq!(9, linebreak_property('\u{0005}'));
        assert_eq!(9, linebreak_property('\u{0008}'));
        assert_eq!(6, linebreak_property('\u{000C}'));
        assert_eq!(9, linebreak_property('\u{0010}'));
        assert_eq!(9, linebreak_property('\u{0013}'));
        assert_eq!(9, linebreak_property('\u{001B}'));
        assert_eq!(9, linebreak_property('\u{001E}'));
        assert_eq!(36, linebreak_property('\u{0029}'));
        assert_eq!(19, linebreak_property('\u{0034}'));
        assert_eq!(2, linebreak_property('\u{004B}'));
        assert_eq!(2, linebreak_property('\u{004E}'));
        assert_eq!(2, linebreak_property('\u{004F}'));
        assert_eq!(2, linebreak_property('\u{0050}'));
        assert_eq!(2, linebreak_property('\u{0053}'));
        assert_eq!(2, linebreak_property('\u{0056}'));
        assert_eq!(2, linebreak_property('\u{0058}'));
        assert_eq!(2, linebreak_property('\u{0059}'));
        assert_eq!(2, linebreak_property('\u{005A}'));
        assert_eq!(2, linebreak_property('\u{005F}'));
        assert_eq!(2, linebreak_property('\u{0063}'));
        assert_eq!(2, linebreak_property('\u{0068}'));
        assert_eq!(2, linebreak_property('\u{006B}'));
        assert_eq!(2, linebreak_property('\u{006D}'));
        assert_eq!(2, linebreak_property('\u{006F}'));
        assert_eq!(2, linebreak_property('\u{0072}'));
        assert_eq!(2, linebreak_property('\u{0074}'));
        assert_eq!(2, linebreak_property('\u{0076}'));
        assert_eq!(2, linebreak_property('\u{0078}'));
        assert_eq!(2, linebreak_property('\u{007A}'));
        assert_eq!(20, linebreak_property('\u{007B}'));
        assert_eq!(4, linebreak_property('\u{007C}'));
        assert_eq!(22, linebreak_property('\u{00B1}'));
        assert_eq!(2, linebreak_property('\u{00CB}'));
        assert_eq!(2, linebreak_property('\u{00F1}'));
        assert_eq!(2, linebreak_property('\u{011E}'));
        assert_eq!(2, linebreak_property('\u{014C}'));
        assert_eq!(2, linebreak_property('\u{021B}'));
        assert_eq!(2, linebreak_property('\u{021D}'));
        assert_eq!(2, linebreak_property('\u{0283}'));
        assert_eq!(2, linebreak_property('\u{02AD}'));
        assert_eq!(2, linebreak_property('\u{02B5}'));
        assert_eq!(2, linebreak_property('\u{02EF}'));
        assert_eq!(9, linebreak_property('\u{0306}'));
        assert_eq!(9, linebreak_property('\u{0345}'));
        assert_eq!(2, linebreak_property('\u{0398}'));
        assert_eq!(2, linebreak_property('\u{03BC}'));
        assert_eq!(2, linebreak_property('\u{03C7}'));
        assert_eq!(2, linebreak_property('\u{03DC}'));
        assert_eq!(2, linebreak_property('\u{0456}'));
        assert_eq!(2, linebreak_property('\u{0533}'));
        assert_eq!(2, linebreak_property('\u{0534}'));
        assert_eq!(2, linebreak_property('\u{053D}'));
        assert_eq!(9, linebreak_property('\u{05A3}'));
        assert_eq!(9, linebreak_property('\u{05A5}'));
        assert_eq!(0, linebreak_property('\u{05FD}'));
        assert_eq!(9, linebreak_property('\u{0612}'));
        assert_eq!(2, linebreak_property('\u{0622}'));
        assert_eq!(2, linebreak_property('\u{0634}'));
        assert_eq!(2, linebreak_property('\u{069B}'));
        assert_eq!(2, linebreak_property('\u{0721}'));
        assert_eq!(2, linebreak_property('\u{0723}'));
        assert_eq!(9, linebreak_property('\u{073F}'));
        assert_eq!(2, linebreak_property('\u{07E2}'));
        assert_eq!(19, linebreak_property('\u{0CE9}'));
        assert_eq!(9, linebreak_property('\u{0FA1}'));
        assert_eq!(9, linebreak_property('\u{0FA2}'));
        assert_eq!(19, linebreak_property('\u{1811}'));
        assert_eq!(2, linebreak_property('\u{2B69}'));
        assert_eq!(37, linebreak_property('\u{30A1}'));
        assert_eq!(14, linebreak_property('\u{3426}'));
        assert_eq!(14, linebreak_property('\u{453C}'));
        assert_eq!(14, linebreak_property('\u{4F16}'));
        assert_eq!(14, linebreak_property('\u{51B2}'));
        assert_eq!(14, linebreak_property('\u{6F3F}'));
        assert_eq!(14, linebreak_property('\u{70B1}'));
        assert_eq!(14, linebreak_property('\u{7A1B}'));
        assert_eq!(14, linebreak_property('\u{8246}'));
        assert_eq!(14, linebreak_property('\u{845F}'));
        assert_eq!(14, linebreak_property('\u{9413}'));
        assert_eq!(14, linebreak_property('\u{9718}'));
        assert_eq!(14, linebreak_property('\u{994B}'));
        assert_eq!(14, linebreak_property('\u{A3F6}'));
        assert_eq!(32, linebreak_property('\u{AC86}'));
        assert_eq!(32, linebreak_property('\u{AD39}'));
        assert_eq!(32, linebreak_property('\u{AEEB}'));
        assert_eq!(32, linebreak_property('\u{BC53}'));
        assert_eq!(32, linebreak_property('\u{D4FE}'));
        assert_eq!(0, linebreak_property('\u{E6AF}'));
        assert_eq!(0, linebreak_property('\u{EDAD}'));
        assert_eq!(0, linebreak_property('\u{F46A}'));
        assert_eq!(0, linebreak_property('\u{F76A}'));
        assert_eq!(14, linebreak_property('\u{F971}'));
        assert_eq!(2, linebreak_property('\u{FB17}'));
        assert_eq!(0, linebreak_property('\u{FD91}'));
        assert_eq!(9, linebreak_property('\u{FE07}'));
        assert_eq!(19, linebreak_property('\u{1113B}'));
        assert_eq!(14, linebreak_property('\u{29588}'));
        assert_eq!(14, linebreak_property('\u{2A4AD}'));
        assert_eq!(14, linebreak_property('\u{2ACE2}'));
        assert_eq!(14, linebreak_property('\u{2D717}'));
        assert_eq!(14, linebreak_property('\u{2F523}'));
        assert_eq!(14, linebreak_property('\u{31AF6}'));
        assert_eq!(14, linebreak_property('\u{3930D}'));
        assert_eq!(0, linebreak_property('\u{4A35E}'));
        assert_eq!(0, linebreak_property('\u{4CD46}'));
        assert_eq!(0, linebreak_property('\u{4ECA7}'));
        assert_eq!(0, linebreak_property('\u{57008}'));
        assert_eq!(0, linebreak_property('\u{6E98A}'));
        assert_eq!(0, linebreak_property('\u{79F16}'));
        assert_eq!(0, linebreak_property('\u{97559}'));
        assert_eq!(0, linebreak_property('\u{A9ADC}'));
        assert_eq!(0, linebreak_property('\u{AB06E}'));
        assert_eq!(0, linebreak_property('\u{B0D1D}'));
        assert_eq!(0, linebreak_property('\u{B607E}'));
        assert_eq!(0, linebreak_property('\u{B9766}'));
        assert_eq!(0, linebreak_property('\u{BAD4F}'));
        assert_eq!(0, linebreak_property('\u{BD41B}'));
        assert_eq!(0, linebreak_property('\u{BEEAB}'));
        assert_eq!(0, linebreak_property('\u{C8072}'));
        assert_eq!(0, linebreak_property('\u{E7604}'));
        assert_eq!(0, linebreak_property('\u{EBF8A}'));
        assert_eq!(0, linebreak_property('\u{F6BED}'));
        assert_eq!(0, linebreak_property('\u{F77FB}'));
        assert_eq!(0, linebreak_property('\u{FA8AD}'));
        assert_eq!(0, linebreak_property('\u{FC4CC}'));
        assert_eq!(0, linebreak_property('\u{FDC59}'));
        assert_eq!(0, linebreak_property('\u{10A02A}'));
    }

    #[test]
    fn linebreak_prop_str() {
        // autogenerated from LineBreak-17.0.0.txt by mk_tables.py --tests-str
        assert_eq!((9, 1), linebreak_property_str("\u{0001}", 0));
        assert_eq!((6, 1), linebreak_property_str("\u{000B}", 0));
        assert_eq!((10, 1), linebreak_property_str("\u{000D}", 0));
        assert_eq!((9, 1), linebreak_property_str("\u{0011}", 0));
        assert_eq!((9, 1), linebreak_property_str("\u{0015}", 0));
        assert_eq!((9, 1), linebreak_property_str("\u{0018}", 0));
        assert_eq!((9, 1), linebreak_property_str("\u{001A}", 0));
        assert_eq!((9, 1), linebreak_property_str("\u{001E}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{0023}", 0));
        assert_eq!((16, 1), linebreak_property_str("\u{002C}", 0));
        assert_eq!((27, 1), linebreak_property_str("\u{002F}", 0));
        assert_eq!((19, 1), linebreak_property_str("\u{0030}", 0));
        assert_eq!((19, 1), linebreak_property_str("\u{0032}", 0));
        assert_eq!((19, 1), linebreak_property_str("\u{0035}", 0));
        assert_eq!((16, 1), linebreak_property_str("\u{003B}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{0044}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{0047}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{004A}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{004B}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{0052}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{0056}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{0058}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{0059}", 0));
        assert_eq!((36, 1), linebreak_property_str("\u{005D}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{0060}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{0062}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{0066}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{006B}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{0075}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{0078}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{007A}", 0));
        assert_eq!((2, 1), linebreak_property_str("\u{007E}", 0));
        assert_eq!((9, 2), linebreak_property_str("\u{008F}", 0));
        assert_eq!((1, 2), linebreak_property_str("\u{00A7}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{00D0}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{00E0}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{012D}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{0198}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{01B9}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{0224}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{0264}", 0));
        assert_eq!((9, 2), linebreak_property_str("\u{0313}", 0));
        assert_eq!((9, 2), linebreak_property_str("\u{032A}", 0));
        assert_eq!((9, 2), linebreak_property_str("\u{0346}", 0));
        assert_eq!((9, 2), linebreak_property_str("\u{0352}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{0390}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{0393}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{039D}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{03E1}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{03EF}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{0439}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{048A}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{048E}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{0496}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{04B9}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{05C0}", 0));
        assert_eq!((9, 2), linebreak_property_str("\u{05C2}", 0));
        assert_eq!((38, 2), linebreak_property_str("\u{05E0}", 0));
        assert_eq!((9, 2), linebreak_property_str("\u{065D}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{06AD}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{06B0}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{070F}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{0769}", 0));
        assert_eq!((2, 2), linebreak_property_str("\u{07CB}", 0));
        assert_eq!((0, 3), linebreak_property_str("\u{0FEF}", 0));
        assert_eq!((0, 3), linebreak_property_str("\u{181F}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{3329}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{3E8F}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{45A0}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{45AA}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{5559}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{60E7}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{63F8}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{64AB}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{64AF}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{6511}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{677C}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{7651}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{7F56}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{8746}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{8A09}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{95AC}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{9BFB}", 0));
        assert_eq!((31, 3), linebreak_property_str("\u{BBDC}", 0));
        assert_eq!((32, 3), linebreak_property_str("\u{C7D0}", 0));
        assert_eq!((32, 3), linebreak_property_str("\u{CA06}", 0));
        assert_eq!((32, 3), linebreak_property_str("\u{CCE9}", 0));
        assert_eq!((32, 3), linebreak_property_str("\u{CD29}", 0));
        assert_eq!((32, 3), linebreak_property_str("\u{D07D}", 0));
        assert_eq!((32, 3), linebreak_property_str("\u{D3FF}", 0));
        assert_eq!((34, 3), linebreak_property_str("\u{D7D9}", 0));
        assert_eq!((34, 3), linebreak_property_str("\u{D7EF}", 0));
        assert_eq!((0, 3), linebreak_property_str("\u{E1D0}", 0));
        assert_eq!((0, 3), linebreak_property_str("\u{EAE3}", 0));
        assert_eq!((2, 3), linebreak_property_str("\u{FBDA}", 0));
        assert_eq!((14, 3), linebreak_property_str("\u{FFBB}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{15EED}", 0));
        assert_eq!((2, 4), linebreak_property_str("\u{1D536}", 0));
        assert_eq!((2, 4), linebreak_property_str("\u{1D6C7}", 0));
        assert_eq!((14, 4), linebreak_property_str("\u{27487}", 0));
        assert_eq!((14, 4), linebreak_property_str("\u{2C566}", 0));
        assert_eq!((14, 4), linebreak_property_str("\u{3D044}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{555AE}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{58E43}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{5B8A3}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{63D6D}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{9B5D4}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{9DC33}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{A25E2}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{A5F88}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{AA8B9}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{B0200}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{C1323}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{C1FB9}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{C39DA}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{CDC7C}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{D374C}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{D4DF4}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{D899A}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{D8BCE}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{DCB1F}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{E2480}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{E5E7B}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{E8B70}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{F2D78}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{F2EF0}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{102D43}", 0));
        assert_eq!((0, 4), linebreak_property_str("\u{10923A}", 0));
    }
    #[test]
    fn lb_iter_simple() {
        assert_eq!(
            vec![(6, false), (11, false)],
            LineBreakIterator::new("hello world").collect::<Vec<_>>()
        );

        // LB7, LB18
        assert_eq!(
            vec![(3, false), (4, false)],
            LineBreakIterator::new("a  b").collect::<Vec<_>>()
        );

        // LB5
        assert_eq!(vec![(2, true), (3, false)], LineBreakIterator::new("a\nb").collect::<Vec<_>>());
        assert_eq!(
            vec![(2, true), (4, true)],
            LineBreakIterator::new("\r\n\r\n").collect::<Vec<_>>()
        );

        // LB8a
        assert_eq!(
            vec![(7, false)],
            LineBreakIterator::new("\u{200D}\u{1F3FB}").collect::<Vec<_>>()
        );

        // LB10 combining mark after space
        assert_eq!(
            vec![(2, false), (4, false)],
            LineBreakIterator::new("a \u{301}").collect::<Vec<_>>()
        );

        // LB15 (old): QU × OP removed in Unicode 17; neutral QU followed by SP+OP now breaks
        assert_eq!(
            vec![(2, false), (3, false)],
            LineBreakIterator::new("\" [").collect::<Vec<_>>()
        );

        // LB17
        assert_eq!(
            vec![(2, false), (10, false), (11, false)],
            LineBreakIterator::new("a \u{2014} \u{2014} c").collect::<Vec<_>>()
        );

        // LB18: break after SP; LB18 (rule 18) has priority over LB19 (rule 19) for neutral QU.
        assert_eq!(
            vec![(2, false), (6, false), (7, false)],
            LineBreakIterator::new("a \"b\" c").collect::<Vec<_>>()
        );

        // LB21
        assert_eq!(vec![(2, false), (3, false)], LineBreakIterator::new("a-b").collect::<Vec<_>>());

        // LB21a: HL (HY|HH) ×.
        assert_eq!(
            vec![(5, false)],
            LineBreakIterator::new("\u{05D0}-\u{05D0}").collect::<Vec<_>>()
        );

        // LB23a
        assert_eq!(vec![(6, false)], LineBreakIterator::new("$\u{1F3FB}%").collect::<Vec<_>>());

        // LB30b
        assert_eq!(
            vec![(8, false)],
            LineBreakIterator::new("\u{1F466}\u{1F3FB}").collect::<Vec<_>>()
        );

        // LB31
        assert_eq!(
            vec![(8, false), (16, false)],
            LineBreakIterator::new("\u{1F1E6}\u{1F1E6}\u{1F1E6}\u{1F1E6}").collect::<Vec<_>>()
        );
    }

    #[test]
    // The final break is hard only when there is an explicit separator.
    fn lb_iter_eot() {
        assert_eq!(vec![(4, false)], LineBreakIterator::new("abc ").collect::<Vec<_>>());

        assert_eq!(vec![(4, true)], LineBreakIterator::new("abc\r").collect::<Vec<_>>());

        assert_eq!(vec![(5, true)], LineBreakIterator::new("abc\u{0085}").collect::<Vec<_>>());
    }
}
