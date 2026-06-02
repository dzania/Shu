use std::{mem::size_of, path::Path};

use crate::{
    error::{Result, ShuError},
    storage::{
        page::{Page, PageId, PageType},
        pager::Pager,
    },
};

const CELL_POINTER_SIZE: usize = size_of::<u16>();
const LEAF_CELL_HEADER_SIZE: usize = 2 * size_of::<u16>();
const INTERNAL_CELL_HEADER_SIZE: usize = size_of::<PageId>() + size_of::<u16>();

pub struct BTree {
    pager: Pager,
}

pub struct LeafCell<'a> {
    key: &'a [u8],
    value: &'a [u8],
}

struct OwnedLeafCell {
    key: Vec<u8>,
    value: Vec<u8>,
}

/// Borrowed internal-page cell.
///
/// The cell stores the child page to follow for keys up to the separator `key`.
pub struct InternalCell<'a> {
    pub child_page_id: PageId,
    pub key: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LeafSearchResult {
    Found(u16),
    Missing(u16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PathFrame {
    page_id: PageId,
    child_index: u16,
}

struct TreeSearchResult {
    leaf: Page,
    leaf_result: LeafSearchResult,
    path: Vec<PathFrame>,
}

#[derive(Clone, Copy, Debug)]
struct LeafInsertPosition {
    index: u16,
    is_new_key: bool,
}

impl LeafInsertPosition {
    fn from_search(result: LeafSearchResult) -> Self {
        match result {
            LeafSearchResult::Found(index) => Self {
                index,
                is_new_key: false,
            },
            LeafSearchResult::Missing(index) => Self {
                index,
                is_new_key: true,
            },
        }
    }

    fn pointer_offset(self) -> usize {
        leaf_cell_pointer_offset(self.index)
    }

    fn pointer_space_needed(self) -> usize {
        if self.is_new_key {
            CELL_POINTER_SIZE
        } else {
            0
        }
    }

    fn needs_pointer_shift(self, record_count: u16) -> bool {
        self.is_new_key && self.index != record_count
    }
}

#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct LeafPageHeader {
    record_count: u16,
    record_content_start: u16,
    right_sibling: PageId,
    left_sibling: PageId,
}

#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct InternalPageHeader {
    record_count: u16,
    record_content_start: u16,
    right_child: PageId,
}

#[derive(Clone, Copy, Debug)]
struct LeafCellLayout {
    cell_start: usize,
    key_len_start: usize,
    value_len_start: usize,
    key_start: usize,
    key_end: usize,
    value_start: usize,
    value_end: usize,
}

