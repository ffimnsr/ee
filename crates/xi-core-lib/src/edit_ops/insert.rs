//! Text insert and surround edit operations.
use super::delete::delete_sel_regions;
use super::*;

pub fn insert<T: Into<Rope>>(base: &Rope, regions: &[SelRegion], text: T) -> RopeDelta {
    let rope = text.into();
    let mut builder = DeltaBuilder::new(base.len());
    for region in regions {
        let iv = Interval::new(region.min(), region.max());
        builder.replace(iv, rope.clone());
    }

    builder.build()
}

/// Replaces each region while adapting replacement casing to matched text.
pub fn insert_preserving_case(base: &Rope, regions: &[SelRegion], text: &str) -> RopeDelta {
    let mut builder = DeltaBuilder::new(base.len());
    for region in regions {
        let iv = Interval::new(region.min(), region.max());
        let matched = base.slice_to_cow(iv);
        builder.replace(iv, Rope::from(match_case(&matched, text)));
    }

    builder.build()
}

pub(crate) fn match_case(matched: &str, replacement: &str) -> String {
    let cased: Vec<char> = matched
        .chars()
        .filter(|character| character.is_lowercase() || character.is_uppercase())
        .collect();
    if cased.is_empty() {
        return replacement.to_owned();
    }
    if cased.iter().all(|character| character.is_uppercase()) {
        return replacement.to_uppercase();
    }
    if cased.iter().all(|character| character.is_lowercase()) {
        return replacement.to_lowercase();
    }
    if cased[0].is_uppercase() && cased[1..].iter().all(|character| character.is_lowercase()) {
        let mut result = String::with_capacity(replacement.len());
        let mut first_cased = true;
        for character in replacement.chars() {
            if character.is_lowercase() || character.is_uppercase() {
                if first_cased {
                    result.extend(character.to_uppercase());
                    first_cased = false;
                } else {
                    result.extend(character.to_lowercase());
                }
            } else {
                result.push(character);
            }
        }
        return result;
    }

    replacement.to_owned()
}

/// Leaves the current selection untouched, but surrounds it with two insertions.
pub fn surround<BT, AT>(
    base: &Rope,
    regions: &[SelRegion],
    before_text: BT,
    after_text: AT,
) -> RopeDelta
where
    BT: Into<Rope>,
    AT: Into<Rope>,
{
    let mut builder = DeltaBuilder::new(base.len());
    let before_rope = before_text.into();
    let after_rope = after_text.into();
    for region in regions {
        let before_iv = Interval::new(region.min(), region.min());
        builder.replace(before_iv, before_rope.clone());
        let after_iv = Interval::new(region.max(), region.max());
        builder.replace(after_iv, after_rope.clone());
    }

    builder.build()
}

pub fn duplicate_line(base: &Rope, regions: &[SelRegion], config: &BufferItems) -> RopeDelta {
    let mut builder = DeltaBuilder::new(base.len());
    // get affected lines or regions
    let mut to_duplicate = BTreeSet::new();

    for region in regions {
        let (first_line, _) = LogicalLines.offset_to_line_col(base, region.min());
        let line_start = LogicalLines.offset_of_line(base, first_line);

        let mut cursor = match region.is_caret() {
            true => Cursor::new(base, line_start),
            false => {
                // duplicate all lines together that are part of the same selections
                let (last_line, _) = LogicalLines.offset_to_line_col(base, region.max());
                let line_end = LogicalLines.offset_of_line(base, last_line);
                Cursor::new(base, line_end)
            }
        };

        if let Some(line_end) = cursor.next::<LinesMetric>() {
            to_duplicate.insert((line_start, line_end));
        }
    }

    for (start, end) in to_duplicate {
        // insert duplicates
        let iv = Interval::new(start, start);
        builder.replace(iv, base.slice(start..end));

        // last line does not have new line character so it needs to be manually added
        if end == base.len() {
            builder.replace(iv, Rope::from(&config.line_ending))
        }
    }

    builder.build()
}

/// Used when the user presses the backspace key. If no delta is returned, then nothing changes.
pub fn delete_backward(base: &Rope, regions: &[SelRegion], config: &BufferItems) -> RopeDelta {
    // Backspace stays separate from generic movement deletion because indentation-aware
    // behavior depends on buffer configuration, not viewport movement semantics.
    let mut deletions = Selection::new();
    for region in regions {
        let start = offset_for_delete_backwards(region, base, config);
        let iv = Interval::new(start, region.max());
        if !iv.is_empty() {
            deletions.add_region(SelRegion::new(iv.start(), iv.end()));
        }
    }

    delete_sel_regions(base, &deletions)
}
