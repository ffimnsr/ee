//! `impl Engine` undo machinery.
use super::*;

impl Engine {
    pub(super) fn empty_subset_before_first_rev(&self) -> Subset {
        let first_rev = &self.revs.first().unwrap();
        // it will be immediately transform_expanded by inserts if it is an Edit, so length must be before
        let len = match first_rev.edit {
            Edit { ref inserts, .. } => inserts.count(CountMatcher::Zero),
            Undo { ref deletes_bitxor, .. } => deletes_bitxor.count(CountMatcher::All),
        };
        Subset::new(len)
    }

    /// Find the first revision that could be affected by toggling a set of undo groups
    pub(super) fn find_first_undo_candidate_index(&self, toggled_groups: &OrdSet<usize>) -> usize {
        // find the lowest toggled undo group number
        if let Some(lowest_group) = toggled_groups.iter().next().copied() {
            for (i, rev) in self.revs.iter().enumerate().rev() {
                if rev.max_undo_so_far < lowest_group {
                    return i + 1; // +1 since we know the one we just found doesn't have it
                }
            }
            0
        } else {
            // no toggled groups, return past end
            self.revs.len()
        }
    }

    // This computes undo all the way from the beginning. An optimization would be to not
    // recompute the prefix up to where the history diverges, but it's not clear that's
    // even worth the code complexity.
    pub(super) fn deletes_from_union_for_undone_groups(&self, groups: &OrdSet<usize>) -> Subset {
        let mut deletes_from_union = self.empty_subset_before_first_rev();

        for rev in &self.revs {
            if let Edit { ref undo_group, ref inserts, ref deletes, .. } = rev.edit {
                if groups.contains(undo_group) {
                    if !inserts.is_empty() {
                        deletes_from_union = deletes_from_union.transform_union(inserts);
                    }
                } else {
                    if !inserts.is_empty() {
                        deletes_from_union = deletes_from_union.transform_expand(inserts);
                    }
                    if !deletes.is_empty() {
                        deletes_from_union = deletes_from_union.union(deletes);
                    }
                }
            }
        }

        deletes_from_union
    }

    pub(super) fn compute_undo(&self, groups: &OrdSet<usize>) -> (Revision, Subset) {
        let toggled_groups = self.undone_groups.clone().symmetric_difference(groups.clone());
        let first_candidate = self.find_first_undo_candidate_index(&toggled_groups);
        // the `false` below: don't invert undos since our first_candidate is based on the current undo set, not past
        let mut deletes_from_union =
            self.deletes_from_union_before_index(first_candidate, false).into_owned();

        for rev in &self.revs[first_candidate..] {
            if let Edit { ref undo_group, ref inserts, ref deletes, .. } = rev.edit {
                if groups.contains(undo_group) {
                    if !inserts.is_empty() {
                        deletes_from_union = deletes_from_union.transform_union(inserts);
                    }
                } else {
                    if !inserts.is_empty() {
                        deletes_from_union = deletes_from_union.transform_expand(inserts);
                    }
                    if !deletes.is_empty() {
                        deletes_from_union = deletes_from_union.union(deletes);
                    }
                }
            }
        }

        let deletes_bitxor = self.deletes_from_union.bitxor(&deletes_from_union);
        let max_undo_so_far = self.revs.last().unwrap().max_undo_so_far;
        (
            Revision {
                rev_id: self.next_rev_id(),
                max_undo_so_far,
                edit: Undo { toggled_groups, deletes_bitxor },
            },
            deletes_from_union,
        )
    }

    pub fn undo<I, T>(&mut self, groups: I)
    where
        I: IntoIterator<Item = T>,
        T: Borrow<usize>,
    {
        let groups: OrdSet<usize> = groups.into_iter().map(|group| *group.borrow()).collect();
        let (new_rev, new_deletes_from_union) = self.compute_undo(&groups);

        if matches!(
            new_rev.edit,
            Undo { ref deletes_bitxor, .. } if deletes_bitxor.is_empty()
        ) {
            self.undone_groups = groups;
            return;
        }

        let (new_text, new_tombstones) = shuffle(
            &self.text,
            &self.tombstones,
            &self.deletes_from_union,
            &new_deletes_from_union,
        );

        self.text = new_text;
        self.tombstones = new_tombstones;
        self.deletes_from_union = new_deletes_from_union;
        self.undone_groups = groups;
        self.revs.push(new_rev);
        self.rev_id_counter += 1;
    }

    pub fn is_equivalent_revision(&self, base_rev: RevId, other_rev: RevId) -> bool {
        let base_subset = self
            .find_rev(base_rev)
            .map(|rev_index| self.deletes_from_cur_union_for_index(rev_index));
        let other_subset = self
            .find_rev(other_rev)
            .map(|rev_index| self.deletes_from_cur_union_for_index(rev_index));

        base_subset.is_some() && base_subset == other_subset
    }
}