impl LeafCellLayout {
    fn new(cell_start: usize, key_len: u16, value_len: u16) -> Self {
        let key_len_start = cell_start;
        let value_len_start = key_len_start + size_of::<u16>();
        let key_start = value_len_start + size_of::<u16>();
        let key_end = key_start + usize::from(key_len);
        let value_start = key_end;
        let value_end = value_start + usize::from(value_len);

        Self {
            cell_start,
            key_len_start,
            value_len_start,
            key_start,
            key_end,
            value_start,
            value_end,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct InternalCellLayout {
    cell_start: usize,
    child_page_id_start: usize,
    key_len_start: usize,
    key_start: usize,
    key_end: usize,
}

impl InternalCellLayout {
    fn new(cell_start: usize, key_len: u16) -> Self {
        let child_page_id_start = cell_start;
        let key_len_start = child_page_id_start + size_of::<PageId>();
        let key_start = key_len_start + size_of::<u16>();
        let key_end = key_start + usize::from(key_len);

        Self {
            cell_start,
            child_page_id_start,
            key_len_start,
            key_start,
            key_end,
        }
    }
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
                let cell = read_leaf_cell(&search_result.leaf, index)?;
                Ok(Some(cell.value.to_owned()))
            }
            LeafSearchResult::Missing(_) => Ok(None),
        }
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let search_result = self.search(key)?;
        let mut page = search_result.leaf;
        match insert_into_leaf(&mut page, key, value) {
            Ok(()) => self.pager.write_page(page.id(), &page),
            Err(error @ ShuError::PageFull { .. }) => {
                self.balance(page, search_result.path, key, value, error)
            }
            Err(error) => Err(error),
        }
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
                    let leaf_result = search_leaf(&page, new_key)?;
                    return Ok(TreeSearchResult {
                        leaf: page,
                        leaf_result,
                        path,
                    });
                }
                PageType::Internal => {
                    let child_index = find_child_index_for_key(&page, new_key)?;
                    let page_id = child_page_id_at_index(&page, child_index)?;
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

    fn balance(
        &mut self,
        mut page: Page,
        path: Vec<PathFrame>,
        key: &[u8],
        value: &[u8],
        overflow_error: ShuError,
    ) -> Result<()> {
        if page.id() == self.pager.root_page_id() {
            let root_page_id = page.id();
            let child_page_id = self.balance_root(&mut page)?;
            let child = self.pager.read_page(child_page_id)?;
            let path = [PathFrame {
                page_id: root_page_id,
                child_index: 0,
            }];
            return self.balance_non_root(child, &path, key, value, overflow_error);
        }

        self.balance_non_root(page, &path, key, value, overflow_error)
    }

    fn balance_root(&mut self, root: &mut Page) -> Result<PageId> {
        let new_page_type = root.page_type()?;
        let child_page_id = self.pager.allocate(new_page_type)?;
        let mut child = self.pager.read_page(child_page_id)?;

        child.body_mut().copy_from_slice(root.body());
        root.header_mut().page_type = PageType::Internal as u8;
        let root_header = InternalPageHeader {
            record_count: 0,
            record_content_start: root.body().len() as u16,
            right_child: child_page_id,
        };
        write_internal_header(root, &root_header)?;
        self.pager.write_page(child.id(), &child)?;
        self.pager.write_page(root.id(), root)?;
        Ok(child_page_id)
    }

    fn balance_non_root(
        &mut self,
        page: Page,
        path: &[PathFrame],
        key: &[u8],
        value: &[u8],
        overflow_error: ShuError,
    ) -> Result<()> {
        let parent_frame = path
            .last()
            .ok_or(ShuError::CorruptedPage { page_id: page.id() })?;
        let parent = self.pager.read_page(parent_frame.page_id)?;

        match page.page_type()? {
            PageType::Leaf => self.split_rightmost_leaf_child(
                parent,
                parent_frame.child_index,
                page,
                key,
                value,
                overflow_error,
            ),
            PageType::Internal | PageType::Meta => Err(overflow_error),
        }
    }

    fn split_rightmost_leaf_child(
        &mut self,
        mut parent: Page,
        child_index: u16,
        page: Page,
        key: &[u8],
        value: &[u8],
        overflow_error: ShuError,
    ) -> Result<()> {
        let parent_header = read_internal_header(&parent)?;
        if child_index != parent_header.record_count {
            return Err(overflow_error);
        }

        let old_leaf_header = read_leaf_header(&page)?;
        let mut records = collect_leaf_records(&page)?;
        upsert_leaf_record(&mut records, key, value);
        if records.len() < 2 {
            return Err(overflow_error);
        }

        let split_index = records.len() / 2;
        let left_records = &records[..split_index];
        let right_records = &records[split_index..];
        let separator_key = left_records
            .last()
            .ok_or(ShuError::CorruptedPage { page_id: page.id() })?
            .key
            .as_slice();
        let separator_key_len = checked_internal_key_len(parent.id(), separator_key.len())?;
        allocate_internal_cell_start(parent.id(), &parent_header, separator_key_len)?;

        let mut left = Page::new(PageType::Leaf, page.id());
        init_leaf_page(&mut left)?;
        write_leaf_records(&mut left, left_records)?;

        let right_page_id = self.pager.allocate(PageType::Leaf)?;
        let mut right = self.pager.read_page(right_page_id)?;
        write_leaf_records(&mut right, right_records)?;

        let mut left_header = read_leaf_header(&left)?;
        left_header.left_sibling = old_leaf_header.left_sibling;
        left_header.right_sibling = right_page_id;
        left.write_body_prefix(&left_header)?;

        let mut right_header = read_leaf_header(&right)?;
        right_header.left_sibling = left.id();
        right_header.right_sibling = old_leaf_header.right_sibling;
        right.write_body_prefix(&right_header)?;

        if old_leaf_header.right_sibling != PageId::new(0) {
            let mut old_right = self.pager.read_page(old_leaf_header.right_sibling)?;
            let mut old_right_header = read_leaf_header(&old_right)?;
            old_right_header.left_sibling = right_page_id;
            old_right.write_body_prefix(&old_right_header)?;
            self.pager.write_page(old_right.id(), &old_right)?;
        }

        append_internal_cell(&mut parent, left.id(), separator_key)?;
        let mut parent_header = read_internal_header(&parent)?;
        parent_header.right_child = right.id();
        write_internal_header(&mut parent, &parent_header)?;

        self.pager.write_page(left.id(), &left)?;
        self.pager.write_page(right.id(), &right)?;
        self.pager.write_page(parent.id(), &parent)
    }
}

pub fn init_leaf_page(page: &mut Page) -> Result<()> {
    page.assert_page_type(PageType::Leaf)?;
    let header = LeafPageHeader {
        record_count: 0,
        record_content_start: page.body().len() as u16,
        right_sibling: PageId::new(0),
        left_sibling: PageId::new(0),
    };

    page.write_body_prefix(&header)?;
    Ok(())
}

/// Writes an empty internal-page header into `page`.
///
/// The page must already have `PageType::Internal` in its common page header.
pub fn init_internal_page(page: &mut Page) -> Result<()> {
    page.assert_page_type(PageType::Internal)?;
    let header = InternalPageHeader {
        record_count: 0,
        record_content_start: page.body().len() as u16,
        right_child: PageId::new(0),
    };

    write_internal_header(page, &header)
}

fn read_leaf_header(page: &Page) -> Result<LeafPageHeader> {
    page.assert_page_type(PageType::Leaf)?;
    let header = page.read_body_prefix::<LeafPageHeader>()?;

    if header.record_content_start as usize > page.body().len() {
        return Err(ShuError::CorruptedPage { page_id: page.id() });
    }

    Ok(header)
}

fn read_internal_header(page: &Page) -> Result<InternalPageHeader> {
    page.assert_page_type(PageType::Internal)?;
    let header = page.read_body_prefix::<InternalPageHeader>()?;
    validate_internal_header(page, &header)?;

    Ok(header)
}

fn write_internal_header(page: &mut Page, header: &InternalPageHeader) -> Result<()> {
    page.assert_page_type(PageType::Internal)?;
    validate_internal_header(page, header)?;
    page.write_body_prefix(header)
}

fn validate_internal_header(page: &Page, header: &InternalPageHeader) -> Result<()> {
    let record_content_start = usize::from(header.record_content_start);
    if record_content_start > page.body().len() {
        return Err(ShuError::CorruptedPage { page_id: page.id() });
    }

    internal_free_space(page.id(), header)?;
    Ok(())
}

fn read_leaf_cell<'a>(page: &'a Page, index: u16) -> Result<LeafCell<'a>> {
    page.assert_page_type(PageType::Leaf)?;
    let header = read_leaf_header(page)?;
    if index >= header.record_count {
        return Err(ShuError::IndexOutOfRange);
    }
    let pointer_offset = leaf_cell_pointer_offset(index);
    let cell_start = page.read_body_u16(pointer_offset)? as usize;
    let key_len = page.read_body_u16(cell_start)?;
    let value_len = page.read_body_u16(cell_start + 2)?;
    let layout = LeafCellLayout::new(cell_start, key_len, value_len);
    let key = page.read_body_bytes(layout.key_start..layout.key_end)?;
    let value = page.read_body_bytes(layout.value_start..layout.value_end)?;
    Ok(LeafCell { key, value })
}

