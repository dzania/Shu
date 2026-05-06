use std::{cmp::Ordering, mem::size_of, path::Path};

use crate::{
    error::{Result, ShuError},
    storage::{
        page::{Page, PageId, PageType},
        pager::Pager,
    },
};

const NO_PAGE: PageId = PageId::new(0);
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

    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let root_page_id = self.pager.root_page_id();
        let root = self.pager.read_page(root_page_id)?;
        let leaf = LeafPage::new(&root)?;

        Ok(leaf.get(key)?.map(|value| value.to_vec()))
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let root_page_id = self.pager.root_page_id();
        let mut root = self.pager.read_page(root_page_id)?;

        LeafPageMut::new(&mut root)?.put(key, value)?;
        self.pager.write_page(root_page_id, &root)
    }

    pub fn sync(&mut self) -> Result<()> {
        self.pager.sync()
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    bytemuck::Pod,
    bytemuck::Zeroable,
)]
#[repr(C)]
struct BTreeNodeHeader {
    cell_count: u16,
    cell_content_start: u16,
    right_sibling: PageId,
    left_sibling: PageId,
    right_child: PageId,
}

impl BTreeNodeHeader {
    fn empty_leaf(body_len: usize) -> Self {
        assert!(body_len <= u16::MAX as usize);

        Self {
            cell_count: 0,
            cell_content_start: body_len as u16,
            right_sibling: NO_PAGE,
            left_sibling: NO_PAGE,
            right_child: NO_PAGE,
        }
    }

    fn read_from(page: &Page) -> Result<Self> {
        page.assert_page_type(PageType::Leaf)?;
        page.read_body_prefix::<Self>()
    }

    fn write_to(&self, page: &mut Page) -> Result<()> {
        page.assert_page_type(PageType::Leaf)?;
        page.write_body_prefix(self)
    }

    fn validate(&self, page_id: PageId, body_len: usize) -> Result<()> {
        if self.cell_content_start as usize > body_len {
            return Err(ShuError::CorruptedPage { page_id });
        }

        let pointer_bytes = (self.cell_count as usize)
            .checked_mul(CELL_POINTER_SIZE)
            .ok_or(ShuError::CorruptedPage { page_id })?;
        let pointer_end = size_of::<Self>()
            .checked_add(pointer_bytes)
            .ok_or(ShuError::CorruptedPage { page_id })?;

        if pointer_end > self.cell_content_start as usize {
            return Err(ShuError::CorruptedPage { page_id });
        }

        if self.right_child != NO_PAGE {
            return Err(ShuError::CorruptedPage { page_id });
        }

        Ok(())
    }
}

pub(crate) fn init_leaf_page(page: &mut Page) -> Result<()> {
    page.assert_page_type(PageType::Leaf)?;
    page.body_mut().fill(0);
    let header = BTreeNodeHeader::empty_leaf(page.body().len());
    header.write_to(page)
}

struct LeafPage<'a> {
    page: &'a Page,
    header: BTreeNodeHeader,
}

impl<'a> LeafPage<'a> {
    fn new(page: &'a Page) -> Result<Self> {
        let header = BTreeNodeHeader::read_from(page)?;
        header.validate(page.id(), page.body().len())?;

        Ok(Self { page, header })
    }

    fn cell_count(&self) -> u16 {
        self.header.cell_count
    }

    #[cfg(test)]
    fn free_space(&self) -> usize {
        let lower =
            size_of::<BTreeNodeHeader>() + self.header.cell_count as usize * CELL_POINTER_SIZE;
        let upper = self.header.cell_content_start as usize;
        assert!(lower <= upper);
        upper - lower
    }

