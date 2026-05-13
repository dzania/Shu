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

pub struct InternalCell<'a> {
    child_page_id: PageId,
    key: &'a [u8],
}

#[derive(Debug, Eq, PartialEq)]
enum SearchResult {
    Found(u16),
    Missing(u16),
}

#[derive(Clone, Copy, Debug)]
struct LeafInsertPosition {
    index: u16,
    is_new_key: bool,
}

impl LeafInsertPosition {
    fn from_search(result: SearchResult) -> Self {
        match result {
            SearchResult::Found(index) => Self {
                index,
                is_new_key: false,
            },
            SearchResult::Missing(index) => Self {
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
        let page = self.pager.read_page(self.pager.root_page_id())?;
        match search_leaf(&page, key)? {
            SearchResult::Found(index) => {
                let cell = read_leaf_cell(&page, index)?;
                Ok(Some(cell.value.to_owned()))
            }
            SearchResult::Missing(_) => Ok(None),
        }
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let root_page_id = self.pager.root_page_id();
        let mut page = self.pager.read_page(root_page_id)?;
        insert_into_leaf(&mut page, key, value)?;
        self.pager.write_page(root_page_id, &page)
    }

    pub fn sync(&mut self) -> Result<()> {
        self.pager.sync()
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

fn read_internal_cell<'a>(page: &'a Page, index: u16) -> Result<InternalCell<'a>> {
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

fn append_internal_cell(page: &mut Page, child_page_id: PageId, key: &[u8]) -> Result<()> {
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

fn search_leaf(page: &Page, new_key: &[u8]) -> Result<SearchResult> {
    page.assert_page_type(PageType::Leaf)?;
    let mut low = 0;
    let mut high = read_leaf_header(page)?.record_count;
    while low < high {
        let mid = (low + high) / 2;
        let key = read_leaf_cell(page, mid)?.key;
        if key == new_key {
            return Ok(SearchResult::Found(mid));
        } else if new_key < key {
            high = mid;
        } else {
            low = mid + 1;
        }
    }

    Ok(SearchResult::Missing(low))
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

        assert_eq!(search_leaf(&page, b"a").unwrap(), SearchResult::Found(0));
        assert_eq!(search_leaf(&page, b"c").unwrap(), SearchResult::Found(1));
        assert_eq!(search_leaf(&page, b"z").unwrap(), SearchResult::Found(2));
    }

    #[test]
    fn search_leaf_returns_missing_insert_positions() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();

        insert_into_leaf(&mut page, b"a", b"first").unwrap();
        insert_into_leaf(&mut page, b"c", b"middle").unwrap();
        insert_into_leaf(&mut page, b"z", b"last").unwrap();

        assert_eq!(search_leaf(&page, b"0").unwrap(), SearchResult::Missing(0));
        assert_eq!(search_leaf(&page, b"b").unwrap(), SearchResult::Missing(1));
        assert_eq!(search_leaf(&page, b"d").unwrap(), SearchResult::Missing(2));
        assert_eq!(search_leaf(&page, b"zz").unwrap(), SearchResult::Missing(3));
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
}
