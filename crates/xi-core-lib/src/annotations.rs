// Copyright 2018 The xi-editor Authors.
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

//! Management of annotations.

use serde::de::Deserializer;
use serde::ser::{SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{self, Value, json};

use crate::view::View;
use xi_rope::Interval;
use xi_rope::Rope;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum AnnotationType {
    Selection,
    Find,
    Other(String),
}

impl AnnotationType {
    fn as_str(&self) -> &str {
        match self {
            AnnotationType::Find => "find",
            AnnotationType::Selection => "selection",
            AnnotationType::Other(s) => s,
        }
    }
}

/// Location and range of an annotation ([start_line, start_col, end_line, end_col]).
/// Location and range of an annotation
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct AnnotationRange {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

impl Serialize for AnnotationRange {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(4))?;
        seq.serialize_element(&self.start_line)?;
        seq.serialize_element(&self.start_col)?;
        seq.serialize_element(&self.end_line)?;
        seq.serialize_element(&self.end_col)?;
        seq.end()
    }
}

impl<'de> Deserialize<'de> for AnnotationRange {
    fn deserialize<D>(deserializer: D) -> Result<AnnotationRange, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut range = AnnotationRange { ..Default::default() };
        let seq = <[usize; 4]>::deserialize(deserializer)?;

        range.start_line = seq[0];
        range.start_col = seq[1];
        range.end_line = seq[2];
        range.end_col = seq[3];

        Ok(range)
    }
}

/// A set of annotations of a given type.
/// A region of an `Annotation`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AnnotationSlice {
    annotation_type: AnnotationType,
    /// Annotation occurrences, guaranteed non-descending start order.
    ranges: Vec<AnnotationRange>,
    /// If present, one payload per range.
    payloads: Option<Vec<Value>>,
}

impl AnnotationSlice {
    pub fn new(
        annotation_type: AnnotationType,
        ranges: Vec<AnnotationRange>,
        payloads: Option<Vec<Value>>,
    ) -> Self {
        AnnotationSlice { annotation_type, ranges, payloads }
    }

    /// Returns json representation.
    pub fn to_json(&self) -> Value {
        json!({
            "type": self.annotation_type.as_str(),
            "ranges": self.ranges,
            "payloads": self.payloads,
            "n": self.ranges.len()
        })
    }
}

/// A trait for types (like `Selection`) that have a distinct representation
/// in core but are presented to the frontend as annotations.
pub trait ToAnnotation {
    /// Returns annotations that overlap the provided interval.
    fn get_annotations(&self, interval: Interval, view: &View, text: &Rope) -> AnnotationSlice;
}

/// All the annotations for a given view
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_annotation_range_serialization() {
        let range = AnnotationRange { start_line: 1, start_col: 3, end_line: 4, end_col: 1 };

        assert_eq!(json!(range).to_string(), "[1,3,4,1]")
    }

    #[test]
    fn test_annotation_range_deserialization() {
        let range: AnnotationRange = serde_json::from_str("[1,3,4,1]").unwrap();
        assert_eq!(range, AnnotationRange { start_line: 1, start_col: 3, end_line: 4, end_col: 1 })
    }

    #[test]
    fn test_annotation_slice_json() {
        let range = AnnotationRange { start_line: 1, start_col: 3, end_line: 4, end_col: 1 };

        let slice = AnnotationSlice {
            annotation_type: AnnotationType::Find,
            ranges: vec![range],
            payloads: None,
        };

        assert_eq!(
            slice.to_json(),
            json!({
                "n": 1,
                "payloads": null,
                "ranges": [[1, 3, 4, 1]],
                "type": "find",
            })
        )
    }
}