    fn get(&self, key: &[u8]) -> Result<Option<&'a [u8]>> {
        match self.search(key)? {
            SearchResult::Found(index) => Ok(Some(self.cell(index)?.value)),
            SearchResult::Missing(_) => Ok(None),
        }
    }

    fn search(&self, key: &[u8]) -> Result<SearchResult> {
        for index in 0..self.cell_count() {
            let cell = self.cell(index)?;

            match cell.key.cmp(key) {
                Ordering::Equal => return Ok(SearchResult::Found(index)),
                Ordering::Greater => return Ok(SearchResult::Missing(index)),
                Ordering::Less => {}
            }
        }

        Ok(SearchResult::Missing(self.cell_count()))
    }

    fn cell_pointer(&self, index: u16) -> Result<u16> {
        if index >= self.header.cell_count {
            return Err(ShuError::IndexOutOfRange);
        };

        let start = size_of::<BTreeNodeHeader>() + index as usize * CELL_POINTER_SIZE;
        read_u16_at(self.page.body(), start, self.page.id())
    }

    fn cell(&self, index: u16) -> Result<LeafCell<'a>> {
        let cell_offset = self.cell_pointer(index)? as usize;
        let body = self.page.body();
        let page_id = self.page.id();

        if cell_offset < self.header.cell_content_start as usize || cell_offset >= body.len() {
            return Err(ShuError::CorruptedPage { page_id });
        }

        let key_len = read_u16_at(body, cell_offset, page_id)? as usize;
        let value_len = read_u16_at(body, cell_offset + size_of::<u16>(), page_id)? as usize;
        let key_start = checked_end(cell_offset, LEAF_CELL_HEADER_SIZE, page_id)?;
        let value_start = checked_end(key_start, key_len, page_id)?;
        let value_end = checked_end(value_start, value_len, page_id)?;

        if value_end > body.len() {
            return Err(ShuError::CorruptedPage { page_id });
        }

        Ok(LeafCell {
            key: &body[key_start..value_start],
            value: &body[value_start..value_end],
        })
    }
}

struct LeafPageMut<'a> {
    page: &'a mut Page,
}

impl<'a> LeafPageMut<'a> {
    fn new(page: &'a mut Page) -> Result<Self> {
        LeafPage::new(page)?;
        Ok(Self { page })
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let page_id = self.page.id();
        let mut cells = self.cells_owned()?;
        let new_cell = OwnedLeafCell::new(page_id, key, value)?;
        let mut insert_index = cells.len();
        let mut replace_existing = false;

        for (index, cell) in cells.iter().enumerate() {
            match cell.key.as_slice().cmp(key) {
                Ordering::Equal => {
                    insert_index = index;
                    replace_existing = true;
                    break;
                }
                Ordering::Greater => {
                    insert_index = index;
                    break;
                }
                Ordering::Less => {}
            }
        }

        if replace_existing {
            cells[insert_index] = new_cell;
        } else {
            cells.insert(insert_index, new_cell);
        }

        self.write_cells(&cells)
    }

    fn cells_owned(&self) -> Result<Vec<OwnedLeafCell>> {
        let leaf = LeafPage::new(self.page)?;
        let mut cells = Vec::with_capacity(leaf.cell_count() as usize);

        for index in 0..leaf.cell_count() {
            let cell = leaf.cell(index)?;
            cells.push(OwnedLeafCell {
                key: cell.key.to_vec(),
                value: cell.value.to_vec(),
            });
        }

        Ok(cells)
    }

    fn write_cells(&mut self, cells: &[OwnedLeafCell]) -> Result<()> {
        let page_id = self.page.id();
        let old_header = BTreeNodeHeader::read_from(self.page)?;
        old_header.validate(page_id, self.page.body().len())?;

        let available = self.page.body().len();
        let required = required_leaf_space(page_id, cells)?;
        if required > available {
            return Err(ShuError::PageFull {
                page_id,
                needed: required,
                available,
            });
        }

        self.page.body_mut().fill(0);

        let mut cell_content_start = available;
        {
            let body = self.page.body_mut();
            for (index, cell) in cells.iter().enumerate() {
                cell_content_start -= cell.encoded_len();

                let pointer_start = size_of::<BTreeNodeHeader>() + index * CELL_POINTER_SIZE;
                let pointer_end = pointer_start + CELL_POINTER_SIZE;
                body[pointer_start..pointer_end]
                    .copy_from_slice(&(cell_content_start as u16).to_le_bytes());

                write_cell_at(body, cell_content_start, cell);
            }
        }

        let header = BTreeNodeHeader {
            cell_count: cells.len() as u16,
            cell_content_start: cell_content_start as u16,
            right_sibling: old_header.right_sibling,
            left_sibling: old_header.left_sibling,
            right_child: NO_PAGE,
        };
        header.write_to(self.page)
    }
}

#[derive(Debug, Eq, PartialEq)]
struct LeafCell<'a> {
    key: &'a [u8],
    value: &'a [u8],
}

