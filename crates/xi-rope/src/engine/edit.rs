//! `impl Engine` edit application.
use super::*;

impl Engine {
    pub(super) fn mk_new_rev(
        &self,
        new_priority: usize,
        undo_group: usize,
        base_rev: RevToken,
        delta: Delta<RopeInfo>,
    ) -> Result<(Revision, Rope, Rope, Subset), Error> {
        let ix = self.find_rev_token(base_rev).ok_or(Error::MissingRevision(base_rev))?;

        let (ins_delta, deletes) = delta.factor();

        // rebase delta to be on the base_rev union instead of the text
        let deletes_at_rev = self.deletes_from_union_for_index(ix);

        // validate delta
        if ins_delta.base_len != deletes_at_rev.len_after_delete() {
            return Err(Error::MalformedDelta {
                delta_len: ins_delta.base_len,
                rev_len: deletes_at_rev.len_after_delete(),
            });
        }

        let mut union_ins_delta = ins_delta.transform_expand(&deletes_at_rev, true);
        let mut new_deletes = deletes.transform_expand(&deletes_at_rev);

        // rebase the delta to be on the head union instead of the base_rev union
        let new_full_priority = FullPriority { priority: new_priority, session_id: self.session };
        for r in &self.revs[ix + 1..] {
            if let Edit { priority, ref inserts, .. } = r.edit {
                if !inserts.is_empty() {
                    let full_priority =
                        FullPriority { priority, session_id: r.rev_id.session_id() };
                    let after = new_full_priority >= full_priority; // should never be ==
                    union_ins_delta = union_ins_delta.transform_expand(inserts, after);
                    new_deletes = new_deletes.transform_expand(inserts);
                }
            }
        }

        // rebase the deletion to be after the inserts instead of directly on the head union
        let new_inserts = union_ins_delta.inserted_subset();
        if !new_inserts.is_empty() {
            new_deletes = new_deletes.transform_expand(&new_inserts);
        }

        // rebase insertions on text and apply
        let cur_deletes_from_union = &self.deletes_from_union;
        let text_ins_delta = union_ins_delta.transform_shrink(cur_deletes_from_union);
        let text_with_inserts = text_ins_delta.apply(&self.text);
        let rebased_deletes_from_union = cur_deletes_from_union.transform_expand(&new_inserts);

        // is the new edit in an undo group that was already undone due to concurrency?
        let undone = self.undone_groups.contains(&undo_group);
        let new_deletes_from_union = {
            let to_delete = if undone { &new_inserts } else { &new_deletes };
            rebased_deletes_from_union.union(to_delete)
        };

        // move deleted or undone-inserted things from text to tombstones
        let (new_text, new_tombstones) = shuffle(
            &text_with_inserts,
            &self.tombstones,
            &rebased_deletes_from_union,
            &new_deletes_from_union,
        );

        let head_rev = &self.revs.last().unwrap();
        Ok((
            Revision {
                rev_id: self.next_rev_id(),
                max_undo_so_far: std::cmp::max(undo_group, head_rev.max_undo_so_far),
                edit: Edit {
                    priority: new_priority,
                    undo_group,
                    inserts: new_inserts,
                    deletes: new_deletes,
                },
            },
            new_text,
            new_tombstones,
            new_deletes_from_union,
        ))
    }
    // NOTE: maybe just deprecate this? we can panic on the other side of
    // the call if/when that makes sense.
    /// Create a new edit based on `base_rev`.
    ///
    /// # Panics
    ///
    /// Panics if `base_rev` does not exist, or if `delta` is poorly formed.
    pub fn edit_rev(
        &mut self,
        priority: usize,
        undo_group: usize,
        base_rev: RevToken,
        delta: Delta<RopeInfo>,
    ) {
        self.try_edit_rev(priority, undo_group, base_rev, delta).unwrap();
    }

    /// Attempts to apply a new edit based on the `Revision` specified by `base_rev`,
    /// Returning an [`Error`] if the `Revision` cannot be found.
    pub fn try_edit_rev(
        &mut self,
        priority: usize,
        undo_group: usize,
        base_rev: RevToken,
        delta: Delta<RopeInfo>,
    ) -> Result<(), Error> {
        let (new_rev, new_text, new_tombstones, new_deletes_from_union) =
            self.mk_new_rev(priority, undo_group, base_rev, delta)?;
        if matches!(
            new_rev.edit,
            Edit { ref inserts, ref deletes, .. } if inserts.is_empty() && deletes.is_empty()
        ) {
            return Ok(());
        }
        self.rev_id_counter += 1;
        self.revs.push(new_rev);
        self.text = new_text;
        self.tombstones = new_tombstones;
        self.deletes_from_union = new_deletes_from_union;
        Ok(())
    }
}
