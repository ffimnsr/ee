// Copyright 2017 The xi-editor Authors.
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

//! Tracks plugin-provided scope spans.

use log::{info, warn};

use std::collections::{BTreeMap, HashSet};
use syntect::parsing::Scope;

use xi_rope::spans::{Spans, SpansBuilder};
use xi_rope::{Interval, RopeDelta};

use crate::plugins::PluginPid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedScopeSpan {
    pub start: usize,
    pub end: usize,
    pub scope: String,
}

/// A collection of layers containing scope information.
#[derive(Default)]
pub struct Layers {
    layers: BTreeMap<PluginPid, ScopeLayer>,
    deleted: HashSet<PluginPid>,
    shape: Spans<()>,
}

/// A collection of scope spans from a single source.
#[derive(Default)]
pub struct ScopeLayer {
    stack_lookup: Vec<Vec<Scope>>,
    /// Human readable scope names, for debugging
    scope_spans: Spans<u32>,
}

impl Layers {
    /// Adds the provided scopes to the layer's lookup table.
    pub fn add_scopes(&mut self, layer: PluginPid, scopes: Vec<Vec<String>>) {
        let _t = tracing::trace_span!("Layers::AddScopes", categories = "core").entered();
        if self.create_if_missing(layer).is_err() {
            return;
        }
        if let Some(scope_layer) = self.layers.get_mut(&layer) {
            scope_layer.add_scopes(scopes);
        } else {
            warn!("scope layer {:?} missing after creation", layer);
        }
    }

    /// Applies the delta to all layers, inserting empty intervals
    /// for any regions inserted in the delta.
    ///
    /// This is useful for clearing spans, and for updating spans
    /// as edits occur.
    pub fn update_all(&mut self, delta: &RopeDelta) {
        self.shape.apply_shape(delta);

        for layer in self.layers.values_mut() {
            layer.blank_scopes(delta);
        }
    }

    /// Updates the scope spans for a given layer.
    pub fn update_layer(&mut self, layer: PluginPid, iv: Interval, spans: Spans<u32>) {
        if self.create_if_missing(layer).is_err() {
            return;
        }
        if let Some(scope_layer) = self.layers.get_mut(&layer) {
            scope_layer.update_scopes(iv, &spans);
        } else {
            warn!("scope layer {:?} missing after creation", layer);
        }
    }

    /// Removes a given layer.
    pub fn remove_layer(&mut self, layer: PluginPid) -> Option<ScopeLayer> {
        self.deleted.insert(layer);
        self.layers.remove(&layer)
    }

    /// Prints scopes for the given `Interval`.
    pub fn debug_print_spans(&self, iv: Interval) {
        for (id, layer) in &self.layers {
            let spans = layer.scope_spans.subseq(iv);
            if spans.iter().next().is_some() {
                info!("scopes for layer {:?}:", id);
                for (iv, val) in spans.iter() {
                    info!("{}: {:?}", iv, layer.stack_lookup[*val as usize]);
                }
            }
        }
    }

    pub fn resolved_spans(&self, iv: Interval) -> Vec<ResolvedScopeSpan> {
        self.layers
            .values()
            .find_map(|layer| {
                let spans = layer.resolved_spans(iv);
                (!spans.is_empty()).then_some(spans)
            })
            .unwrap_or_default()
    }

    /// Returns an `Err` if this layer has been deleted; the caller should return.
    fn create_if_missing(&mut self, layer_id: PluginPid) -> Result<(), ()> {
        if self.deleted.contains(&layer_id) {
            return Err(());
        }
        if !self.layers.contains_key(&layer_id) {
            self.layers.insert(layer_id, ScopeLayer::new(self.shape.len()));
        }
        Ok(())
    }
}

impl ScopeLayer {
    pub fn new(len: usize) -> Self {
        ScopeLayer { stack_lookup: Vec::new(), scope_spans: SpansBuilder::new(len).build() }
    }

    fn add_scopes(&mut self, scopes: Vec<Vec<String>>) {
        let mut stacks = Vec::with_capacity(scopes.len());
        for stack in scopes {
            let scopes = stack
                .iter()
                .map(|s| Scope::new(s))
                .filter(|result| match *result {
                    Err(ref err) => {
                        warn!("failed to resolve scope {}\nErr: {:?}", &stack.join(" "), err);
                        false
                    }
                    _ => true,
                })
                .map(|s| s.unwrap())
                .collect::<Vec<_>>();
            stacks.push(scopes);
        }

        self.stack_lookup.append(&mut stacks);
    }

    fn update_scopes(&mut self, iv: Interval, spans: &Spans<u32>) {
        self.scope_spans.edit(iv, spans.to_owned());
    }

    fn resolved_spans(&self, iv: Interval) -> Vec<ResolvedScopeSpan> {
        self.scope_spans
            .subseq(iv)
            .iter()
            .filter_map(|(span_iv, scope_id)| {
                let stack = self.stack_lookup.get(*scope_id as usize)?;
                let scope = stack.iter().map(|scope| scope.to_string()).collect::<Vec<_>>().join(" ");
                if scope.is_empty() {
                    return None;
                }
                Some(ResolvedScopeSpan { start: span_iv.start(), end: span_iv.end(), scope })
            })
            .collect()
    }

    /// Applies `delta`, which is presumed to contain empty spans.
    /// This is only used when we receive an edit, to adjust current span
    /// positions.
    fn blank_scopes(&mut self, delta: &RopeDelta) {
        self.scope_spans.apply_shape(delta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_spans_return_line_local_scope_strings() {
        let mut layers = Layers::default();
        layers.add_scopes(
            PluginPid(1),
            vec![
                vec!["keyword.control.rust".into()],
                vec!["constant.numeric.decimal.rust".into()],
            ],
        );

        let mut builder = SpansBuilder::new(16);
        builder.add_span(Interval::new(0, 3), 0);
        builder.add_span(Interval::new(8, 10), 1);
        layers.update_layer(PluginPid(1), Interval::new(0, 16), builder.build());

        let spans = layers.resolved_spans(Interval::new(0, 12));
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].start, 0);
        assert_eq!(spans[0].end, 3);
        assert_eq!(spans[0].scope, "keyword.control.rust");
        assert_eq!(spans[1].start, 8);
        assert_eq!(spans[1].end, 10);
        assert_eq!(spans[1].scope, "constant.numeric.decimal.rust");
    }
}
