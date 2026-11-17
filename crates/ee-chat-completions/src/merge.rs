//! Bounded deep merge for provider body overrides.
//!
//! The reference agent merges `http.body` and provider options over the
//! provider-native body instead of replacing it
//! (`packages/ai/src/schema/options.ts:85-110`), so a partial override such as
//! `{"reasoning":{"effort":"high"}}` keeps the sibling keys EE already wrote.
//! Top-level insertion, which this module replaces, silently dropped them.
//!
//! Merge rules:
//! - two objects merge recursively, key by key;
//! - anything else replaces the target wholesale, so arrays are never
//!   concatenated;
//! - a `null` patch value deletes the key; it is never sent as a body field.
//!
//! The recursion is bounded by depth and by a total node budget, so a hostile
//! or accidental deeply nested override cannot blow the stack. When a bound is
//! reached the remaining patch replaces its target, which is deterministic and
//! never partial and silent.

use serde_json::{Map, Value};

/// Largest object nesting a merge follows before replacing wholesale.
pub const MAX_MERGE_DEPTH: usize = 32;

/// Largest number of patch keys one merge visits before replacing wholesale.
pub const MAX_MERGE_NODES: usize = 4096;

/// Merges `patch` into `target` in place.
///
/// `target` and `patch` are expected to be JSON objects at the top level; any
/// other shape makes this a replacement.
pub fn deep_merge(target: &mut Value, patch: &Value) {
    let mut budget = MAX_MERGE_NODES;
    merge_at(target, patch, 0, &mut budget);
}

fn merge_at(target: &mut Value, patch: &Value, depth: usize, budget: &mut usize) {
    if depth >= MAX_MERGE_DEPTH || *budget == 0 {
        *target = patch.clone();
        return;
    }
    let (Some(target_object), Some(patch_object)) = (target.as_object_mut(), patch.as_object())
    else {
        *target = patch.clone();
        return;
    };
    merge_objects(target_object, patch_object, depth, budget);
}

fn merge_objects(
    target: &mut Map<String, Value>,
    patch: &Map<String, Value>,
    depth: usize,
    budget: &mut usize,
) {
    for (key, value) in patch {
        if *budget == 0 {
            target.insert(key.clone(), value.clone());
            continue;
        }
        *budget -= 1;
        if value.is_null() {
            target.remove(key);
            continue;
        }
        match target.get_mut(key) {
            Some(existing) => merge_at(existing, value, depth + 1, budget),
            None => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn nested_objects_merge_and_keep_siblings() {
        let mut target =
            json!({ "reasoning": { "effort": "low", "summary": "auto" }, "model": "m" });
        deep_merge(&mut target, &json!({ "reasoning": { "effort": "high" } }));

        assert_eq!(
            target,
            json!({ "reasoning": { "effort": "high", "summary": "auto" }, "model": "m" })
        );
    }

    #[test]
    fn arrays_replace_wholesale() {
        let mut target = json!({ "include": ["a", "b"] });
        deep_merge(&mut target, &json!({ "include": ["c"] }));
        assert_eq!(target, json!({ "include": ["c"] }));
    }

    #[test]
    fn null_deletes_the_key() {
        let mut target = json!({ "store": true, "model": "m" });
        deep_merge(&mut target, &json!({ "store": null }));
        assert_eq!(target, json!({ "model": "m" }));
    }

    #[test]
    fn scalars_replace_objects_and_objects_replace_scalars() {
        let mut target = json!({ "a": { "b": 1 }, "c": 2 });
        deep_merge(&mut target, &json!({ "a": 5, "c": { "d": 2 } }));
        assert_eq!(target, json!({ "a": 5, "c": { "d": 2 } }));
    }

    #[test]
    fn depth_cap_replace_is_deterministic() {
        fn nested(depth: usize) -> Value {
            let mut value = json!({ "leaf": true });
            for _ in 0..depth {
                value = json!({ "next": value });
            }
            value
        }
        let mut target = nested(MAX_MERGE_DEPTH + 4);
        let patch = nested(MAX_MERGE_DEPTH + 4);
        deep_merge(&mut target, &patch);
        assert_eq!(target, patch, "beyond the cap the patch replaces its target");
    }

    #[test]
    fn non_object_target_is_replaced() {
        let mut target = json!([1, 2]);
        deep_merge(&mut target, &json!({ "a": 1 }));
        assert_eq!(target, json!({ "a": 1 }));
    }
}
