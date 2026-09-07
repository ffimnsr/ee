//! `impl Engine` garbage collection.
use super::*;

impl Engine {
    pub fn gc<I, T>(&mut self, gc_groups: I)
    where
        I: IntoIterator<Item = T>,
        T: Borrow<usize>,
    {
        let gc_groups: OrdSet<usize> = gc_groups.into_iter().map(|group| *group.borrow()).collect();
        let mut gc_dels = self.empty_subset_before_first_rev();
        let mut retain_revs = BTreeSet::new();
        if let Some(first) = self.revs.first() {
            retain_revs.insert(first.rev_id);
        }
        if let Some(last) = self.revs.last() {
            retain_revs.insert(last.rev_id);
        }
        {
            for rev in &self.revs {
                if let Edit { ref undo_group, ref inserts, ref deletes, .. } = rev.edit {
                    if !retain_revs.contains(&rev.rev_id) && gc_groups.contains(undo_group) {
                        if self.undone_groups.contains(undo_group) {
                            if !inserts.is_empty() {
                                gc_dels = gc_dels.transform_union(inserts);
                            }
                        } else {
                            if !inserts.is_empty() {
                                gc_dels = gc_dels.transform_expand(inserts);
                            }
                            if !deletes.is_empty() {
                                gc_dels = gc_dels.union(deletes);
                            }
                        }
                    } else if !inserts.is_empty() {
                        gc_dels = gc_dels.transform_expand(inserts);
                    }
                }
            }
        }
        if !gc_dels.is_empty() {
            let not_in_tombstones = self.deletes_from_union.complement();
            let dels_from_tombstones = gc_dels.transform_shrink(&not_in_tombstones);
            self.tombstones = dels_from_tombstones.delete_from(&self.tombstones);
            self.deletes_from_union = self.deletes_from_union.transform_shrink(&gc_dels);
        }
        let old_revs = std::mem::take(&mut self.revs);
        for rev in old_revs.into_iter().rev() {
            match rev.edit {
                Edit { priority, undo_group, inserts, deletes } => {
                    let new_gc_dels = if inserts.is_empty() {
                        None
                    } else {
                        Some(gc_dels.transform_shrink(&inserts))
                    };
                    let (inserts, deletes) = if gc_dels.is_empty() {
                        (inserts, deletes)
                    } else {
                        (inserts.transform_shrink(&gc_dels), deletes.transform_shrink(&gc_dels))
                    };
                    let retain_visible_insert =
                        gc_groups.contains(&undo_group) && !inserts.is_empty();
                    if retain_revs.contains(&rev.rev_id)
                        || !gc_groups.contains(&undo_group)
                        || retain_visible_insert
                    {
                        let drop_gc_only_delete =
                            gc_groups.contains(&undo_group) && inserts.is_empty();
                        if !drop_gc_only_delete && (!inserts.is_empty() || !deletes.is_empty()) {
                            self.revs.push(Revision {
                                rev_id: rev.rev_id,
                                max_undo_so_far: rev.max_undo_so_far,
                                edit: Edit { priority, undo_group, inserts, deletes },
                            });
                        }
                    }
                    if let Some(new_gc_dels) = new_gc_dels {
                        gc_dels = new_gc_dels;
                    }
                }
                Undo { toggled_groups, deletes_bitxor } => {
                    // We're super-aggressive about dropping these; after gc, the history
                    // of which undos were used to compute deletes_from_union in edits may be lost.
                    if retain_revs.contains(&rev.rev_id) {
                        let new_deletes_bitxor = if gc_dels.is_empty() {
                            deletes_bitxor
                        } else {
                            deletes_bitxor.transform_shrink(&gc_dels)
                        };
                        self.revs.push(Revision {
                            rev_id: rev.rev_id,
                            max_undo_so_far: rev.max_undo_so_far,
                            edit: Undo {
                                toggled_groups: toggled_groups
                                    .clone()
                                    .relative_complement(gc_groups.clone()),
                                deletes_bitxor: new_deletes_bitxor,
                            },
                        })
                    }
                }
            }
        }
        self.revs.reverse();
        if !self.revs.iter().any(|rev| matches!(rev.edit, Edit { .. })) {
            self.text = Rope::default();
            self.tombstones = Rope::default();
            self.deletes_from_union = Subset::new(0);
            self.undone_groups = OrdSet::new();
        }
    }
}