#[derive(Debug, Eq, PartialEq)]
struct OwnedLeafCell {
    key: Vec<u8>,
    value: Vec<u8>,
}

impl OwnedLeafCell {
    fn new(page_id: PageId, key: &[u8], value: &[u8]) -> Result<Self> {

        Ok(Self {
            key: key.to_vec(),
            value: value.to_vec(),
        })
    }

    fn encoded_len(&self) -> usize {
        LEAF_CELL_HEADER_SIZE + self.key.len() + self.value.len()
    }
}

#[derive(Debug, Eq, PartialEq)]
enum SearchResult {
    Found(u16),
    Missing(u16),
}

fn read_u16_at(body: &[u8], offset: usize, page_id: PageId) -> Result<u16> {
    let end = checked_end(offset, size_of::<u16>(), page_id)?;
    if end > body.len() {
        return Err(ShuError::CorruptedPage { page_id });
    }

    Ok(u16::from_le_bytes([body[offset], body[offset + 1]]))
}

fn checked_end(offset: usize, len: usize, page_id: PageId) -> Result<usize> {
    offset
        .checked_add(len)
        .ok_or(ShuError::CorruptedPage { page_id })
}


fn required_leaf_space(page_id: PageId, cells: &[OwnedLeafCell]) -> Result<usize> {
    if cells.len() > u16::MAX as usize {
        return Err(ShuError::PageFull {
            page_id,
            needed: usize::MAX,
            available: 0,
        });
    }

    let pointer_bytes = cells
        .len()
        .checked_mul(CELL_POINTER_SIZE)
        .ok_or(ShuError::PageFull {
            page_id,
            needed: usize::MAX,
            available: 0,
        })?;
    let mut required = size_of::<BTreeNodeHeader>()
        .checked_add(pointer_bytes)
        .ok_or(ShuError::PageFull {
            page_id,
            needed: usize::MAX,
            available: 0,
        })?;

    for cell in cells {
        required = required
            .checked_add(cell.encoded_len())
            .ok_or(ShuError::PageFull {
                page_id,
                needed: usize::MAX,
                available: 0,
            })?;
    }

    Ok(required)
}

fn write_cell_at(body: &mut [u8], offset: usize, cell: &OwnedLeafCell) {
    let key_len_end = offset + size_of::<u16>();
    let value_len_end = key_len_end + size_of::<u16>();
    let key_end = value_len_end + cell.key.len();
    let value_end = key_end + cell.value.len();

    body[offset..key_len_end].copy_from_slice(&(cell.key.len() as u16).to_le_bytes());
    body[key_len_end..value_len_end].copy_from_slice(&(cell.value.len() as u16).to_le_bytes());
    body[value_len_end..key_end].copy_from_slice(&cell.key);
    body[key_end..value_end].copy_from_slice(&cell.value);
}

#[cfg(test)]
mod tests {
    use tempfile::NamedTempFile;

    use super::*;
    use crate::storage::header::INITIAL_ROOT_PAGE_ID;

    fn read_node_header(page: &Page) -> BTreeNodeHeader {
        page.read_body_prefix::<BTreeNodeHeader>().unwrap()
    }

    fn write_leaf_cell(page: &mut Page, index: u16, key: &[u8], value: &[u8]) -> u16 {
        assert!(key.len() <= u16::MAX as usize);
        assert!(value.len() <= u16::MAX as usize);

        let mut header = read_node_header(page);
        let cell_len = LEAF_CELL_HEADER_SIZE + key.len() + value.len();
        let cell_offset = header.cell_content_start as usize - cell_len;
        assert!(cell_offset <= u16::MAX as usize);

        let pointer_start = size_of::<BTreeNodeHeader>() + index as usize * CELL_POINTER_SIZE;
        let pointer_end = pointer_start + CELL_POINTER_SIZE;
        let key_len_end = cell_offset + size_of::<u16>();
        let value_len_end = key_len_end + size_of::<u16>();
        let key_end = value_len_end + key.len();
        let value_end = key_end + value.len();

        {
            let body = page.body_mut();
            body[pointer_start..pointer_end].copy_from_slice(&(cell_offset as u16).to_le_bytes());
            body[cell_offset..key_len_end].copy_from_slice(&(key.len() as u16).to_le_bytes());
            body[key_len_end..value_len_end].copy_from_slice(&(value.len() as u16).to_le_bytes());
            body[value_len_end..key_end].copy_from_slice(key);
            body[key_end..value_end].copy_from_slice(value);
        }

        header.cell_count = header.cell_count.max(index + 1);
        header.cell_content_start = cell_offset as u16;
        header.write_to(page).unwrap();

        cell_offset as u16
    }

