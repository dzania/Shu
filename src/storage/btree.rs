use std::{mem, path::Path};

use crate::{
    error::{Result, ShuError},
    storage::{
        btree_page::{
            INTERNAL_ENTRIES_CAPACITY, InternalEntries, InternalEntry, InternalNodeMut,
            LEAF_ENTRIES_CAPACITY, LeafEntry, LeafInsert, LeafNode, LeafNodeMut, LeafSearchResult,
            Node, NodeKind, leaf_entry_space,
        },
        page::PageId,
        pager::Pager,
    },
};

/// Upper bound on root-to-leaf depth. Real trees stay far below it, since every split at least
/// doubles the number of pages per level; hitting it means the page graph has a cycle.
const TREE_DEPTH_MAX: usize = 64;

pub struct BTree {
    pager: Pager,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PathFrame {
    page_id: PageId,
    child_index: u16,
    /// The descent followed this page's right child.
    is_rightmost: bool,
}

/// Child pointers a split adds to the parent. `entries[0].child` is always the page that was
/// split. The entries go in at that page's child slot, and `right` takes over the slot that used
/// to point at the split page.
#[derive(Debug)]
struct Split {
    entries: Vec<InternalEntry>,
    right: PageId,
}

struct InternalGroup {
    entries: InternalEntries,
    /// `None` for the rightmost group.
    promoted_separator: Option<Vec<u8>>,
}

impl BTree {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            pager: Pager::open(path)?,
        })
    }

    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut page_id = self.pager.root_page_id();
        for _ in 0..TREE_DEPTH_MAX {
            match Node::from_page(self.pager.read_page(page_id)?)? {
                Node::Leaf(leaf) => {
                    return match leaf.search(key)? {
                        LeafSearchResult::Found(index) => {
                            Ok(Some(leaf.entry(index)?.value.to_vec()))
                        }
                        LeafSearchResult::Missing(_) => Ok(None),
                    };
                }
                Node::Internal(node) => {
                    page_id = node.child_at(node.child_index_for_key(key)?)?;
                }
            }
        }
        Err(ShuError::TreeTooDeep {
            depth_max: TREE_DEPTH_MAX,
        })
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let (leaf_id, mut path) = self.descend(key)?;
        if leaf_entry_space(key.len(), value.len()) > LEAF_ENTRIES_CAPACITY {
            return Err(ShuError::CellTooLarge {
                page_id: leaf_id,
                key_len: key.len(),
                value_len: value.len(),
            });
        }

        let mut leaf = LeafNodeMut::from_page(self.pager.get_mut(leaf_id)?)?;
        if leaf.insert(key, value)? == LeafInsert::Inserted {
            return Ok(());
        }

        // Inserts never shrink a page, so only overflow needs handling here. Splits propagate
        // upwards one level per iteration.
        let appending = path.iter().all(|frame| frame.is_rightmost);
        let mut split = self.split_leaf(leaf_id, key, value, appending)?;
        // One iteration per path frame plus the root split, and one more to observe completion.
        for _ in 0..=TREE_DEPTH_MAX + 1 {
            let Some(pending) = split else {
                return Ok(());
            };
            split = match path.pop() {
                Some(frame) => self.insert_into_parent(frame, pending, appending)?,
                None => self.grow_root(pending)?,
            };
        }
        Err(ShuError::TreeTooDeep {
            depth_max: TREE_DEPTH_MAX,
        })
    }

    pub fn sync(&mut self) -> Result<()> {
        self.pager.sync()
    }

    /// Walks from the root to the leaf that owns `key`, recording the path for splits.
    fn descend(&mut self, key: &[u8]) -> Result<(PageId, Vec<PathFrame>)> {
        let mut path = Vec::new();
        let mut page_id = self.pager.root_page_id();
        while path.len() < TREE_DEPTH_MAX {
            match Node::from_page(self.pager.read_page(page_id)?)? {
                Node::Leaf(_) => return Ok((page_id, path)),
                Node::Internal(node) => {
                    let child_index = node.child_index_for_key(key)?;
                    path.push(PathFrame {
                        page_id,
                        child_index,
                        is_rightmost: child_index == node.len(),
                    });
                    page_id = node.child_at(child_index)?;
                }
            }
        }
        Err(ShuError::TreeTooDeep {
            depth_max: TREE_DEPTH_MAX,
        })
    }

    /// Makes room for `key` in a leaf that reported `NoSpace`. Returns the split for the parent,
    /// or `None` when compacting the leaf was enough.
    fn split_leaf(
        &mut self,
        leaf_id: PageId,
        key: &[u8],
        value: &[u8],
        appending: bool,
    ) -> Result<Option<Split>> {
        let leaf = LeafNode::from_page(self.pager.read_page(leaf_id)?)?;

        // Appending past the end of the tree: leave the full leaf as is and start a new one, so
        // sequential inserts fill pages completely instead of leaving them half empty.
        if appending
            && let Some(last_key) = leaf.last_key()?
            && key > last_key
        {
            let separator = last_key.to_vec();
            let new_leaf_id = self.pager.allocate(NodeKind::Leaf)?;
            LeafNodeMut::from_page(self.pager.get_mut(new_leaf_id)?)?
                .rewrite(&[LeafEntry::new(key, value)])?;
            return Ok(Some(Split {
                entries: vec![InternalEntry::new(leaf_id, separator)],
                right: new_leaf_id,
            }));
        }

        let mut entries = leaf.entries()?;
        upsert_leaf_entry(&mut entries, key, value);
        let space: usize = entries.iter().map(LeafEntry::space).sum();
        if space <= LEAF_ENTRIES_CAPACITY {
            // The page only lacked contiguous space; a rewrite reclaims the holes.
            LeafNodeMut::from_page(self.pager.get_mut(leaf_id)?)?.rewrite(&entries)?;
            return Ok(None);
        }

        self.write_leaf_groups(leaf_id, split_leaf_entries(entries))
            .map(Some)
    }

    fn write_leaf_groups(&mut self, page_id: PageId, groups: Vec<Vec<LeafEntry>>) -> Result<Split> {
        assert!(groups.len() >= 2, "a split produces at least two pages");
        let last_index = groups.len() - 1;
        let mut entries = Vec::with_capacity(last_index);
        let mut child_id = page_id;

        for (index, mut group) in groups.into_iter().enumerate() {
            if index > 0 {
                child_id = self.pager.allocate(NodeKind::Leaf)?;
            }
            LeafNodeMut::from_page(self.pager.get_mut(child_id)?)?.rewrite(&group)?;
            if index < last_index {
                let last = group.pop().expect("split groups are never empty");
                entries.push(InternalEntry::new(child_id, last.key));
            }
        }

        Ok(Split {
            entries,
            right: child_id,
        })
    }

    /// Adds a child split to the parent at `frame`. Returns the parent's own split when it
    /// overflows.
    fn insert_into_parent(
        &mut self,
        frame: PathFrame,
        split: Split,
        appending: bool,
    ) -> Result<Option<Split>> {
        let needed: usize = split.entries.iter().map(InternalEntry::space).sum();
        let mut parent = InternalNodeMut::from_page(self.pager.get_mut(frame.page_id)?)?;

        if needed <= parent.free_space() {
            let mut child_index = frame.child_index;
            for entry in &split.entries {
                parent.insert(child_index, entry)?;
                child_index += 1;
            }
            parent.set_child(child_index, split.right)?;
            return Ok(None);
        }

        // Appending past the end of the tree: the parent keeps every entry and its right child
        // still points at the split page, so it stays untouched. Only the separator moves up and a
        // new rightmost sibling takes the new page.
        if appending && let [promoted] = split.entries.as_slice() {
            assert_eq!(
                promoted.child,
                parent.as_node().child_at(frame.child_index)?
            );
            let separator = promoted.separator.clone();
            let sibling_id = self.pager.allocate(NodeKind::Internal)?;
            InternalNodeMut::from_page(self.pager.get_mut(sibling_id)?)?
                .set_child(0, split.right)?;
            return Ok(Some(Split {
                entries: vec![InternalEntry::new(frame.page_id, separator)],
                right: sibling_id,
            }));
        }

        let mut internal = parent.as_node().entries()?;
        splice_split(&mut internal, frame.child_index, split)?;
        if internal.space() <= INTERNAL_ENTRIES_CAPACITY {
            parent.rewrite(&internal)?;
            return Ok(None);
        }

        self.write_internal_groups(frame.page_id, split_internal_entries(internal))
            .map(Some)
    }

    fn write_internal_groups(
        &mut self,
        page_id: PageId,
        groups: Vec<InternalGroup>,
    ) -> Result<Split> {
        assert!(groups.len() >= 2, "a split produces at least two pages");
        let mut entries = Vec::with_capacity(groups.len() - 1);
        let mut child_id = page_id;

        for (index, group) in groups.into_iter().enumerate() {
            if index > 0 {
                child_id = self.pager.allocate(NodeKind::Internal)?;
            }
            InternalNodeMut::from_page(self.pager.get_mut(child_id)?)?.rewrite(&group.entries)?;
            if let Some(separator) = group.promoted_separator {
                entries.push(InternalEntry::new(child_id, separator));
            }
        }

        Ok(Split {
            entries,
            right: child_id,
        })
    }

    /// The root keeps its page id, so a root split moves the root's contents to a new page and
    /// turns the root into an internal page above it.
    fn grow_root(&mut self, mut split: Split) -> Result<Option<Split>> {
        let root_id = self.pager.root_page_id();
        let first = split
            .entries
            .first_mut()
            .expect("a split adds at least one entry");
        assert_eq!(
            first.child, root_id,
            "only the root can split without a parent"
        );

        // Copying is fine here: it happens once per tree level ever added.
        let root = self.pager.read_page(root_id)?.clone();
        let moved_id = self.pager.allocate(Node::from_page(&root)?.kind())?;
        self.pager.get_mut(moved_id)?.copy_contents_from(&root);
        first.child = moved_id;

        InternalNodeMut::init(self.pager.get_mut(root_id)?)?.set_child(0, moved_id)?;
        let frame = PathFrame {
            page_id: root_id,
            child_index: 0,
            is_rightmost: true,
        };
        self.insert_into_parent(frame, split, false)
    }
}