fn collect_leaf_records(page: &Page) -> Result<Vec<OwnedLeafCell>> {
    let header = read_leaf_header(page)?;
    let mut records = Vec::with_capacity(usize::from(header.record_count));

    for index in 0..header.record_count {
        let cell = read_leaf_cell(page, index)?;
        records.push(OwnedLeafCell {
            key: cell.key.to_owned(),
            value: cell.value.to_owned(),
        });
    }

    Ok(records)
}

fn upsert_leaf_record(records: &mut Vec<OwnedLeafCell>, key: &[u8], value: &[u8]) {
    match records.binary_search_by(|record| record.key.as_slice().cmp(key)) {
        Ok(index) => records[index].value = value.to_owned(),
        Err(index) => records.insert(
            index,
            OwnedLeafCell {
                key: key.to_owned(),
                value: value.to_owned(),
            },
        ),
    }
}

fn write_leaf_records(page: &mut Page, records: &[OwnedLeafCell]) -> Result<()> {
    page.assert_page_type(PageType::Leaf)?;

    for record in records {
        insert_into_leaf(page, &record.key, &record.value)?;
    }

    Ok(())
}

/// Reads an internal-page cell by slot index.
pub fn read_internal_cell(page: &Page, index: u16) -> Result<InternalCell<'_>> {
    page.assert_page_type(PageType::Internal)?;
    let header = read_internal_header(page)?;
    if index >= header.record_count {
        return Err(ShuError::IndexOutOfRange);
    }

    let pointer_offset = internal_cell_pointer_offset(index);
    let cell_start = page.read_body_u16(pointer_offset)? as usize;
    let child_page_id = read_internal_cell_child_page_id(page, cell_start)?;
    let key_len = page.read_body_u16(cell_start + size_of::<PageId>())?;
    let layout = InternalCellLayout::new(cell_start, key_len);
    let key = page.read_body_bytes(layout.key_start..layout.key_end)?;

    Ok(InternalCell { child_page_id, key })
}