    #[test]
    fn init_leaf_page_sets_empty_leaf_header() {
        let mut page = Page::new(PageType::Leaf, PageId::new(7));

        init_leaf_page(&mut page).unwrap();

        let header = read_node_header(&page);
        assert_eq!(header.cell_count, 0);
        assert_eq!(header.cell_content_start, page.body().len() as u16);
        assert_eq!(header.right_sibling, NO_PAGE);
        assert_eq!(header.left_sibling, NO_PAGE);
        assert_eq!(header.right_child, NO_PAGE);
    }

    #[test]
    fn new_database_root_leaf_is_initialized() {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let root = pager.read_page(INITIAL_ROOT_PAGE_ID).unwrap();

        let header = read_node_header(&root);
        assert_eq!(header.cell_count, 0);
        assert_eq!(header.cell_content_start, root.body().len() as u16);
        assert_eq!(header.right_sibling, NO_PAGE);
        assert_eq!(header.left_sibling, NO_PAGE);
    }

    #[test]
    fn allocated_leaf_pages_are_initialized() {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let page_id = pager.allocate(PageType::Leaf).unwrap();
        let page = pager.read_page(page_id).unwrap();

        let header = read_node_header(&page);
        assert_eq!(header.cell_count, 0);
        assert_eq!(header.cell_content_start, page.body().len() as u16);
        assert_eq!(header.right_sibling, NO_PAGE);
        assert_eq!(header.left_sibling, NO_PAGE);
    }

    #[test]
    fn empty_leaf_page_reports_zero_cells_and_full_free_space() {
        let mut page = Page::new(PageType::Leaf, PageId::new(7));
        init_leaf_page(&mut page).unwrap();

        let leaf = LeafPage::new(&page).unwrap();

        assert_eq!(leaf.cell_count(), 0);
        assert_eq!(
            leaf.free_space(),
            page.body().len() - size_of::<BTreeNodeHeader>()
        );
    }

    #[test]
    fn leaf_page_reads_cell_pointer_by_logical_index() {
        let mut page = Page::new(PageType::Leaf, PageId::new(7));
        init_leaf_page(&mut page).unwrap();

        let mut header = read_node_header(&page);
        header.cell_count = 2;
        page.write_body_prefix(&header).unwrap();

        let first_pointer_start = size_of::<BTreeNodeHeader>();
        let second_pointer_start = first_pointer_start + CELL_POINTER_SIZE;
        page.body_mut()[first_pointer_start..second_pointer_start]
            .copy_from_slice(&300_u16.to_le_bytes());
        page.body_mut()[second_pointer_start..second_pointer_start + CELL_POINTER_SIZE]
            .copy_from_slice(&400_u16.to_le_bytes());

        let leaf = LeafPage::new(&page).unwrap();
        assert_eq!(leaf.cell_pointer(0).unwrap(), 300);
        assert_eq!(leaf.cell_pointer(1).unwrap(), 400);
    }

    #[test]
    fn leaf_page_rejects_pointer_index_at_cell_count() {
        let mut page = Page::new(PageType::Leaf, PageId::new(7));
        init_leaf_page(&mut page).unwrap();

        let mut header = read_node_header(&page);
        header.cell_count = 2;
        page.write_body_prefix(&header).unwrap();

        let leaf = LeafPage::new(&page).unwrap();
        assert!(matches!(
            leaf.cell_pointer(2),
            Err(ShuError::IndexOutOfRange)
        ));
    }

    #[test]
    fn leaf_page_decodes_cell_key_and_value() {
        let mut page = Page::new(PageType::Leaf, PageId::new(7));
        init_leaf_page(&mut page).unwrap();
        write_leaf_cell(&mut page, 0, b"cat", b"meow");

        let leaf = LeafPage::new(&page).unwrap();
        let cell = leaf.cell(0).unwrap();

        assert_eq!(cell.key, b"cat");
        assert_eq!(cell.value, b"meow");
    }