fn upsert_leaf_entry(entries: &mut Vec<LeafEntry>, key: &[u8], value: &[u8]) {
    match entries.binary_search_by(|entry| entry.key.as_slice().cmp(key)) {
        Ok(index) => entries[index].value = value.to_vec(),
        Err(index) => entries.insert(index, LeafEntry::new(key, value)),
    }
}

/// Inserts a child split into owned parent entries. See `Split` for the slot layout.
fn splice_split(internal: &mut InternalEntries, child_index: u16, split: Split) -> Result<()> {
    let index = usize::from(child_index);
    if index > internal.entries.len() {
        return Err(ShuError::IndexOutOfRange);
    }

    let right_slot = index + split.entries.len();
    internal.entries.splice(index..index, split.entries);
    match internal.entries.get_mut(right_slot) {
        Some(entry) => entry.child = split.right,
        None => internal.right_child = split.right,
    }
    Ok(())
}

/// Splits entries that overflow one page into the fewest pages that hold them, with bytes
/// balanced between pages so the next inserts do not overflow straight away.
fn split_leaf_entries(entries: Vec<LeafEntry>) -> Vec<Vec<LeafEntry>> {
    let total: usize = entries.iter().map(LeafEntry::space).sum();
    assert!(
        total > LEAF_ENTRIES_CAPACITY,
        "only overflowing pages split"
    );
    let target = split_target(total, LEAF_ENTRIES_CAPACITY);

    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut current_size = 0;
    for entry in entries {
        let size = entry.space();
        if !current.is_empty()
            && (current_size >= target || current_size + size > LEAF_ENTRIES_CAPACITY)
        {
            groups.push(mem::take(&mut current));
            current_size = 0;
        }
        current_size += size;
        current.push(entry);
    }
    groups.push(current);
    groups
}