fn insert_into_leaf(page: &mut Page, key: &[u8], value: &[u8]) -> Result<()> {
    page.assert_page_type(PageType::Leaf)?;
    let mut header = read_leaf_header(page)?;
    let (key_len, value_len) = checked_cell_lens(page.id(), key.len(), value.len())?;
    let position = LeafInsertPosition::from_search(search_leaf(page, key)?);
    let cell_start = allocate_leaf_cell_start(page.id(), &header, key_len, value_len, position)?;
    let layout = LeafCellLayout::new(cell_start, key_len, value_len);

    if position.needs_pointer_shift(header.record_count) {
        shift_leaf_pointers(page, position.index, header.record_count)?;
    }

    write_leaf_cell(page, position, &layout, key_len, value_len, key, value)?;

    if position.is_new_key {
        header.record_count += 1;
    }
    header.record_content_start = layout.cell_start as u16;
    page.write_body_prefix(&header)
}

/// Appends an internal-page cell.
///
/// This is a page-format primitive. Callers that build a B-tree must preserve separator key order
/// and keep the page header's right-child field consistent with the tree shape.
pub fn append_internal_cell(page: &mut Page, child_page_id: PageId, key: &[u8]) -> Result<()> {
    page.assert_page_type(PageType::Internal)?;
    let mut header = read_internal_header(page)?;
    let key_len = checked_internal_key_len(page.id(), key.len())?;
    let cell_start = allocate_internal_cell_start(page.id(), &header, key_len)?;
    let layout = InternalCellLayout::new(cell_start, key_len);

    write_internal_cell(
        page,
        header.record_count,
        &layout,
        child_page_id,
        key_len,
        key,
    )?;

    header.record_count += 1;
    header.record_content_start = layout.cell_start as u16;
    write_internal_header(page, &header)
}

fn allocate_leaf_cell_start(
    page_id: PageId,
    header: &LeafPageHeader,
    key_len: u16,
    value_len: u16,
    position: LeafInsertPosition,
) -> Result<usize> {
    let cell_size = leaf_cell_size(key_len, value_len);
    let available = leaf_free_space(page_id, header)?;
    let needed = position.pointer_space_needed() + cell_size;

    if needed > available {
        return Err(ShuError::PageFull {
            page_id,
            needed,
            available,
        });
    }

    Ok(usize::from(header.record_content_start) - cell_size)
}

fn leaf_free_space(page_id: PageId, header: &LeafPageHeader) -> Result<usize> {
    let pointer_end = leaf_cell_pointer_offset(header.record_count);
    usize::from(header.record_content_start)
        .checked_sub(pointer_end)
        .ok_or(ShuError::CorruptedPage { page_id })
}

