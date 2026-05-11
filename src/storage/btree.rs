use std::{cmp::Ordering, mem::size_of, path::Path, usize};

use crate::{
    error::{Result, ShuError},
    storage::{
        page::{PAGE_SIZE, Page, PageId, PageType},
        pager::Pager,
    },
};

const CELL_POINTER_SIZE: usize = size_of::<u16>();
const LEAF_CELL_HEADER_SIZE: usize = 2 * size_of::<u16>();

pub struct BTree {
    pager: Pager,
}

pub struct LeafCell<'a> {
    key: &'a [u8],
    value: &'a [u8],
}

#[derive(Debug, Eq, PartialEq)]
enum SearchResult {
    // Replace key
    Found(u16),
    // Key doesn't exist insert
    Missing(u16),
}

#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct LeafPageHeader {
    record_count: u16,
    record_content_start: u16,
    right_sibling: PageId,
    left_sibling: PageId,
}

#[derive(Clone, Copy, Debug)]
struct LeafCellLayout {
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
            key_len_start,
            value_len_start,
            key_start,
            key_end,
            value_start,
            value_end,
        }
    }
}

impl BTree {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            pager: Pager::open(path)?,
        })
    }

    pub fn get(&mut self, key: &[u8]) -> Result<()> {
        todo!()
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        todo!()
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

fn read_leaf_header(page: &Page) -> Result<LeafPageHeader> {
    page.assert_page_type(PageType::Leaf)?;
    let header = page.read_body_prefix::<LeafPageHeader>()?;

    if header.record_content_start as usize > page.body().len() {
        return Err(ShuError::CorruptedPage { page_id: page.id() });
    }

    Ok(header)
}

fn read_leaf_cell<'a>(page: &'a Page, index: u16) -> Result<LeafCell<'a>> {
    page.assert_page_type(PageType::Leaf)?;
    let header = read_leaf_header(page)?;
    // TODO: proper error handling
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

fn insert_into_leaf(page: &mut Page, key: &[u8], value: &[u8]) -> Result<()> {
    page.assert_page_type(PageType::Leaf)?;
    let mut header = read_leaf_header(page)?;
    let (key_len, value_len) = checked_cell_lens(page.id(), key.len(), value.len())?;
    let cell_size = leaf_cell_size(key_len, value_len);
    let cell_start = usize::from(header.record_content_start) - cell_size;
    let layout = LeafCellLayout::new(cell_start, key_len, value_len);
    let (cell_index, is_new_key) = match search_leaf(page, key)? {
        SearchResult::Found(index) => (index, false),
        SearchResult::Missing(index) => (index, true),
    };
    let pointer_offset = leaf_cell_pointer_offset(cell_index);
    let pointer_end = pointer_offset + CELL_POINTER_SIZE;
    if pointer_end > cell_start {
        panic!("Page full -> this will not happen in the future this is placeholder")
    }
    if cell_size > header.record_content_start as usize {
        panic!("Page full panic this won't happen soon")
    }

    if is_new_key && cell_index != header.record_count {
        shift_leaf_pointers(page, cell_index, header.record_count)?;
    }

    page.write_body_u16(pointer_offset, cell_start as u16)?;

    // write key len and value
    page.write_body_u16(layout.key_len_start, key_len)?;
    page.write_body_u16(layout.value_len_start, value_len)?;
    // write key and value data
    page.write_body_bytes(layout.key_start..layout.key_end, key)?;
    page.write_body_bytes(layout.value_start..layout.value_end, value)?;
    if is_new_key {
        header.record_count += 1;
    }
    header.record_content_start = cell_start as u16;
    page.write_body_prefix(&header)?;

    Ok(())
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
}
