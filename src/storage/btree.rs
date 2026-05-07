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

pub struct LeafCell<'a> {
    key: &'a [u8],
    value: &'a [u8],
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

fn insert_into_empty_leaf(page: &mut Page, key: &[u8], value: &[u8]) -> Result<()> {
    page.assert_page_type(PageType::Leaf)?;
    let mut header = read_leaf_header(page)?;
    assert!(header.record_count == 0);

    let (key_len, value_len) = checked_cell_lens(page.id(), key.len(), value.len())?;
    let cell_size = leaf_cell_size(key_len, value_len);
    let cell_start = usize::from(header.record_content_start) - cell_size;
    let layout = LeafCellLayout::new(cell_start, key_len, value_len);
    let pointer_offset = leaf_cell_pointer_offset(0);
    // Write cell pointer
    page.write_body_u16(pointer_offset, cell_start as u16)?;

    // Write key len and value
    page.write_body_u16(layout.key_len_start, key_len)?;
    page.write_body_u16(layout.value_len_start, value_len)?;
    // Write key and value data
    page.write_body_bytes(layout.key_start..layout.key_end, key)?;
    page.write_body_bytes(layout.value_start..layout.value_end, value)?;
    header.record_count = 1;
    header.record_content_start = cell_start as u16;
    page.write_body_prefix(&header)?;

    Ok(())
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

#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct LeafPageHeader {
    record_count: u16,
    record_content_start: u16,
    right_sibling: PageId,
    left_sibling: PageId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_into_empty_leaf_stores_one_cell() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();

        insert_into_empty_leaf(&mut page, b"abc", b"hello").unwrap();

        let header = read_leaf_header(&page).unwrap();
        assert_eq!(header.record_count, 1);

        let body = page.body();
        let pointer_offset = leaf_cell_pointer_offset(0);
        let cell_start = u16::from_le_bytes(
            body[pointer_offset..pointer_offset + CELL_POINTER_SIZE]
                .try_into()
                .unwrap(),
        );
        assert_eq!(cell_start, header.record_content_start);

        let cell_start = usize::from(cell_start);
        let key_len = u16::from_le_bytes(
            body[cell_start..cell_start + size_of::<u16>()]
                .try_into()
                .unwrap(),
        );
        let value_len_offset = cell_start + size_of::<u16>();
        let value_len = u16::from_le_bytes(
            body[value_len_offset..value_len_offset + size_of::<u16>()]
                .try_into()
                .unwrap(),
        );
        assert_eq!(key_len, 3);
        assert_eq!(value_len, 5);

        let layout = LeafCellLayout::new(cell_start, key_len, value_len);
        assert_eq!(&body[layout.key_start..layout.key_end], b"abc");
        assert_eq!(&body[layout.value_start..layout.value_end], b"hello");
    }

    #[test]
    fn read_leaf_cell_returns_inserted_key_and_value() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();
        insert_into_empty_leaf(&mut page, b"abc", b"hello").unwrap();

        let cell = read_leaf_cell(&page, 0).unwrap();

        assert_eq!(cell.key, b"abc");
        assert_eq!(cell.value, b"hello");
    }

    #[test]
    fn read_leaf_cell_rejects_index_past_record_count() {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        init_leaf_page(&mut page).unwrap();

        let result = read_leaf_cell(&page, 0);

        assert!(matches!(result, Err(ShuError::IndexOutOfRange)));
    }
}