fn allocate_internal_cell_start(
    page_id: PageId,
    header: &InternalPageHeader,
    key_len: u16,
) -> Result<usize> {
    let cell_size = internal_cell_size(key_len);
    let available = internal_free_space(page_id, header)?;
    let needed = CELL_POINTER_SIZE + cell_size;

    if needed > available {
        return Err(ShuError::PageFull {
            page_id,
            needed,
            available,
        });
    }

    Ok(usize::from(header.record_content_start) - cell_size)
}

fn internal_free_space(page_id: PageId, header: &InternalPageHeader) -> Result<usize> {
    let pointer_end = internal_cell_pointer_offset(header.record_count);
    usize::from(header.record_content_start)
        .checked_sub(pointer_end)
        .ok_or(ShuError::CorruptedPage { page_id })
}

fn write_leaf_cell(
    page: &mut Page,
    position: LeafInsertPosition,
    layout: &LeafCellLayout,
    key_len: u16,
    value_len: u16,
    key: &[u8],
    value: &[u8],
) -> Result<()> {
    page.write_body_u16(position.pointer_offset(), layout.cell_start as u16)?;
    page.write_body_u16(layout.key_len_start, key_len)?;
    page.write_body_u16(layout.value_len_start, value_len)?;
    page.write_body_bytes(layout.key_start..layout.key_end, key)?;
    page.write_body_bytes(layout.value_start..layout.value_end, value)?;
    Ok(())
}

fn write_internal_cell(
    page: &mut Page,
    index: u16,
    layout: &InternalCellLayout,
    child_page_id: PageId,
    key_len: u16,
    key: &[u8],
) -> Result<()> {
    page.assert_page_type(PageType::Internal)?;
    page.write_body_u16(
        internal_cell_pointer_offset(index),
        layout.cell_start as u16,
    )?;
    page.write_body_bytes(
        layout.child_page_id_start..layout.key_len_start,
        bytemuck::bytes_of(&child_page_id),
    )?;
    page.write_body_u16(layout.key_len_start, key_len)?;
    page.write_body_bytes(layout.key_start..layout.key_end, key)?;
    Ok(())
}

fn read_internal_cell_child_page_id(page: &Page, cell_start: usize) -> Result<PageId> {
    let start = cell_start;
    let end = start + size_of::<PageId>();
    let bytes = page.read_body_bytes(start..end)?;
    Ok(bytemuck::pod_read_unaligned(bytes))
}

fn shift_leaf_pointers(page: &mut Page, start_index: u16, record_count: u16) -> Result<()> {
    let start = leaf_cell_pointer_offset(start_index);
    let end = leaf_cell_pointer_offset(record_count);
    let dst_start = start + CELL_POINTER_SIZE;
    page.copy_body_within(start..end, dst_start)
}

/// Returns the child page to follow for `new_key` in an internal page.
#[cfg(test)]
fn find_child_for_key(page: &Page, new_key: &[u8]) -> Result<PageId> {
    let child_index = find_child_index_for_key(page, new_key)?;
    child_page_id_at_index(page, child_index)
}

fn find_child_index_for_key(page: &Page, new_key: &[u8]) -> Result<u16> {
    page.assert_page_type(PageType::Internal)?;
    let mut low = 0;

    let header = read_internal_header(page)?;
    let mut high = header.record_count;

    while low < high {
        let mid = low + (high - low) / 2;
        let key = read_internal_cell(page, mid)?.key;
        if new_key <= key {
            high = mid
        } else {
            low = mid + 1
        }
    }

    Ok(low)
}

fn child_page_id_at_index(page: &Page, child_index: u16) -> Result<PageId> {
    page.assert_page_type(PageType::Internal)?;
    let header = read_internal_header(page)?;

    if child_index < header.record_count {
        return Ok(read_internal_cell(page, child_index)?.child_page_id);
    }
    if child_index == header.record_count {
        return Ok(header.right_child);
    }

    Err(ShuError::IndexOutOfRange)
}

