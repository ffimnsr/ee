//! `impl Engine` lifecycle, revision lookup, and session identity.
use super::*;

impl Engine {
    pub fn new(initial_contents: Rope) -> Engine {
        let mut engine = Engine::empty();
        if !initial_contents.is_empty() {
            let first_rev = engine.get_head_rev_id().token();
            let delta = Delta::simple_edit(Interval::new(0, 0), initial_contents, 0);
            engine.edit_rev(0, 0, first_rev, delta);
        }
        engine
    }

    pub fn empty() -> Engine {
        let deletes_from_union = Subset::new(0);
        let rev = Revision {
            rev_id: RevId { session1: 0, session2: 0, num: 0 },
            edit: Undo {
                toggled_groups: OrdSet::new(),
                deletes_bitxor: deletes_from_union.clone(),
            },
            max_undo_so_far: 0,
        };
        Engine {
            session: default_session(),
            rev_id_counter: 1,
            text: Rope::default(),
            tombstones: Rope::default(),
            deletes_from_union,
            undone_groups: OrdSet::new(),
            revs: vec![rev],
        }
    }

    pub(super) fn next_rev_id(&self) -> RevId {
        RevId { session1: self.session.0, session2: self.session.1, num: self.rev_id_counter }
    }

    pub(super) fn find_rev(&self, rev_id: RevId) -> Option<usize> {
        self.revs.iter().enumerate().rev().find(|&(_, rev)| rev.rev_id == rev_id).map(|(i, _)| i)
    }

    pub(super) fn find_rev_token(&self, rev_token: RevToken) -> Option<usize> {
        self.revs
            .iter()
            .enumerate()
            .rev()
            .find(|&(_, rev)| rev.rev_id.token() == rev_token)
            .map(|(i, _)| i)
    }

    /// Find what the `deletes_from_union` field in Engine would have been at the time
    /// of a certain `rev_index`. In other words, the deletes from the union string at that time.
    pub(super) fn deletes_from_union_for_index(&self, rev_index: usize) -> Cow<'_, Subset> {
        self.deletes_from_union_before_index(rev_index + 1, true)
    }

    /// Garbage collection means undo can sometimes need to replay the very first
    /// revision, and so needs a way to get the deletion set before then.
    pub(super) fn deletes_from_union_before_index(
        &self,
        rev_index: usize,
        invert_undos: bool,
    ) -> Cow<'_, Subset> {
        let mut deletes_from_union = Cow::Borrowed(&self.deletes_from_union);
        let mut undone_groups = Cow::Borrowed(&self.undone_groups);

        // invert the changes to deletes_from_union starting in the present and working backwards
        for rev in self.revs[rev_index..].iter().rev() {
            deletes_from_union = match rev.edit {
                Edit { ref inserts, ref deletes, ref undo_group, .. } => {
                    if undone_groups.contains(undo_group) {
                        // no need to un-delete undone inserts since we'll just shrink them out
                        Cow::Owned(deletes_from_union.transform_shrink(inserts))
                    } else {
                        let un_deleted = deletes_from_union.subtract(deletes);
                        Cow::Owned(un_deleted.transform_shrink(inserts))
                    }
                }
                Undo { ref toggled_groups, ref deletes_bitxor } => {
                    if invert_undos {
                        let new_undone = undone_groups
                            .clone()
                            .into_owned()
                            .symmetric_difference(toggled_groups.clone());
                        undone_groups = Cow::Owned(new_undone);
                        Cow::Owned(deletes_from_union.bitxor(deletes_bitxor))
                    } else {
                        deletes_from_union
                    }
                }
            }
        }
        deletes_from_union
    }

    /// Get the contents of the document at a given revision number
    pub(super) fn rev_content_for_index(&self, rev_index: usize) -> Rope {
        let old_deletes_from_union = self.deletes_from_cur_union_for_index(rev_index);
        let delta =
            Delta::synthesize(&self.tombstones, &self.deletes_from_union, &old_deletes_from_union);
        delta.apply(&self.text)
    }

    /// Get the Subset to delete from the current union string in order to obtain a revision's content
    pub(super) fn deletes_from_cur_union_for_index(&self, rev_index: usize) -> Cow<'_, Subset> {
        let mut deletes_from_union = self.deletes_from_union_for_index(rev_index);
        for rev in &self.revs[rev_index + 1..] {
            if let Edit { ref inserts, .. } = rev.edit {
                if !inserts.is_empty() {
                    deletes_from_union = Cow::Owned(deletes_from_union.transform_union(inserts));
                }
            }
        }
        deletes_from_union
    }

    /// Returns the largest undo group ID used so far
    pub fn max_undo_group_id(&self) -> usize {
        self.revs.last().unwrap().max_undo_so_far
    }

    /// Get revision id of head revision.
    pub fn get_head_rev_id(&self) -> RevId {
        self.revs.last().unwrap().rev_id
    }

    /// Get text of head revision.
    pub fn get_head(&self) -> &Rope {
        &self.text
    }

    /// Get text of a given revision, if it can be found.
    pub fn get_rev(&self, rev: RevToken) -> Option<Rope> {
        self.find_rev_token(rev).map(|rev_index| self.rev_content_for_index(rev_index))
    }

    /// A delta that, when applied to `base_rev`, results in the current head. Returns
    /// an error if there is not at least one edit.
    pub fn try_delta_rev_head(&self, base_rev: RevToken) -> Result<Delta<RopeInfo>, Error> {
        let ix = self.find_rev_token(base_rev).ok_or(Error::MissingRevision(base_rev))?;
        let prev_from_union = self.deletes_from_cur_union_for_index(ix);
        let old_tombstones = shuffle_tombstones(
            &self.text,
            &self.tombstones,
            &self.deletes_from_union,
            &prev_from_union,
        );
        Ok(Delta::synthesize(&old_tombstones, &prev_from_union, &self.deletes_from_union))
    }
}

impl Engine {
    pub fn set_session_id(&mut self, session: SessionId) {
        assert_eq!(
            1,
            self.revs.len(),
            "Revisions were added to an Engine before set_session_id, these may collide."
        );
        self.session = session;
    }
}
