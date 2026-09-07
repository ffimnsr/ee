//! Number cycling and text capitalization operations.
use super::*;

/// Changes the number(s) under the cursor(s) with the `transform_function`.
/// If there is a number next to or on the beginning of the region, then
/// this number will be replaced with the result of `transform_function` and
/// the cursor will be placed at the end of the number.
/// Some Examples with a increment `transform_function`:
///
/// "|1234" -> "1235|"
/// "12|34" -> "1235|"
/// "-|12" -> "-11|"
/// "another number is 123|]" -> "another number is 124"
///
/// This function also works fine with multiple regions.
pub fn change_number<F: Fn(i128) -> Option<i128>>(
    base: &Rope,
    regions: &[SelRegion],
    transform_function: F,
) -> RopeDelta {
    let mut builder = DeltaBuilder::new(base.len());
    for region in regions {
        let mut cursor = WordCursor::new(base, region.end);
        let (mut start, end) = cursor.select_word();

        // if the word begins with '-', then it is a negative number
        if start > 0 && base.byte_at(start - 1) == (b'-') {
            start -= 1;
        }

        let word = base.slice_to_cow(start..end);
        if let Some(number) = word.parse::<i128>().ok().and_then(&transform_function) {
            let interval = Interval::new(start, end);
            builder.replace(interval, Rope::from(number.to_string()));
        }
    }

    builder.build()
}

// capitalization behaviour is similar to behaviour in XCode
pub fn capitalize_text(base: &Rope, regions: &[SelRegion]) -> (RopeDelta, Selection) {
    let mut builder = DeltaBuilder::new(base.len());
    let mut final_selection = Selection::new();

    for &region in regions {
        final_selection.add_region(SelRegion::new(region.max(), region.max()));
        let mut word_cursor = WordCursor::new(base, region.min());

        loop {
            // capitalize each word in the current selection
            let (start, end) = word_cursor.select_word();

            if start < end {
                let interval = Interval::new(start, end);
                let word = base.slice_to_cow(start..end);

                // first letter is uppercase, remaining letters are lowercase
                let (first_char, rest) = word.split_at(1);
                let capitalized_text = [first_char.to_uppercase(), rest.to_lowercase()].concat();
                builder.replace(interval, Rope::from(capitalized_text));
            }

            if word_cursor.next_boundary().is_none() || end > region.max() {
                break;
            }
        }
    }

    (builder.build(), final_selection)
}