/// Returns space where key should be inserted in the leaf page
fn search_leaf(page: &Page, new_key: &[u8]) -> Result<LeafSearchResult> {
    page.assert_page_type(PageType::Leaf)?;
    let mut low = 0;
    let mut high = read_leaf_header(page)?.record_count;
    while low < high {
        let mid = (low + high) / 2;
        let key = read_leaf_cell(page, mid)?.key;
        if key == new_key {
            return Ok(LeafSearchResult::Found(mid));
        } else if new_key < key {
            high = mid;
        } else {
            low = mid + 1;
        }
    }

    Ok(LeafSearchResult::Missing(low))
}

fn checked_cell_lens(page_id: PageId, key_len: usize, value_len: usize) -> Result<(u16, u16)> {
    if key_len > u16::MAX as usize || value_len > u16::MAX as usize {
        return Err(ShuError::CellTooLarge {
            page_id,
            key_len,
            value_len,
        });
    }

    Ok((key_len as u16, value_len as u16))
}

fn checked_internal_key_len(page_id: PageId, key_len: usize) -> Result<u16> {
    if key_len > u16::MAX as usize {
        return Err(ShuError::CellTooLarge {
            page_id,
            key_len,
            value_len: 0,
        });
    }

    Ok(key_len as u16)
}

fn internal_cell_size(key_len: u16) -> usize {
    INTERNAL_CELL_HEADER_SIZE + usize::from(key_len)
}

fn internal_cell_pointer_offset(index: u16) -> usize {
    size_of::<InternalPageHeader>() + usize::from(index) * CELL_POINTER_SIZE
}

fn leaf_cell_size(key_len: u16, value_len: u16) -> usize {
    LEAF_CELL_HEADER_SIZE + usize::from(key_len) + usize::from(value_len)
}

