use std::path::Path;

use crate::{
    error::{Result, ShuError},
    storage::{
        btree_page::{
            BTreePage, InternalEntries, InternalEntry, LeafEntry, LeafSearchResult,
            init_internal_page, internal_entries_capacity, internal_entry_space,
            leaf_entries_capacity, leaf_entry_space,
        },
        header::INITIAL_ROOT_PAGE_ID,
        page::{Page, PageId, PageType},
        pager::Pager,
    },
};

pub struct BTree {
    pager: Pager,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PathFrame {
    page_id: PageId,
    child_index: u16,
}

struct TreeSearchResult {
    leaf_id: PageId,
    leaf_result: LeafSearchResult,
    path: Vec<PathFrame>,
}

#[derive(Debug)]
struct Sibling {
    page: PageId,
    // Index of the cell pointing to the page in parent
    index: u16,
}

struct LoadedSibling {
    page: Page,
    index: u16,
}

struct ParentChild {
    page_id: PageId,
    separator: Option<Vec<u8>>, // max key for this child
}

impl BTree {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            pager: Pager::open(path)?,
        })
    }

    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let search_result = self.search(key)?;
        match search_result.leaf_result {
            LeafSearchResult::Found(index) => {
                let leaf = self.pager.read_page(search_result.leaf_id)?;
                let entry = leaf.leaf_entry(index)?;
                Ok(Some(entry.value.to_owned()))
            }
            LeafSearchResult::Missing(_) => Ok(None),
        }
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let search_result = self.search(key)?;
        self.pager
            .get_mut(search_result.leaf_id)?
            .insert_leaf_entry(key, value)?;
        self.balance(search_result.leaf_id, search_result.path)?;
        Ok(())
    }

    fn balance(&mut self, page_id: PageId, mut path: Vec<PathFrame>) -> Result<()> {
        let page = self.pager.read_page(page_id)?;
        // Started from the bottom now we're here
        if page.page_type()? == PageType::Meta {
            return Ok(());
        };
        let is_root = page_id == INITIAL_ROOT_PAGE_ID;

        // The root is a special case because it can be fully empty.
        let is_underflow = !is_root && page.is_underflow()?;

        let is_overflow = page.is_overflow()?;
        // Nothing to do, the node is balanced.
        if !is_overflow && !is_underflow {
            return Ok(());
        }

        if is_root && is_underflow {
            if page.page_type()? == PageType::Leaf {
                return Ok(());
            };
            let entries = page.internal_entries()?;

            if entries.entries.is_empty() {
                let child = self.pager.read_page(entries.right_child)?;

                let root = self.pager.get_mut(page_id)?;
                root.set_page_type(child.page_type()?);
                root.copy_body_from(&child);
            }

            return Ok(());
        };

        if is_root && is_overflow {
            match page.page_type()? {
                // Database has only root page we have to create mutate root page
                // and create two new leaf pages
                PageType::Leaf => {
                    let entries = page.leaf_entries()?; // includes overflow
                    let groups = distribute_leaf_entries(entries);
                    let mut children = Vec::with_capacity(groups.len());

                    for group in groups {
                        let leaf_id = self.pager.allocate(PageType::Leaf)?.id();
                        self.pager.get_mut(leaf_id)?.rewrite_leaf_entries(&group)?;
                        let separator = group.last().unwrap().key.clone();
                        children.push((leaf_id, separator));
                    }

                    let right_child = children.last().unwrap().0;

                    let entries = children[..children.len() - 1]
                        .iter()
                        .map(|(child, separator)| InternalEntry::new(*child, separator.clone()))
                        .collect();

                    let internal_entries = InternalEntries::new(entries, right_child);
                    let root = self.pager.get_mut(page_id)?;
                    root.set_page_type(PageType::Internal);
                    root.rewrite_internal_entries(&internal_entries)?;
                }

                PageType::Internal => {
                    let entries = page.internal_entries()?; // includes overflow
                    let groups = distribute_internal_entries(entries);

                    let mut root_entries = Vec::with_capacity(groups.len() - 1);
                    let mut right_child = None;

                    for group in groups {
                        let child_id = self.pager.allocate(PageType::Internal)?.id();
                        self.pager
                            .get_mut(child_id)?
                            .rewrite_internal_entries(&group.entries)?;

                        match group.promoted_separator {
                            Some(separator) => {
                                root_entries.push(InternalEntry::new(child_id, separator));
                            }
                            None => {
                                right_child = Some(child_id);
                            }
                        }
                    }

                    let root_internal_entries =
                        InternalEntries::new(root_entries, right_child.unwrap());

                    let root = self.pager.get_mut(page_id)?;
                    init_internal_page(root)?;
                    root.rewrite_internal_entries(&root_internal_entries)?;
                }

                PageType::Meta => return Ok(()),
            }

            return Ok(());
        }

        let frame = path.pop().ok_or(ShuError::Rebalance)?;
        match page.page_type()? {
            PageType::Leaf => {
                self.redistribute_leaf_window(frame.page_id, page_id, frame.child_index)?;
            }
            PageType::Internal => {
                self.redistribute_internal_window(frame.page_id, page_id, frame.child_index)?
            }
            PageType::Meta => unreachable!(),
        }

        self.balance(frame.page_id, path)?;
        Ok(())
    }

    fn load_window(
        &mut self,
        page_id: PageId,
        parent: &Page,
        child_index: u16,
    ) -> Result<Vec<LoadedSibling>> {
        let mut window = self
            .load_siblings(page_id, parent)?
            .into_iter()
            .map(|sibling| {
                Ok(LoadedSibling {
                    index: sibling.index,
                    page: self.pager.read_page(sibling.page)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        window.push(LoadedSibling {
            index: child_index,
            page: self.pager.read_page(page_id)?,
        });

        window.sort_by_key(|child| child.index);
        Ok(window)
    }

    fn redistribute_leaf_window(
        &mut self,
        parent_id: PageId,
        page_id: PageId,
        child_index: u16,
    ) -> Result<()> {
        let parent = self.pager.read_page(parent_id)?;
        let window = self.load_window(page_id, &parent, child_index)?;
        let start = window.first().unwrap().index;
        let end = window.last().unwrap().index;

        let parent_child_count = parent.internal_entries()?.entries.len() + 1;
        let range_reaches_right_child = usize::from(end) == parent_child_count - 1;

        let mut entries = Vec::new();
        for child in &window {
            entries.extend(child.page.leaf_entries()?);
        }

        let groups = distribute_leaf_entries(entries);
        let mut replacements = Vec::with_capacity(groups.len());

        for (i, group) in groups.iter().enumerate() {
            let child_id = if i < window.len() {
                window[i].page.id()
            } else {
                self.pager.allocate(PageType::Leaf)?.id()
            };

            self.pager.get_mut(child_id)?.rewrite_leaf_entries(group)?;

            let is_last = i + 1 == groups.len();
            let separator = if is_last && range_reaches_right_child {
                None
            } else {
                Some(group.last().unwrap().key.clone())
            };

            replacements.push(ParentChild {
                page_id: child_id,
                separator,
            });
        }

        let parent = self.pager.get_mut(parent_id)?;
        rewrite_parent_child_range(parent, start, end, replacements)
    }

    fn redistribute_internal_window(
        &mut self,
        parent_id: PageId,
        page_id: PageId,
        child_index: u16,
    ) -> Result<()> {
        let parent = self.pager.read_page(parent_id)?;
        let mut window = self
            .load_siblings(page_id, &parent)?
            .into_iter()
            .map(|sibling| {
                Ok(LoadedSibling {
                    index: sibling.index,
                    page: self.pager.read_page(sibling.page)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        window.push(LoadedSibling {
            index: child_index,
            page: self.pager.read_page(page_id)?,
        });
        window.sort_by_key(|sibling| sibling.index);
        let start = window
            .first()
            .ok_or(ShuError::CorruptedPage {
                page_id: parent.id(),
            })?
            .index;

        let end = window
            .last()
            .ok_or(ShuError::CorruptedPage {
                page_id: parent.id(),
            })?
            .index;

        let internal = parent.internal_entries()?;

        let mut slots = internal
            .entries
            .into_iter()
            .map(|entry| ParentChild {
                page_id: entry.child,
                separator: Some(entry.separator),
            })
            .collect::<Vec<_>>();

        slots.push(ParentChild {
            page_id: internal.right_child,
            separator: None,
        });

        let old_end_separator = slots[usize::from(end)].separator.clone();
        let mut flat_entries = Vec::new();
        let mut final_right_child = None;

        for (i, child) in window.iter().enumerate() {
            let internal = child.page.internal_entries()?;
            flat_entries.extend(internal.entries);

            if i + 1 < window.len() {
                let divider = slots[usize::from(child.index)].separator.clone().ok_or(
                    ShuError::CorruptedPage {
                        page_id: parent.id(),
                    },
                )?;

                flat_entries.push(InternalEntry::new(internal.right_child, divider));
            } else {
                final_right_child = Some(internal.right_child);
            }
        }

        let groups = distribute_internal_entries(InternalEntries::new(
            flat_entries,
            final_right_child.unwrap(),
        ));

        let group_len = groups.len();
        let mut replacements = Vec::with_capacity(group_len);

        for (i, group) in groups.into_iter().enumerate() {
            let child_id = if i < window.len() {
                window[i].page.id()
            } else {
                self.pager.allocate(PageType::Internal)?.id()
            };

            self.pager
                .get_mut(child_id)?
                .rewrite_internal_entries(&group.entries)?;

            let is_last = i + 1 == group_len;
            let separator = if is_last {
                old_end_separator.clone()
            } else {
                group.promoted_separator
            };

            replacements.push(ParentChild {
                page_id: child_id,
                separator,
            });
        }

        let parent = self.pager.get_mut(parent_id)?;
        rewrite_parent_child_range(parent, start, end, replacements)
    }

    /// Given the parent page and page id returns sibling pages on left and right side
    fn load_siblings(&mut self, page_id: PageId, parent: &Page) -> Result<Vec<Sibling>> {
        let internal = parent.internal_entries()?;
        let child_count = internal.entries.len() + 1;

        let child_at = |index: usize| {
            if index < internal.entries.len() {
                internal.entries[index].child
            } else {
                internal.right_child
            }
        };

        let child_index = (0..child_count)
            .find(|&index| child_at(index) == page_id)
            .ok_or(ShuError::CorruptedPage {
                page_id: parent.id(),
            })?;

        Ok([
            child_index.checked_sub(1),
            (child_index + 1 < child_count).then_some(child_index + 1),
        ]
        .into_iter()
        .flatten()
        .map(|index| Sibling {
            index: index as u16,
            page: child_at(index),
        })
        .collect())
    }

    pub fn sync(&mut self) -> Result<()> {
        self.pager.sync()
    }

    fn search(&mut self, new_key: &[u8]) -> Result<TreeSearchResult> {
        let mut path = Vec::new();
        let mut page = self.pager.read_page(self.pager.root_page_id())?;
        loop {
            match page.page_type()? {
                PageType::Leaf => {
                    let leaf_result = page.leaf_search(new_key)?;
                    return Ok(TreeSearchResult {
                        leaf_id: page.id(),
                        leaf_result,
                        path,
                    });
                }
                PageType::Internal => {
                    let child_index = page.child_index_for_key(new_key)?;
                    let page_id = page.child_at(child_index)?;
                    path.push(PathFrame {
                        page_id: page.id(),
                        child_index,
                    });
                    page = self.pager.read_page(page_id)?;
                }
                PageType::Meta => return Err(ShuError::InvalidPageType),
            }
        }
    }
}

/// Creates N number of groups to fit pages
fn distribute_leaf_entries(entries: Vec<LeafEntry>) -> Vec<Vec<LeafEntry>> {
    let free_space = leaf_entries_capacity();
    let mut group_lengths = Vec::new();
    let mut current_len = 0;
    let mut current_size = 0;

    for entry in &entries {
        let size = leaf_entry_space(entry);

        if current_size + size > free_space && current_len > 0 {
            group_lengths.push(current_len);
            current_len = 0;
            current_size = 0;
        }

        current_size += size;
        current_len += 1;
    }

    if current_len > 0 {
        group_lengths.push(current_len);
    }

    let mut groups = Vec::with_capacity(group_lengths.len());
    let mut entries = entries.into_iter();

    for len in group_lengths {
        let mut group = Vec::with_capacity(len);
        group.extend(entries.by_ref().take(len));
        groups.push(group);
    }

    groups
}

struct InternalEntryGroup {
    entries: InternalEntries,
    promoted_separator: Option<Vec<u8>>, // None for the rightmost group
}

fn distribute_internal_entries(entries: InternalEntries) -> Vec<InternalEntryGroup> {
    let free_space = internal_entries_capacity();

    let mut group_lengths = Vec::new();
    let mut current_len = 0;
    let mut current_size = 0;
    let mut index = 0;

    while index < entries.entries.len() {
        let size = internal_entry_space(&entries.entries[index]);

        if current_size + size > free_space && current_len > 0 {
            group_lengths.push(current_len);

            // This entry becomes the boundary:
            // - child becomes this group's right_child
            // - separator is promoted to the parent
            index += 1;

            current_len = 0;
            current_size = 0;
            continue;
        }

        current_size += size;
        current_len += 1;
        index += 1;
    }

    group_lengths.push(current_len);

    let rightmost_child = entries.right_child;
    let last_group_index = group_lengths.len() - 1;
    let mut entries = entries.entries.into_iter();
    let mut groups = Vec::with_capacity(group_lengths.len());

    for (group_index, len) in group_lengths.into_iter().enumerate() {
        let mut group_entries = Vec::with_capacity(len);
        group_entries.extend(entries.by_ref().take(len));

        let (right_child, promoted_separator) = if group_index == last_group_index {
            (rightmost_child, None)
        } else {
            let promoted = entries.next().unwrap();
            (promoted.child, Some(promoted.separator))
        };

        groups.push(InternalEntryGroup {
            entries: InternalEntries::new(group_entries, right_child),
            promoted_separator,
        });
    }

    groups
}

fn rewrite_parent_child_range(
    parent: &mut Page,
    start_index: u16,
    end_index: u16,
    replacements: Vec<ParentChild>,
) -> Result<()> {
    let internal = parent.internal_entries()?;
    let mut slots = internal
        .entries
        .into_iter()
        .map(|entry| ParentChild {
            page_id: entry.child,
            separator: Some(entry.separator),
        })
        .collect::<Vec<_>>();
    slots.push(ParentChild {
        page_id: internal.right_child,
        separator: None,
    });
    slots.splice(
        usize::from(start_index)..=usize::from(end_index),
        replacements,
    );
    let right_child = slots
        .last()
        .ok_or(ShuError::CorruptedPage {
            page_id: parent.id(),
        })?
        .page_id;

    let entries = slots[..slots.len() - 1]
        .iter()
        .map(|slot| {
            let separator = slot.separator.clone().ok_or(ShuError::CorruptedPage {
                page_id: parent.id(),
            })?;

            Ok(InternalEntry::new(slot.page_id, separator))
        })
        .collect::<Result<Vec<_>>>()?;
    parent.rewrite_internal_entries(&InternalEntries::new(entries, right_child))
}

#[cfg(test)]
fn find_child_for_key(page: &Page, key: &[u8]) -> Result<PageId> {
    let child_index = page.child_index_for_key(key)?;
    page.child_at(child_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::btree_page::init_leaf_page;

    #[test]
    fn leaf_entry_rejects_index_past_record_count() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();

        let result = page.leaf_entry(0);

        assert!(matches!(result, Err(ShuError::IndexOutOfRange)));
    }

    #[test]
    fn init_internal_page_writes_empty_entries() {
        let mut page = Page::new(PageType::Internal, PageId::new(2));

        init_internal_page(&mut page).unwrap();

        let entries = page.internal_entries().unwrap();
        assert!(entries.entries.is_empty());
        assert_eq!(entries.right_child, PageId::new(0));
    }

    #[test]
    fn leaf_page_underflow_tracks_half_full_occupancy() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();

        assert!(page.is_underflow().unwrap());

        page.insert_leaf_entry(b"k", &vec![0; 2200]).unwrap();

        assert!(!page.is_underflow().unwrap());
    }

    #[test]
    fn internal_page_underflow_tracks_half_full_occupancy() {
        let mut page = Page::new(PageType::Internal, PageId::new(2));
        init_internal_page(&mut page).unwrap();

        assert!(page.is_underflow().unwrap());

        page.append_internal_entry(&InternalEntry::new(PageId::new(7), vec![0; 2200]))
            .unwrap();

        assert!(!page.is_underflow().unwrap());
    }

    #[test]
    fn append_internal_entry_writes_child_and_separator_key() {
        let mut page = Page::new(PageType::Internal, PageId::new(2));
        init_internal_page(&mut page).unwrap();

        page.append_internal_entry(&InternalEntry::new(PageId::new(7), b"cat"))
            .unwrap();
        page.append_internal_entry(&InternalEntry::new(PageId::new(8), b"dog"))
            .unwrap();

        let entries = page.internal_entries().unwrap();
        assert_eq!(entries.entries.len(), 2);

        let first = page.internal_entry(0).unwrap();
        assert_eq!(first.child, PageId::new(7));
        assert_eq!(first.separator, b"cat");

        let second = page.internal_entry(1).unwrap();
        assert_eq!(second.child, PageId::new(8));
        assert_eq!(second.separator, b"dog");
    }

    #[test]
    fn append_internal_entry_supports_empty_separator_key_at_body_tail() {
        let mut page = Page::new(PageType::Internal, PageId::new(2));
        init_internal_page(&mut page).unwrap();

        page.append_internal_entry(&InternalEntry::new(PageId::new(7), b""))
            .unwrap();

        let entry = page.internal_entry(0).unwrap();
        assert_eq!(entry.child, PageId::new(7));
        assert_eq!(entry.separator, b"");
    }

    #[test]
    fn rewrite_internal_entries_rewrites_entries_and_right_child() {
        let mut page = Page::new(PageType::Internal, PageId::new(2));
        init_internal_page(&mut page).unwrap();
        page.append_internal_entry(&InternalEntry::new(PageId::new(99), b"stale"))
            .unwrap();

        let entries = InternalEntries::new(
            vec![
                InternalEntry::new(PageId::new(10), b"cat"),
                InternalEntry::new(PageId::new(11), b"dog"),
            ],
            PageId::new(12),
        );

        page.rewrite_internal_entries(&entries).unwrap();

        let rewritten = page.internal_entries().unwrap();
        assert_eq!(rewritten.entries.len(), 2);
        assert_eq!(rewritten.right_child, PageId::new(12));

        let first = page.internal_entry(0).unwrap();
        assert_eq!(first.child, PageId::new(10));
        assert_eq!(first.separator, b"cat");

        let second = page.internal_entry(1).unwrap();
        assert_eq!(second.child, PageId::new(11));
        assert_eq!(second.separator, b"dog");
    }

    #[test]
    fn insert_internal_entry_rejects_index_past_record_count() {
        let mut page = Page::new(PageType::Internal, PageId::new(2));
        init_internal_page(&mut page).unwrap();

        let result = page.insert_internal_entry(1, &InternalEntry::new(PageId::new(10), b"cat"));

        assert!(matches!(result, Err(ShuError::IndexOutOfRange)));
    }

    #[test]
    fn find_child_for_key_routes_internal_boundaries() {
        let mut page = Page::new(PageType::Internal, PageId::new(2));
        init_internal_page(&mut page).unwrap();
        page.append_internal_entry(&InternalEntry::new(PageId::new(10), b"cat"))
            .unwrap();
        page.append_internal_entry(&InternalEntry::new(PageId::new(11), b"dog"))
            .unwrap();
        page.append_internal_entry(&InternalEntry::new(PageId::new(12), b"fox"))
            .unwrap();
        page.set_right_child(PageId::new(13)).unwrap();

        assert_eq!(find_child_for_key(&page, b"ant").unwrap(), PageId::new(10));
        assert_eq!(find_child_for_key(&page, b"cat").unwrap(), PageId::new(10));
        assert_eq!(find_child_for_key(&page, b"cow").unwrap(), PageId::new(11));
        assert_eq!(find_child_for_key(&page, b"dog").unwrap(), PageId::new(11));
        assert_eq!(find_child_for_key(&page, b"elk").unwrap(), PageId::new(12));
        assert_eq!(find_child_for_key(&page, b"fox").unwrap(), PageId::new(12));
        assert_eq!(find_child_for_key(&page, b"zoo").unwrap(), PageId::new(13));
    }

    #[test]
    fn find_child_for_key_uses_right_child_for_empty_internal_page() {
        let mut page = Page::new(PageType::Internal, PageId::new(2));
        init_internal_page(&mut page).unwrap();

        page.set_right_child(PageId::new(9)).unwrap();

        assert_eq!(find_child_for_key(&page, b"cat").unwrap(), PageId::new(9));
    }

    #[test]
    fn insert_leaf_entry_appends_two_entries() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();

        page.insert_leaf_entry(b"a", b"first").unwrap();
        page.insert_leaf_entry(b"b", b"second").unwrap();

        let entries = page.leaf_entries().unwrap();
        assert_eq!(entries.len(), 2);

        let first = page.leaf_entry(0).unwrap();
        assert_eq!(first.key, b"a");
        assert_eq!(first.value, b"first");

        let second = page.leaf_entry(1).unwrap();
        assert_eq!(second.key, b"b");
        assert_eq!(second.value, b"second");
    }

    #[test]
    fn insert_leaf_entry_keeps_entries_sorted_by_key() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();

        page.insert_leaf_entry(b"z", b"last").unwrap();
        page.insert_leaf_entry(b"a", b"first").unwrap();

        let entries = page.leaf_entries().unwrap();
        assert_eq!(entries.len(), 2);

        let first = page.leaf_entry(0).unwrap();
        assert_eq!(first.key, b"a");
        assert_eq!(first.value, b"first");

        let second = page.leaf_entry(1).unwrap();
        assert_eq!(second.key, b"z");
        assert_eq!(second.value, b"last");
    }

    #[test]
    fn search_leaf_finds_existing_keys() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();

        page.insert_leaf_entry(b"a", b"first").unwrap();
        page.insert_leaf_entry(b"c", b"middle").unwrap();
        page.insert_leaf_entry(b"z", b"last").unwrap();

        assert_eq!(page.leaf_search(b"a").unwrap(), LeafSearchResult::Found(0));
        assert_eq!(page.leaf_search(b"c").unwrap(), LeafSearchResult::Found(1));
        assert_eq!(page.leaf_search(b"z").unwrap(), LeafSearchResult::Found(2));
    }

    #[test]
    fn search_leaf_returns_missing_insert_positions() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();

        page.insert_leaf_entry(b"a", b"first").unwrap();
        page.insert_leaf_entry(b"c", b"middle").unwrap();
        page.insert_leaf_entry(b"z", b"last").unwrap();

        assert_eq!(
            page.leaf_search(b"0").unwrap(),
            LeafSearchResult::Missing(0)
        );
        assert_eq!(
            page.leaf_search(b"b").unwrap(),
            LeafSearchResult::Missing(1)
        );
        assert_eq!(
            page.leaf_search(b"d").unwrap(),
            LeafSearchResult::Missing(2)
        );
        assert_eq!(
            page.leaf_search(b"zz").unwrap(),
            LeafSearchResult::Missing(3)
        );
    }

    fn allocate_leaf_with_records(tree: &mut BTree, records: &[(&[u8], &[u8])]) -> PageId {
        let page_id = tree.pager.allocate(PageType::Leaf).unwrap().id();
        let page = tree.pager.get_mut(page_id).unwrap();
        for &(key, value) in records {
            page.insert_leaf_entry(key, value).unwrap();
        }
        page_id
    }

    fn tree_with_internal_root() -> BTree {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut tree = BTree::open(&path).unwrap();
        let left_leaf = allocate_leaf_with_records(
            &mut tree,
            &[
                (&b"ant"[..], &b"left-ant"[..]),
                (&b"cat"[..], &b"left-cat"[..]),
            ],
        );
        let middle_left_leaf = allocate_leaf_with_records(
            &mut tree,
            &[
                (&b"cow"[..], &b"middle-cow"[..]),
                (&b"dog"[..], &b"middle-dog"[..]),
            ],
        );
        let middle_right_leaf = allocate_leaf_with_records(
            &mut tree,
            &[
                (&b"elk"[..], &b"right-elk"[..]),
                (&b"fox"[..], &b"right-fox"[..]),
            ],
        );
        let right_leaf = allocate_leaf_with_records(
            &mut tree,
            &[
                (&b"yak"[..], &b"tail-yak"[..]),
                (&b"zoo"[..], &b"tail-zoo"[..]),
            ],
        );

        let root_page_id = tree.pager.root_page_id();
        let root = tree.pager.get_mut(root_page_id).unwrap();
        root.set_page_type(PageType::Internal);
        init_internal_page(root).unwrap();
        root.append_internal_entry(&InternalEntry::new(left_leaf, b"cat"))
            .unwrap();
        root.append_internal_entry(&InternalEntry::new(middle_left_leaf, b"dog"))
            .unwrap();
        root.append_internal_entry(&InternalEntry::new(middle_right_leaf, b"fox"))
            .unwrap();
        root.set_right_child(right_leaf).unwrap();

        tree
    }

    #[test]
    fn btree_get_descends_through_internal_root_to_find_keys() {
        let mut tree = tree_with_internal_root();

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
        let mut tree = tree_with_internal_root();

        assert_eq!(tree.get(b"ape").unwrap(), None);
        assert_eq!(tree.get(b"doe").unwrap(), None);
        assert_eq!(tree.get(b"gnu").unwrap(), None);
        assert_eq!(tree.get(b"zip").unwrap(), None);
    }

    #[test]
    fn btree_put_inserts_after_internal_descent() {
        let mut tree = tree_with_internal_root();

        tree.put(b"ape", b"left-ape").unwrap();
        tree.put(b"doe", b"middle-doe").unwrap();
        tree.put(b"fop", b"right-fop").unwrap();
        tree.put(b"zip", b"tail-zip").unwrap();

        assert_eq!(tree.get(b"ape").unwrap(), Some(b"left-ape".to_vec()));
        assert_eq!(tree.get(b"doe").unwrap(), Some(b"middle-doe".to_vec()));
        assert_eq!(tree.get(b"fop").unwrap(), Some(b"right-fop".to_vec()));
        assert_eq!(tree.get(b"zip").unwrap(), Some(b"tail-zip".to_vec()));
    }

    #[test]
    fn btree_put_replaces_after_internal_descent() {
        let mut tree = tree_with_internal_root();

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
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut tree = BTree::open(&path).unwrap();
        tree.put(b"cat", b"meow").unwrap();

        assert_eq!(tree.get(b"cat").unwrap(), Some(b"meow".to_vec()));
        assert_eq!(tree.get(b"dog").unwrap(), None);
    }

    #[test]
    fn btree_put_replaces_existing_root_leaf_value() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut tree = BTree::open(&path).unwrap();
        tree.put(b"cat", b"meow").unwrap();
        tree.put(b"cat", b"purr").unwrap();

        assert_eq!(tree.get(b"cat").unwrap(), Some(b"purr".to_vec()));
    }

    #[test]
    fn btree_put_splits_root_leaf_when_full() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut tree = BTree::open(&path).unwrap();

        for index in 0..70 {
            let key = format!("key-{index:03}");
            let value = vec![index as u8; 48];
            tree.put(key.as_bytes(), &value).unwrap();
        }

        for index in 0..70 {
            let key = format!("key-{index:03}");
            let value = vec![index as u8; 48];
            assert_eq!(tree.get(key.as_bytes()).unwrap(), Some(value));
        }
    }

    #[test]
    fn btree_put_splits_internal_parent_when_full() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut tree = BTree::open(&path).unwrap();

        for index in 0..700 {
            let key = format!("key-{index:04}");
            let value = vec![index as u8; 900];
            tree.put(key.as_bytes(), &value)
                .unwrap_or_else(|error| panic!("insert {key} failed: {error}"));
        }

        for index in 0..700 {
            let key = format!("key-{index:04}");
            let value = vec![index as u8; 900];
            assert_eq!(tree.get(key.as_bytes()).unwrap(), Some(value));
        }
    }
}
