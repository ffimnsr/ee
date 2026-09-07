//! `impl Engine` CRDT merge plus the merge/rebase helper machinery.
use super::*;

impl Engine {
    pub fn merge(&mut self, other: &Engine) {
        let (mut new_revs, remote_undos, text, tombstones, deletes_from_union) = {
            let base_index = find_base_index(&self.revs, &other.revs);
            let a_to_merge = &self.revs[base_index..];
            let b_to_merge = &other.revs[base_index..];

            let common = find_common(a_to_merge, b_to_merge);
            let remote_undos = b_to_merge
                .iter()
                .filter(|rev| !common.contains(&rev.rev_id))
                .filter_map(|rev| match rev.edit {
                    Undo { ref toggled_groups, .. } => {
                        Some((rev.rev_id, rev.max_undo_so_far, toggled_groups.clone()))
                    }
                    Edit { .. } => None,
                })
                .collect::<Vec<_>>();

            let a_new = rearrange(a_to_merge, &common, self.deletes_from_union.len());
            let b_new = rearrange(b_to_merge, &common, other.deletes_from_union.len());

            let b_deltas =
                compute_deltas(&b_new, &other.text, &other.tombstones, &other.deletes_from_union);
            let expand_by = compute_transforms(a_new);

            let max_undo = self.max_undo_group_id();
            let (new_revs, text, tombstones, deletes_from_union) = rebase(
                expand_by,
                b_deltas,
                self.text.clone(),
                self.tombstones.clone(),
                self.deletes_from_union.clone(),
                max_undo,
            );
            (new_revs, remote_undos, text, tombstones, deletes_from_union)
        };

        self.text = text;
        self.tombstones = tombstones;
        self.deletes_from_union = deletes_from_union;
        self.revs.append(&mut new_revs);
        for (rev_id, max_undo_so_far, toggled_groups) in remote_undos {
            self.merge_undo_revision(rev_id, max_undo_so_far, toggled_groups);
        }
        self.reconcile_undo_state();
    }

    pub(super) fn merge_undo_revision(
        &mut self,
        rev_id: RevId,
        max_undo_so_far: usize,
        toggled_groups: OrdSet<usize>,
    ) {
        self.revs.push(Revision {
            rev_id,
            max_undo_so_far: std::cmp::max(max_undo_so_far, self.max_undo_group_id()),
            edit: Undo {
                toggled_groups,
                deletes_bitxor: Subset::new(self.deletes_from_union.len()),
            },
        });
    }

    pub(super) fn reconcile_undo_state(&mut self) {
        let groups = self
            .revs
            .iter()
            .filter_map(|rev| match rev.edit {
                Undo { ref toggled_groups, .. } if !toggled_groups.is_empty() => {
                    Some(toggled_groups.clone())
                }
                _ => None,
            })
            .fold(OrdSet::new(), |groups, toggled| groups.symmetric_difference(toggled));
        let new_deletes_from_union = self.deletes_from_union_for_undone_groups(&groups);
        if new_deletes_from_union != self.deletes_from_union {
            let (new_text, new_tombstones) = shuffle(
                &self.text,
                &self.tombstones,
                &self.deletes_from_union,
                &new_deletes_from_union,
            );
            self.text = new_text;
            self.tombstones = new_tombstones;
            self.deletes_from_union = new_deletes_from_union;
        }
        self.undone_groups = groups;
    }
}

// ======== Merge helpers

/// Find an index before which everything is the same
pub(super) fn find_base_index(a: &[Revision], b: &[Revision]) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }

    a.iter().zip(b.iter()).take_while(|(left, right)| left.rev_id == right.rev_id).count()
}

/// Find a set of revisions common to both lists
pub(super) fn find_common(a: &[Revision], b: &[Revision]) -> BTreeSet<RevId> {
    let (smaller, larger) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let smaller_ids: HashSet<RevId> = smaller.iter().map(|rev| rev.rev_id).collect();
    larger
        .iter()
        .filter_map(|rev| smaller_ids.contains(&rev.rev_id).then_some(rev.rev_id))
        .collect()
}

/// Returns the operations in `revs` that don't have their `rev_id` in
/// `base_revs`, but modified so that they are in the same order but based on
/// the `base_revs`. This allows the rest of the merge to operate on only
/// revisions not shared by both sides.
///
/// Conceptually, see the diagram below, with `.` being base revs and `n` being
/// non-base revs, `N` being transformed non-base revs, and rearranges it:
/// .n..n...nn..  -> ........NNNN -> returns vec![N,N,N,N]
///
/// Undo revisions do not affect insert ordering, so they are skipped here and
/// replayed separately during merge.
pub(super) fn rearrange(
    revs: &[Revision],
    base_revs: &BTreeSet<RevId>,
    head_len: usize,
) -> Vec<Revision> {
    // transform representing the characters added by common revisions after a point.
    let mut s = Subset::new(head_len);

    let mut out = Vec::with_capacity(revs.len() - base_revs.len());
    for rev in revs.iter().rev() {
        let is_base = base_revs.contains(&rev.rev_id);
        let contents = match rev.edit {
            Contents::Edit { priority, undo_group, ref inserts, ref deletes } => {
                if is_base {
                    s = inserts.transform_union(&s);
                    None
                } else {
                    // fast-forward this revision over all common ones after it
                    let transformed_inserts = inserts.transform_expand(&s);
                    let transformed_deletes = deletes.transform_expand(&s);
                    // we don't want new revisions before this to be transformed after us
                    s = s.transform_shrink(&transformed_inserts);
                    Some(Contents::Edit {
                        inserts: transformed_inserts,
                        deletes: transformed_deletes,
                        priority,
                        undo_group,
                    })
                }
            }
            Contents::Undo { .. } => None,
        };
        if let Some(edit) = contents {
            out.push(Revision { edit, rev_id: rev.rev_id, max_undo_so_far: rev.max_undo_so_far });
        }
    }

    out.as_mut_slice().reverse();
    out
}