fn leaf_cell_pointer_offset(index: u16) -> usize {
    size_of::<LeafPageHeader>() + usize::from(index) * CELL_POINTER_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_leaf_cell_rejects_index_past_record_count() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();

        let result = read_leaf_cell(&page, 0);

        assert!(matches!(result, Err(ShuError::IndexOutOfRange)));
    }

    #[test]
    fn init_internal_page_writes_empty_header() {
        let mut page = Page::new(PageType::Internal, PageId::new(2));

        init_internal_page(&mut page).unwrap();

        let header = read_internal_header(&page).unwrap();
        assert_eq!(header.record_count, 0);
        assert_eq!(usize::from(header.record_content_start), page.body().len());
        assert_eq!(header.right_child, PageId::new(0));
    }

    #[test]
    fn append_internal_cell_writes_child_and_separator_key() {
        let mut page = Page::new(PageType::Internal, PageId::new(2));
        init_internal_page(&mut page).unwrap();

        append_internal_cell(&mut page, PageId::new(7), b"cat").unwrap();
        append_internal_cell(&mut page, PageId::new(8), b"dog").unwrap();

        let header = read_internal_header(&page).unwrap();
        assert_eq!(header.record_count, 2);

        let first = read_internal_cell(&page, 0).unwrap();
        assert_eq!(first.child_page_id, PageId::new(7));
        assert_eq!(first.key, b"cat");

        let second = read_internal_cell(&page, 1).unwrap();
        assert_eq!(second.child_page_id, PageId::new(8));
        assert_eq!(second.key, b"dog");
    }

    #[test]
    fn append_internal_cell_supports_empty_separator_key_at_body_tail() {
        let mut page = Page::new(PageType::Internal, PageId::new(2));
        init_internal_page(&mut page).unwrap();

        append_internal_cell(&mut page, PageId::new(7), b"").unwrap();

        let cell = read_internal_cell(&page, 0).unwrap();
        assert_eq!(cell.child_page_id, PageId::new(7));
        assert_eq!(cell.key, b"");
    }

    #[test]
    fn find_child_for_key_routes_internal_boundaries() {
        let mut page = Page::new(PageType::Internal, PageId::new(2));
        init_internal_page(&mut page).unwrap();
        append_internal_cell(&mut page, PageId::new(10), b"cat").unwrap();
        append_internal_cell(&mut page, PageId::new(11), b"dog").unwrap();
        append_internal_cell(&mut page, PageId::new(12), b"fox").unwrap();

        let mut header = read_internal_header(&page).unwrap();
        header.right_child = PageId::new(13);
        write_internal_header(&mut page, &header).unwrap();

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

        let mut header = read_internal_header(&page).unwrap();
        header.right_child = PageId::new(9);
        write_internal_header(&mut page, &header).unwrap();

        assert_eq!(find_child_for_key(&page, b"cat").unwrap(), PageId::new(9));
    }

    #[test]
    fn insert_into_leaf_appends_two_records() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();

        insert_into_leaf(&mut page, b"a", b"first").unwrap();
        insert_into_leaf(&mut page, b"b", b"second").unwrap();

        let header = read_leaf_header(&page).unwrap();
        assert_eq!(header.record_count, 2);

        let first = read_leaf_cell(&page, 0).unwrap();
        assert_eq!(first.key, b"a");
        assert_eq!(first.value, b"first");

        let second = read_leaf_cell(&page, 1).unwrap();
        assert_eq!(second.key, b"b");
        assert_eq!(second.value, b"second");
    }

    #[test]
    fn insert_into_leaf_keeps_slots_sorted_by_key() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();

        insert_into_leaf(&mut page, b"z", b"last").unwrap();
        insert_into_leaf(&mut page, b"a", b"first").unwrap();

        let header = read_leaf_header(&page).unwrap();
        assert_eq!(header.record_count, 2);

        let first = read_leaf_cell(&page, 0).unwrap();
        assert_eq!(first.key, b"a");
        assert_eq!(first.value, b"first");

        let second = read_leaf_cell(&page, 1).unwrap();
        assert_eq!(second.key, b"z");
        assert_eq!(second.value, b"last");
    }

    #[test]
    fn search_leaf_finds_existing_keys() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();

        insert_into_leaf(&mut page, b"a", b"first").unwrap();
        insert_into_leaf(&mut page, b"c", b"middle").unwrap();
        insert_into_leaf(&mut page, b"z", b"last").unwrap();

        assert_eq!(
            search_leaf(&page, b"a").unwrap(),
            LeafSearchResult::Found(0)
        );
        assert_eq!(
            search_leaf(&page, b"c").unwrap(),
            LeafSearchResult::Found(1)
        );
        assert_eq!(
            search_leaf(&page, b"z").unwrap(),
            LeafSearchResult::Found(2)
        );
    }

    #[test]
    fn search_leaf_returns_missing_insert_positions() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();

        insert_into_leaf(&mut page, b"a", b"first").unwrap();
        insert_into_leaf(&mut page, b"c", b"middle").unwrap();
        insert_into_leaf(&mut page, b"z", b"last").unwrap();

        assert_eq!(
            search_leaf(&page, b"0").unwrap(),
            LeafSearchResult::Missing(0)
        );
        assert_eq!(
            search_leaf(&page, b"b").unwrap(),
            LeafSearchResult::Missing(1)
        );
        assert_eq!(
            search_leaf(&page, b"d").unwrap(),
            LeafSearchResult::Missing(2)
        );
        assert_eq!(
            search_leaf(&page, b"zz").unwrap(),
            LeafSearchResult::Missing(3)
        );
    }

    fn allocate_leaf_with_records(tree: &mut BTree, records: &[(&[u8], &[u8])]) -> PageId {
        let page_id = tree.pager.allocate(PageType::Leaf).unwrap();
        let mut page = tree.pager.read_page(page_id).unwrap();
        for &(key, value) in records {
            insert_into_leaf(&mut page, key, value).unwrap();
        }
        tree.pager.write_page(page_id, &page).unwrap();
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
        let mut root = Page::new(PageType::Internal, root_page_id);
        init_internal_page(&mut root).unwrap();
        append_internal_cell(&mut root, left_leaf, b"cat").unwrap();
        append_internal_cell(&mut root, middle_left_leaf, b"dog").unwrap();
        append_internal_cell(&mut root, middle_right_leaf, b"fox").unwrap();

        let mut header = read_internal_header(&root).unwrap();
        header.right_child = right_leaf;
        write_internal_header(&mut root, &header).unwrap();
        tree.pager.write_page(root_page_id, &root).unwrap();

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
}