/// Internal counterpart of `split_leaf_entries`. The entry at each boundary does not stay in a
/// page: its child becomes the left group's right child and its separator moves to the parent.
fn split_internal_entries(entries: InternalEntries) -> Vec<InternalGroup> {
    let total = entries.space();
    assert!(
        total > INTERNAL_ENTRIES_CAPACITY,
        "only overflowing pages split"
    );
    let target = split_target(total, INTERNAL_ENTRIES_CAPACITY);

    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut current_size = 0;
    for entry in entries.entries {
        let size = entry.space();
        if !current.is_empty()
            && (current_size >= target || current_size + size > INTERNAL_ENTRIES_CAPACITY)
        {
            groups.push(InternalGroup {
                entries: InternalEntries::new(mem::take(&mut current), entry.child),
                promoted_separator: Some(entry.separator),
            });
            current_size = 0;
            continue;
        }
        current_size += size;
        current.push(entry);
    }
    groups.push(InternalGroup {
        entries: InternalEntries::new(current, entries.right_child),
        promoted_separator: None,
    });
    groups
}

/// Bytes per page when spreading `total` bytes over the fewest pages of `capacity` bytes.
fn split_target(total: usize, capacity: usize) -> usize {
    let page_count = total.div_ceil(capacity).max(2);
    total.div_ceil(page_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_tree() -> (tempfile::TempDir, BTree) {
        let dir = tempfile::tempdir().unwrap();
        let tree = BTree::open(&dir.path().join("test.db")).unwrap();
        (dir, tree)
    }

    /// Walks the whole tree, asserting key order, separator bounds and equal leaf depth. Returns
    /// every key in order.
    fn checked_keys(tree: &mut BTree) -> Vec<Vec<u8>> {
        struct Visit {
            page_id: PageId,
            depth: usize,
            lower: Option<Vec<u8>>,
            upper: Option<Vec<u8>>,
        }

        let mut keys: Vec<Vec<u8>> = Vec::new();
        let mut leaf_depth = None;
        let mut stack = vec![Visit {
            page_id: tree.pager.root_page_id(),
            depth: 0,
            lower: None,
            upper: None,
        }];

        while let Some(visit) = stack.pop() {
            assert!(visit.depth < TREE_DEPTH_MAX);
            let in_bounds = |key: &[u8]| {
                visit.lower.as_deref().is_none_or(|lower| key > lower)
                    && visit.upper.as_deref().is_none_or(|upper| key <= upper)
            };

            match Node::from_page(tree.pager.read_page(visit.page_id).unwrap()).unwrap() {
                Node::Leaf(leaf) => {
                    assert_eq!(*leaf_depth.get_or_insert(visit.depth), visit.depth);
                    for entry in leaf.entries().unwrap() {
                        assert!(in_bounds(&entry.key));
                        keys.push(entry.key);
                    }
                }
                Node::Internal(node) => {
                    let internal = node.entries().unwrap();
                    let mut lower = visit.lower.clone();
                    let mut children = Vec::with_capacity(internal.entries.len() + 1);
                    for entry in internal.entries {
                        assert!(in_bounds(&entry.separator));
                        children.push((entry.child, lower.clone(), Some(entry.separator.clone())));
                        lower = Some(entry.separator);
                    }
                    children.push((internal.right_child, lower, visit.upper.clone()));
                    // Push right to left so leaves come off the stack in key order.
                    for (page_id, lower, upper) in children.into_iter().rev() {
                        stack.push(Visit {
                            page_id,
                            depth: visit.depth + 1,
                            lower,
                            upper,
                        });
                    }
                }
            }
        }

        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
        keys
    }

    fn allocate_leaf_with_records(tree: &mut BTree, records: &[(&[u8], &[u8])]) -> PageId {
        let page_id = tree.pager.allocate(NodeKind::Leaf).unwrap();
        let mut leaf = LeafNodeMut::from_page(tree.pager.get_mut(page_id).unwrap()).unwrap();
        for &(key, value) in records {
            assert_eq!(leaf.insert(key, value).unwrap(), LeafInsert::Inserted);
        }
        page_id
    }

    fn tree_with_internal_root() -> (tempfile::TempDir, BTree) {
        let (dir, mut tree) = temp_tree();
        let left_leaf =
            allocate_leaf_with_records(&mut tree, &[(b"ant", b"left-ant"), (b"cat", b"left-cat")]);
        let middle_left_leaf = allocate_leaf_with_records(
            &mut tree,
            &[(b"cow", b"middle-cow"), (b"dog", b"middle-dog")],
        );
        let middle_right_leaf = allocate_leaf_with_records(
            &mut tree,
            &[(b"elk", b"right-elk"), (b"fox", b"right-fox")],
        );
        let right_leaf =
            allocate_leaf_with_records(&mut tree, &[(b"yak", b"tail-yak"), (b"zoo", b"tail-zoo")]);

        let root_page_id = tree.pager.root_page_id();
        let mut root = InternalNodeMut::init(tree.pager.get_mut(root_page_id).unwrap()).unwrap();
        root.rewrite(&InternalEntries::new(
            vec![
                InternalEntry::new(left_leaf, b"cat"),
                InternalEntry::new(middle_left_leaf, b"dog"),
                InternalEntry::new(middle_right_leaf, b"fox"),
            ],
            right_leaf,
        ))
        .unwrap();

        (dir, tree)
    }

    fn sequential_key(index: u32) -> [u8; 4] {
        index.to_be_bytes()
    }

    fn scattered_key(index: u32) -> [u8; 4] {
        index.wrapping_mul(0x9E37_79B9).to_be_bytes()
    }

    #[test]
    fn btree_get_descends_through_internal_root_to_find_keys() {
        let (_dir, mut tree) = tree_with_internal_root();

        assert_eq!(tree.get(b"ant").unwrap(), Some(b"left-ant".to_vec()));
        assert_eq!(tree.get(b"cat").unwrap(), Some(b"left-cat".to_vec()));
        assert_eq!(tree.get(b"cow").unwrap(), Some(b"middle-cow".to_vec()));
        assert_eq!(tree.get(b"dog").unwrap(), Some(b"middle-dog".to_vec()));
        assert_eq!(tree.get(b"elk").unwrap(), Some(b"right-elk".to_vec()));
        assert_eq!(tree.get(b"fox").unwrap(), Some(b"right-fox".to_vec()));
        assert_eq!(tree.get(b"yak").unwrap(), Some(b"tail-yak".to_vec()));
        assert_eq!(tree.get(b"zoo").unwrap(), Some(b"tail-zoo".to_vec()));
    }

    #[test]
    fn btree_get_returns_none_after_internal_descent() {
        let (_dir, mut tree) = tree_with_internal_root();

        assert_eq!(tree.get(b"ape").unwrap(), None);
        assert_eq!(tree.get(b"doe").unwrap(), None);
        assert_eq!(tree.get(b"gnu").unwrap(), None);
        assert_eq!(tree.get(b"zip").unwrap(), None);
    }

    #[test]
    fn btree_put_inserts_after_internal_descent() {
        let (_dir, mut tree) = tree_with_internal_root();

        tree.put(b"ape", b"left-ape").unwrap();
        tree.put(b"doe", b"middle-doe").unwrap();
        tree.put(b"fop", b"right-fop").unwrap();
        tree.put(b"zip", b"tail-zip").unwrap();

        assert_eq!(tree.get(b"ape").unwrap(), Some(b"left-ape".to_vec()));
        assert_eq!(tree.get(b"doe").unwrap(), Some(b"middle-doe".to_vec()));
        assert_eq!(tree.get(b"fop").unwrap(), Some(b"right-fop".to_vec()));
        assert_eq!(tree.get(b"zip").unwrap(), Some(b"tail-zip".to_vec()));
        assert_eq!(checked_keys(&mut tree).len(), 12);
    }

    #[test]
    fn btree_put_replaces_after_internal_descent() {
        let (_dir, mut tree) = tree_with_internal_root();

        tree.put(b"cat", b"left-cat-updated").unwrap();
        tree.put(b"dog", b"middle-dog-updated").unwrap();
        tree.put(b"fox", b"right-fox-updated").unwrap();
        tree.put(b"zoo", b"tail-zoo-updated").unwrap();

        assert_eq!(
            tree.get(b"cat").unwrap(),
            Some(b"left-cat-updated".to_vec())
        );
        assert_eq!(
            tree.get(b"dog").unwrap(),
            Some(b"middle-dog-updated".to_vec())
        );
        assert_eq!(
            tree.get(b"fox").unwrap(),
            Some(b"right-fox-updated".to_vec())
        );
        assert_eq!(
            tree.get(b"zoo").unwrap(),
            Some(b"tail-zoo-updated".to_vec())
        );
    }

    #[test]
    fn btree_put_get_round_trip_on_root_leaf() {
        let (_dir, mut tree) = temp_tree();

        tree.put(b"cat", b"meow").unwrap();

        assert_eq!(tree.get(b"cat").unwrap(), Some(b"meow".to_vec()));
        assert_eq!(tree.get(b"dog").unwrap(), None);
    }

    #[test]
    fn btree_get_on_empty_tree_returns_none() {
        let (_dir, mut tree) = temp_tree();

        assert_eq!(tree.get(b"cat").unwrap(), None);
    }

    #[test]
    fn btree_put_replaces_existing_root_leaf_value() {
        let (_dir, mut tree) = temp_tree();

        tree.put(b"cat", b"meow").unwrap();
        tree.put(b"cat", b"purr").unwrap();

        assert_eq!(tree.get(b"cat").unwrap(), Some(b"purr".to_vec()));
    }

    #[test]
    fn btree_put_splits_root_leaf_when_full() {
        let (_dir, mut tree) = temp_tree();

        for index in 0..70 {
            let key = format!("key-{index:03}");
            tree.put(key.as_bytes(), &[index as u8; 48]).unwrap();
        }

        for index in 0..70 {
            let key = format!("key-{index:03}");
            assert_eq!(
                tree.get(key.as_bytes()).unwrap(),
                Some(vec![index as u8; 48])
            );
        }
        assert_eq!(checked_keys(&mut tree).len(), 70);
    }

    #[test]
    fn btree_put_splits_internal_parent_when_full() {
        let (_dir, mut tree) = temp_tree();

        for index in 0..700 {
            let key = format!("key-{index:04}");
            tree.put(key.as_bytes(), &vec![index as u8; 900])
                .unwrap_or_else(|error| panic!("insert {key} failed: {error}"));
        }

        for index in 0..700 {
            let key = format!("key-{index:04}");
            assert_eq!(
                tree.get(key.as_bytes()).unwrap(),
                Some(vec![index as u8; 900])
            );
        }
        assert_eq!(checked_keys(&mut tree).len(), 700);
    }

    #[test]
    fn btree_scattered_inserts_keep_tree_valid() {
        let (_dir, mut tree) = temp_tree();

        for index in 0..20_000 {
            tree.put(&scattered_key(index), &index.to_le_bytes())
                .unwrap();
        }

        for index in 0..20_000 {
            assert_eq!(
                tree.get(&scattered_key(index)).unwrap(),
                Some(index.to_le_bytes().to_vec())
            );
        }
        assert_eq!(tree.get(&scattered_key(20_000)).unwrap(), None);
        assert_eq!(checked_keys(&mut tree).len(), 20_000);
    }

    #[test]
    fn btree_sequential_inserts_fill_leaves() {
        let (_dir, mut tree) = temp_tree();
        let entry_count: u32 = 20_000;
        let value = [7_u8; 32];

        for index in 0..entry_count {
            tree.put(&sequential_key(index), &value).unwrap();
        }

        // The append path leaves full pages behind. Splitting each page in half would need
        // about twice as many leaves.
        let entries_per_leaf = LEAF_ENTRIES_CAPACITY / leaf_entry_space(4, value.len());
        let full_leaf_count = usize::try_from(entry_count)
            .unwrap()
            .div_ceil(entries_per_leaf);
        let page_count = usize::try_from(tree.pager.page_count).unwrap();
        assert!(
            page_count <= full_leaf_count + full_leaf_count / 10,
            "{page_count} pages for {full_leaf_count} full leaves"
        );
        assert_eq!(checked_keys(&mut tree).len(), 20_000);
    }

    #[test]
    fn btree_value_updates_with_changing_sizes_reclaim_space() {
        let (_dir, mut tree) = temp_tree();

        for round in 0..50_u8 {
            for index in 0..200 {
                let len = usize::from(round % 7) * 10 + usize::try_from(index % 3).unwrap();
                tree.put(&sequential_key(index), &vec![round; len]).unwrap();
            }
        }

        for index in 0..200 {
            let len = usize::from(49_u8 % 7) * 10 + usize::try_from(index % 3).unwrap();
            assert_eq!(
                tree.get(&sequential_key(index)).unwrap(),
                Some(vec![49; len])
            );
        }
        assert_eq!(checked_keys(&mut tree).len(), 200);
    }

    #[test]
    fn btree_large_entries_split_into_three_pages() {
        let (_dir, mut tree) = temp_tree();
        let half = vec![1_u8; LEAF_ENTRIES_CAPACITY / 2 - 20];
        let large = vec![2_u8; LEAF_ENTRIES_CAPACITY - 20];

        tree.put(b"a", &half).unwrap();
        tree.put(b"c", &half).unwrap();
        tree.put(b"b", &large).unwrap();

        assert_eq!(tree.get(b"a").unwrap(), Some(half.clone()));
        assert_eq!(tree.get(b"b").unwrap(), Some(large));
        assert_eq!(tree.get(b"c").unwrap(), Some(half));
        assert_eq!(checked_keys(&mut tree), [b"a", b"b", b"c"]);
    }

    #[test]
    fn btree_put_rejects_entry_larger_than_a_page() {
        let (_dir, mut tree) = temp_tree();
        let value = vec![0_u8; LEAF_ENTRIES_CAPACITY];

        let result = tree.put(b"k", &value);

        assert!(matches!(result, Err(ShuError::CellTooLarge { .. })));
        assert_eq!(tree.get(b"k").unwrap(), None);
    }

    #[test]
    fn btree_reopen_after_sync_preserves_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        {
            let mut tree = BTree::open(&path).unwrap();
            for index in 0..5_000 {
                tree.put(&scattered_key(index), &index.to_le_bytes())
                    .unwrap();
            }
            tree.sync().unwrap();
        }

        let mut tree = BTree::open(&path).unwrap();
        for index in 0..5_000 {
            assert_eq!(
                tree.get(&scattered_key(index)).unwrap(),
                Some(index.to_le_bytes().to_vec())
            );
        }
        assert_eq!(checked_keys(&mut tree).len(), 5_000);
    }

    #[test]
    fn splice_split_replaces_entry_child_and_right_child_slots() {
        let base = InternalEntries::new(
            vec![InternalEntry::new(PageId::new(10), b"m")],
            PageId::new(11),
        );

        let mut middle = base.clone();
        splice_split(
            &mut middle,
            0,
            Split {
                entries: vec![InternalEntry::new(PageId::new(10), b"f")],
                right: PageId::new(20),
            },
        )
        .unwrap();

        let mut right = base.clone();
        splice_split(
            &mut right,
            1,
            Split {
                entries: vec![InternalEntry::new(PageId::new(11), b"t")],
                right: PageId::new(21),
            },
        )
        .unwrap();

        assert_eq!(
            middle,
            InternalEntries::new(
                vec![
                    InternalEntry::new(PageId::new(10), b"f"),
                    InternalEntry::new(PageId::new(20), b"m"),
                ],
                PageId::new(11),
            )
        );
        assert_eq!(
            right,
            InternalEntries::new(
                vec![
                    InternalEntry::new(PageId::new(10), b"m"),
                    InternalEntry::new(PageId::new(11), b"t"),
                ],
                PageId::new(21),
            )
        );
    }

    #[test]
    fn splice_split_rejects_slot_past_right_child() {
        let mut internal = InternalEntries::new(Vec::new(), PageId::new(11));

        let result = splice_split(
            &mut internal,
            1,
            Split {
                entries: vec![InternalEntry::new(PageId::new(11), b"t")],
                right: PageId::new(21),
            },
        );

        assert!(matches!(result, Err(ShuError::IndexOutOfRange)));
    }

    #[test]
    fn split_leaf_entries_balances_bytes_and_respects_capacity() {
        let entries: Vec<_> = (0..100_u32)
            .map(|index| LeafEntry::new(index.to_be_bytes(), [0_u8; 40]))
            .collect();
        let total: usize = entries.iter().map(LeafEntry::space).sum();

        let groups = split_leaf_entries(entries);

        assert_eq!(groups.len(), 2);
        for group in &groups {
            let size: usize = group.iter().map(LeafEntry::space).sum();
            assert!(size <= LEAF_ENTRIES_CAPACITY);
            assert!(size >= total / 2 - leaf_entry_space(4, 40));
        }
    }
}