#[derive(Clone, Debug)]
pub(super) struct DeltaOp {
    pub(super) rev_id: RevId,
    pub(super) priority: usize,
    pub(super) undo_group: usize,
    pub(super) inserts: InsertDelta<RopeInfo>,
    pub(super) deletes: Subset,
}

/// Transform `revs`, which doesn't include information on the actual content of the operations,
/// into an `InsertDelta`-based representation that does by working backward from the text and tombstones.
pub(super) fn compute_deltas(
    revs: &[Revision],
    text: &Rope,
    tombstones: &Rope,
    deletes_from_union: &Subset,
) -> Vec<DeltaOp> {
    let mut out = Vec::with_capacity(revs.len());

    let mut cur_all_inserts = Subset::new(deletes_from_union.len());
    for rev in revs.iter().rev() {
        match rev.edit {
            Contents::Edit { priority, undo_group, ref inserts, ref deletes } => {
                let older_all_inserts = inserts.transform_union(&cur_all_inserts);

                let tombstones_here =
                    shuffle_tombstones(text, tombstones, deletes_from_union, &older_all_inserts);
                let delta =
                    Delta::synthesize(&tombstones_here, &older_all_inserts, &cur_all_inserts);
                let (ins, _) = delta.factor();
                out.push(DeltaOp {
                    rev_id: rev.rev_id,
                    priority,
                    undo_group,
                    inserts: ins,
                    deletes: deletes.clone(),
                });

                cur_all_inserts = older_all_inserts;
            }
            Contents::Undo { .. } => panic!("can't merge undo yet"),
        }
    }

    out.as_mut_slice().reverse();
    out
}

/// Computes a series of priorities and transforms for the deltas on the right
/// from the new revisions on the left.
///
/// Applies an optimization where it combines sequential revisions with the
/// same priority into one transform to decrease the number of transforms that
/// have to be considered in `rebase` substantially for normal editing
/// patterns. Any large runs of typing in the same place by the same user (e.g
/// typing a paragraph) will be combined into a single segment in a transform
/// as opposed to thousands of revisions.
pub(super) fn compute_transforms(revs: Vec<Revision>) -> Vec<(FullPriority, Subset)> {
    let mut out = Vec::new();
    let mut last_priority: Option<usize> = None;
    for r in revs {
        if let Contents::Edit { priority, inserts, .. } = r.edit {
            if inserts.is_empty() {
                continue;
            }
            if Some(priority) == last_priority {
                let last: &mut (FullPriority, Subset) = out.last_mut().unwrap();
                last.1 = last.1.transform_union(&inserts);
            } else {
                last_priority = Some(priority);
                let prio = FullPriority { priority, session_id: r.rev_id.session_id() };
                out.push((prio, inserts));
            }
        }
    }
    out
}

/// Rebase `b_new` on top of `expand_by` and return revision contents that can be appended as new
/// revisions on top of the revisions represented by `expand_by`.
pub(super) fn rebase(
    mut expand_by: Vec<(FullPriority, Subset)>,
    b_new: Vec<DeltaOp>,
    mut text: Rope,
    mut tombstones: Rope,
    mut deletes_from_union: Subset,
    mut max_undo_so_far: usize,
) -> (Vec<Revision>, Rope, Rope, Subset) {
    let mut out = Vec::with_capacity(b_new.len());

    let mut next_expand_by = Vec::with_capacity(expand_by.len());
    for op in b_new {
        let DeltaOp { rev_id, priority, undo_group, mut inserts, mut deletes } = op;
        let full_priority = FullPriority { priority, session_id: rev_id.session_id() };
        // expand by each in expand_by
        for &(trans_priority, ref trans_inserts) in &expand_by {
            // Equal priorities only happen for identical revisions, which do not rebase here.
            let after = full_priority >= trans_priority;
            // d-expand by other
            inserts = inserts.transform_expand(trans_inserts, after);
            // trans-expand other by expanded so they have the same context
            let inserted = inserts.inserted_subset();
            let new_trans_inserts = trans_inserts.transform_expand(&inserted);
            // The deletes are already after our inserts, but we need to include the other inserts
            deletes = deletes.transform_expand(&new_trans_inserts);
            // On the next step we want things in expand_by to have op in the context
            next_expand_by.push((trans_priority, new_trans_inserts));
        }

        let text_inserts = inserts.transform_shrink(&deletes_from_union);
        let text_with_inserts = text_inserts.apply(&text);
        let inserted = inserts.inserted_subset();

        let expanded_deletes_from_union = deletes_from_union.transform_expand(&inserted);
        let new_deletes_from_union = expanded_deletes_from_union.union(&deletes);
        let (new_text, new_tombstones) = shuffle(
            &text_with_inserts,
            &tombstones,
            &expanded_deletes_from_union,
            &new_deletes_from_union,
        );

        text = new_text;
        tombstones = new_tombstones;
        deletes_from_union = new_deletes_from_union;

        max_undo_so_far = std::cmp::max(max_undo_so_far, undo_group);
        out.push(Revision {
            rev_id,
            max_undo_so_far,
            edit: Contents::Edit { priority, undo_group, deletes, inserts: inserted },
        });

        expand_by = next_expand_by;
        next_expand_by = Vec::with_capacity(expand_by.len());
    }

    (out, text, tombstones, deletes_from_union)
}