    #[test]
    fn leaf_page_decodes_cell_by_logical_pointer_index() {
        let mut page = Page::new(PageType::Leaf, PageId::new(7));
        init_leaf_page(&mut page).unwrap();
        write_leaf_cell(&mut page, 1, b"z", b"last");
        write_leaf_cell(&mut page, 0, b"a", b"first");

        let leaf = LeafPage::new(&page).unwrap();
        let first = leaf.cell(0).unwrap();
        let second = leaf.cell(1).unwrap();

        assert_eq!(first.key, b"a");
        assert_eq!(first.value, b"first");
        assert_eq!(second.key, b"z");
        assert_eq!(second.value, b"last");
    }

    #[test]
    fn leaf_page_rejects_cell_index_at_cell_count() {
        let mut page = Page::new(PageType::Leaf, PageId::new(7));
        init_leaf_page(&mut page).unwrap();
        write_leaf_cell(&mut page, 0, b"cat", b"meow");

        let leaf = LeafPage::new(&page).unwrap();

        assert!(matches!(leaf.cell(1), Err(ShuError::IndexOutOfRange)));
    }

    #[test]
    fn leaf_page_rejects_cell_pointer_outside_body() {
        let mut page = Page::new(PageType::Leaf, PageId::new(7));
        init_leaf_page(&mut page).unwrap();

        let mut header = read_node_header(&page);
        header.cell_count = 1;
        header.write_to(&mut page).unwrap();

        let pointer_start = size_of::<BTreeNodeHeader>();
        let pointer_end = pointer_start + CELL_POINTER_SIZE;
        let body_len = page.body().len() as u16;
        page.body_mut()[pointer_start..pointer_end].copy_from_slice(&body_len.to_le_bytes());

        let leaf = LeafPage::new(&page).unwrap();

        match leaf.cell(0) {
            Err(ShuError::CorruptedPage { page_id }) => {
                assert_eq!(page_id, PageId::new(7));
            }
            result => panic!("expected corrupted page error, got {result:?}"),
        }
    }

    #[test]
    fn leaf_page_mut_inserts_cells_in_key_order() {
        let mut page = Page::new(PageType::Leaf, PageId::new(7));
        init_leaf_page(&mut page).unwrap();

        let mut leaf = LeafPageMut::new(&mut page).unwrap();
        leaf.put(b"z", b"last").unwrap();
        leaf.put(b"a", b"first").unwrap();

        let leaf = LeafPage::new(&page).unwrap();
        assert_eq!(leaf.cell_count(), 2);
        assert_eq!(leaf.cell(0).unwrap().key, b"a");
        assert_eq!(leaf.cell(1).unwrap().key, b"z");
    }

    #[test]
    fn leaf_page_mut_replaces_existing_key() {
        let mut page = Page::new(PageType::Leaf, PageId::new(7));
        init_leaf_page(&mut page).unwrap();

        let mut leaf = LeafPageMut::new(&mut page).unwrap();
        leaf.put(b"cat", b"meow").unwrap();
        leaf.put(b"cat", b"purr").unwrap();

        let leaf = LeafPage::new(&page).unwrap();
        assert_eq!(leaf.cell_count(), 1);
        assert_eq!(leaf.get(b"cat").unwrap(), Some(&b"purr"[..]));
    }

    #[test]
    fn btree_put_get_round_trip() {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut tree = BTree::open(&path).unwrap();
        tree.put(b"cat", b"meow").unwrap();
        tree.put(b"ant", b"tiny").unwrap();

        assert_eq!(tree.get(b"cat").unwrap(), Some(b"meow".to_vec()));
        assert_eq!(tree.get(b"ant").unwrap(), Some(b"tiny".to_vec()));
        assert_eq!(tree.get(b"dog").unwrap(), None);
    }

    #[test]
    fn btree_reopen_preserves_root_leaf_values() {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        {
            let mut tree = BTree::open(&path).unwrap();
            tree.put(b"cat", b"meow").unwrap();
            tree.sync().unwrap();
        }

        let mut tree = BTree::open(&path).unwrap();
        assert_eq!(tree.get(b"cat").unwrap(), Some(b"meow".to_vec()));
    }
}
