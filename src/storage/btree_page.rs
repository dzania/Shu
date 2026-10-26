use std::{cmp::Ordering, mem::size_of};

use crate::{
    error::{Result, ShuError},
    storage::page::{HEADER_SIZE, PAGE_SIZE, Page, PageId, PageType},
};

const BODY_SIZE: usize = PAGE_SIZE - HEADER_SIZE;
const CELL_POINTER_SIZE: usize = size_of::<u16>();
const LEAF_HEADER_SIZE: usize = size_of::<LeafPageHeader>();
const INTERNAL_HEADER_SIZE: usize = size_of::<InternalPageHeader>();
const LEAF_CELL_HEADER_SIZE: usize = 2 * size_of::<u16>();
const INTERNAL_CELL_HEADER_SIZE: usize = size_of::<PageId>() + size_of::<u16>();

/// Bytes available for cells and cell pointers in an empty leaf page.
pub(crate) const LEAF_ENTRIES_CAPACITY: usize = BODY_SIZE - LEAF_HEADER_SIZE;
/// Bytes available for cells and cell pointers in an empty internal page.
pub(crate) const INTERNAL_ENTRIES_CAPACITY: usize = BODY_SIZE - INTERNAL_HEADER_SIZE;

// Cell offsets are stored as `u16`, so every body offset must fit in one.
const _: () = assert!(BODY_SIZE <= u16::MAX as usize);
const _: () = assert!(LEAF_HEADER_SIZE < BODY_SIZE);
const _: () = assert!(INTERNAL_HEADER_SIZE < BODY_SIZE);
// Any separator copied from a leaf key must fit in an empty internal page.
const _: () = assert!(
    LEAF_ENTRIES_CAPACITY - LEAF_CELL_HEADER_SIZE
        <= INTERNAL_ENTRIES_CAPACITY - INTERNAL_CELL_HEADER_SIZE
);

/// The two page types that make up the B-tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NodeKind {
    Leaf,
    Internal,
}

impl From<NodeKind> for PageType {
    fn from(kind: NodeKind) -> Self {
        match kind {
            NodeKind::Leaf => PageType::Leaf,
            NodeKind::Internal => PageType::Internal,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LeafEntry {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

impl LeafEntry {
    pub(crate) fn new(key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }

    /// Returns the page bytes this entry occupies, including its cell pointer.
    pub(crate) fn space(&self) -> usize {
        leaf_entry_space(self.key.len(), self.value.len())
    }
}

/// Returns the page bytes a leaf entry occupies, including its cell pointer.
pub(crate) fn leaf_entry_space(key_len: usize, value_len: usize) -> usize {
    CELL_POINTER_SIZE + LEAF_CELL_HEADER_SIZE + key_len + value_len
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LeafEntryRef<'a> {
    pub key: &'a [u8],
    pub value: &'a [u8],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InternalEntry {
    pub child: PageId,
    pub separator: Vec<u8>,
}

impl InternalEntry {
    pub(crate) fn new(child: PageId, separator: impl Into<Vec<u8>>) -> Self {
        Self {
            child,
            separator: separator.into(),
        }
    }

    /// Returns the page bytes this entry occupies, including its cell pointer.
    pub(crate) fn space(&self) -> usize {
        CELL_POINTER_SIZE + INTERNAL_CELL_HEADER_SIZE + self.separator.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InternalEntryRef<'a> {
    pub child: PageId,
    pub separator: &'a [u8],
}

/// Owned contents of an internal page. Child `i` holds keys `<= entries[i].separator`;
/// `right_child` holds keys greater than every separator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InternalEntries {
    pub entries: Vec<InternalEntry>,
    pub right_child: PageId,
}

impl InternalEntries {
    pub(crate) fn new(entries: Vec<InternalEntry>, right_child: PageId) -> Self {
        Self {
            entries,
            right_child,
        }
    }

    pub(crate) fn space(&self) -> usize {
        self.entries.iter().map(InternalEntry::space).sum()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeafSearchResult {
    Found(u16),
    Missing(u16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub(crate) enum LeafInsert {
    Inserted,
    /// The contiguous free space is too small. The page is unchanged; the caller must compact or
    /// split it.
    NoSpace,
}

/// Slotted cell area shared by leaf and internal pages. Cell pointers grow up from the end of the
/// page-type header and cells grow down from the end of the body.
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct CellArea {
    record_count: u16,
    /// Body offset of the lowest cell.
    content_start: u16,
}

impl CellArea {
    const EMPTY: Self = Self {
        record_count: 0,
        content_start: BODY_SIZE as u16,
    };

    fn pointers_end(self, header_size: usize) -> usize {
        header_size + usize::from(self.record_count) * CELL_POINTER_SIZE
    }

    fn validate(self, page_id: PageId, header_size: usize) -> Result<()> {
        let content_start = usize::from(self.content_start);
        if content_start > BODY_SIZE || self.pointers_end(header_size) > content_start {
            return Err(ShuError::CorruptedPage { page_id });
        }
        Ok(())
    }

    /// Contiguous free bytes between the pointer array and the cells. Holes left by replaced
    /// cells are not counted; only a rewrite reclaims them.
    fn free_space(self, header_size: usize) -> usize {
        usize::from(self.content_start) - self.pointers_end(header_size)
    }

    fn cell_start(self, page: &Page, header_size: usize, index: u16) -> Result<usize> {
        if index >= self.record_count {
            return Err(ShuError::IndexOutOfRange);
        }
        let pointer_offset = header_size + usize::from(index) * CELL_POINTER_SIZE;
        let cell_start = usize::from(page.read_body_u16(pointer_offset)?);
        if cell_start < usize::from(self.content_start) {
            return Err(ShuError::CorruptedPage { page_id: page.id() });
        }
        Ok(cell_start)
    }

    /// Takes `cell_size` bytes from the free space for a cell without a pointer. Returns the body
    /// offset where the caller must write the cell.
    fn allocate_cell(
        &mut self,
        page_id: PageId,
        header_size: usize,
        cell_size: usize,
    ) -> Result<u16> {
        let available = self.free_space(header_size);
        if cell_size > available {
            return Err(page_full(page_id, cell_size, available));
        }
        self.content_start -= u16::try_from(cell_size).expect("cell size is bounded by free space");
        Ok(self.content_start)
    }

    /// Reserves space for a new cell at pointer `index`, shifting later pointers right. Returns the
    /// body offset where the caller must write the cell.
    fn insert_cell(
        &mut self,
        page: &mut Page,
        header_size: usize,
        index: u16,
        cell_size: usize,
    ) -> Result<usize> {
        if index > self.record_count {
            return Err(ShuError::IndexOutOfRange);
        }
        let needed = cell_size + CELL_POINTER_SIZE;
        let available = self.free_space(header_size);
        if needed > available {
            return Err(page_full(page.id(), needed, available));
        }

        let pointer_offset = header_size + usize::from(index) * CELL_POINTER_SIZE;
        let pointers_end = self.pointers_end(header_size);
        page.copy_body_within(
            pointer_offset..pointers_end,
            pointer_offset + CELL_POINTER_SIZE,
        )?;
        let cell_start = self.allocate_cell(page.id(), header_size, cell_size)?;
        page.write_body_u16(pointer_offset, cell_start)?;
        self.record_count = self
            .record_count
            .checked_add(1)
            .expect("record count is bounded by the page size");
        Ok(usize::from(cell_start))
    }
}

#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct LeafPageHeader {
    cells: CellArea,
    right_sibling: PageId,
    left_sibling: PageId,
}

#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct InternalPageHeader {
    cells: CellArea,
    right_child: PageId,
}

/// A validated, read-only view of a B-tree page.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Node<'a> {
    Leaf(LeafNode<'a>),
    Internal(InternalNode<'a>),
}

impl<'a> Node<'a> {
    /// Interprets `page` as a B-tree node. Meta pages and unknown type bytes are rejected here, so
    /// B-tree code only ever matches on two kinds.
    pub(crate) fn from_page(page: &'a Page) -> Result<Self> {
        match page.page_type()? {
            PageType::Leaf => Ok(Self::Leaf(LeafNode::with_header(page)?)),
            PageType::Internal => Ok(Self::Internal(InternalNode::with_header(page)?)),
            PageType::Meta => Err(ShuError::InvalidPageType),
        }
    }

    pub(crate) fn kind(&self) -> NodeKind {
        match self {
            Self::Leaf(_) => NodeKind::Leaf,
            Self::Internal(_) => NodeKind::Internal,
        }
    }
}

/// Read-only leaf view. The header is read and validated once, at construction.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LeafNode<'a> {
    page: &'a Page,
    header: LeafPageHeader,
}

impl<'a> LeafNode<'a> {
    pub(crate) fn from_page(page: &'a Page) -> Result<Self> {
        expect_page_type(page, PageType::Leaf)?;
        Self::with_header(page)
    }

    fn with_header(page: &'a Page) -> Result<Self> {
        let header = page.read_body_prefix::<LeafPageHeader>()?;
        header.cells.validate(page.id(), LEAF_HEADER_SIZE)?;
        Ok(Self { page, header })
    }

    pub(crate) fn len(&self) -> u16 {
        self.header.cells.record_count
    }

    pub(crate) fn entry(&self, index: u16) -> Result<LeafEntryRef<'a>> {
        let cell_start = self
            .header
            .cells
            .cell_start(self.page, LEAF_HEADER_SIZE, index)?;
        read_leaf_cell(self.page, cell_start)
    }

    pub(crate) fn search(&self, key: &[u8]) -> Result<LeafSearchResult> {
        let mut low = 0;
        let mut high = self.len();
        while low < high {
            let mid = low + (high - low) / 2;
            match self.entry(mid)?.key.cmp(key) {
                Ordering::Equal => return Ok(LeafSearchResult::Found(mid)),
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
            }
        }
        Ok(LeafSearchResult::Missing(low))
    }

    pub(crate) fn last_key(&self) -> Result<Option<&'a [u8]>> {
        match self.len().checked_sub(1) {
            Some(index) => Ok(Some(self.entry(index)?.key)),
            None => Ok(None),
        }
    }

    /// Copies every entry out of the page. Allocates two buffers per entry, so keep it off the
    /// per-operation path.
    pub(crate) fn entries(&self) -> Result<Vec<LeafEntry>> {
        let mut entries = Vec::with_capacity(usize::from(self.len()));
        for index in 0..self.len() {
            let entry = self.entry(index)?;
            entries.push(LeafEntry::new(entry.key, entry.value));
        }
        Ok(entries)
    }
}

/// Mutable leaf view. Keeps a copy of the header and writes it back after each change.
#[derive(Debug)]
pub(crate) struct LeafNodeMut<'a> {
    page: &'a mut Page,
    header: LeafPageHeader,
}

impl<'a> LeafNodeMut<'a> {
    pub(crate) fn from_page(page: &'a mut Page) -> Result<Self> {
        let header = LeafNode::from_page(page)?.header;
        Ok(Self { page, header })
    }

    /// Formats `page` as an empty leaf, discarding its previous contents.
    pub(crate) fn init(page: &'a mut Page) -> Result<Self> {
        page.set_page_type(PageType::Leaf);
        let mut node = Self {
            page,
            header: LeafPageHeader {
                cells: CellArea::EMPTY,
                right_sibling: PageId::new(0),
                left_sibling: PageId::new(0),
            },
        };
        node.write_header()?;
        Ok(node)
    }

    pub(crate) fn as_node(&self) -> LeafNode<'_> {
        LeafNode {
            page: self.page,
            header: self.header,
        }
    }

    /// Inserts or replaces `key` in place. Returns `NoSpace` without touching the page when the
    /// contiguous free space is too small.
    pub(crate) fn insert(&mut self, key: &[u8], value: &[u8]) -> Result<LeafInsert> {
        checked_leaf_entry_lens(self.page.id(), key.len(), value.len())?;
        match self.as_node().search(key)? {
            LeafSearchResult::Found(index) => self.replace(index, key, value),
            LeafSearchResult::Missing(index) => {
                let cell_size = leaf_cell_size(key.len(), value.len());
                if cell_size + CELL_POINTER_SIZE > self.free_space() {
                    return Ok(LeafInsert::NoSpace);
                }
                let cell_start =
                    self.header
                        .cells
                        .insert_cell(self.page, LEAF_HEADER_SIZE, index, cell_size)?;
                write_leaf_cell(self.page, cell_start, key, value)?;
                self.write_header()?;
                Ok(LeafInsert::Inserted)
            }
        }
    }

    /// Replaces the value at `index`. A same-length value is overwritten in place; otherwise a new
    /// cell is written and the old one becomes a hole until the next rewrite.
    fn replace(&mut self, index: u16, key: &[u8], value: &[u8]) -> Result<LeafInsert> {
        let cell_start = self
            .header
            .cells
            .cell_start(self.page, LEAF_HEADER_SIZE, index)?;
        let old_value_len = read_leaf_cell(self.page, cell_start)?.value.len();
        if old_value_len == value.len() {
            let value_start = cell_start + LEAF_CELL_HEADER_SIZE + key.len();
            self.page
                .write_body_bytes(value_start..value_start + value.len(), value)?;
            return Ok(LeafInsert::Inserted);
        }

        let cell_size = leaf_cell_size(key.len(), value.len());
        if cell_size > self.free_space() {
            return Ok(LeafInsert::NoSpace);
        }
        let new_cell_start =
            self.header
                .cells
                .allocate_cell(self.page.id(), LEAF_HEADER_SIZE, cell_size)?;
        write_leaf_cell(self.page, usize::from(new_cell_start), key, value)?;
        let pointer_offset = LEAF_HEADER_SIZE + usize::from(index) * CELL_POINTER_SIZE;
        self.page.write_body_u16(pointer_offset, new_cell_start)?;
        self.write_header()?;
        Ok(LeafInsert::Inserted)
    }

    /// Replaces the page contents with `entries`, which must be sorted by unique key. Reclaims any
    /// holes left by earlier replacements.
    pub(crate) fn rewrite(&mut self, entries: &[LeafEntry]) -> Result<()> {
        if entries.windows(2).any(|pair| pair[0].key >= pair[1].key) {
            return Err(ShuError::CorruptedPage {
                page_id: self.page.id(),
            });
        }
        let needed: usize = entries.iter().map(LeafEntry::space).sum();
        if needed > LEAF_ENTRIES_CAPACITY {
            return Err(page_full(self.page.id(), needed, LEAF_ENTRIES_CAPACITY));
        }

        self.header.cells = CellArea::EMPTY;
        for entry in entries {
            let index = self.header.cells.record_count;
            let cell_size = leaf_cell_size(entry.key.len(), entry.value.len());
            let cell_start =
                self.header
                    .cells
                    .insert_cell(self.page, LEAF_HEADER_SIZE, index, cell_size)?;
            write_leaf_cell(self.page, cell_start, &entry.key, &entry.value)?;
        }
        self.write_header()
    }

    fn free_space(&self) -> usize {
        self.header.cells.free_space(LEAF_HEADER_SIZE)
    }

    fn write_header(&mut self) -> Result<()> {
        self.page.write_body_prefix(&self.header)
    }
}

/// Read-only internal page view. The header is read and validated once, at construction.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InternalNode<'a> {
    page: &'a Page,
    header: InternalPageHeader,
}

impl<'a> InternalNode<'a> {
    pub(crate) fn from_page(page: &'a Page) -> Result<Self> {
        expect_page_type(page, PageType::Internal)?;
        Self::with_header(page)
    }

    fn with_header(page: &'a Page) -> Result<Self> {
        let header = page.read_body_prefix::<InternalPageHeader>()?;
        header.cells.validate(page.id(), INTERNAL_HEADER_SIZE)?;
        Ok(Self { page, header })
    }

    /// Number of separator entries. The node has `len() + 1` children.
    pub(crate) fn len(&self) -> u16 {
        self.header.cells.record_count
    }

    pub(crate) fn entry(&self, index: u16) -> Result<InternalEntryRef<'a>> {
        let cell_start = self
            .header
            .cells
            .cell_start(self.page, INTERNAL_HEADER_SIZE, index)?;
        let child_end = cell_start + size_of::<PageId>();
        let child = bytemuck::pod_read_unaligned(self.page.read_body_bytes(cell_start..child_end)?);
        let key_len = usize::from(self.page.read_body_u16(child_end)?);
        let key_start = cell_start + INTERNAL_CELL_HEADER_SIZE;
        let separator = self.page.read_body_bytes(key_start..key_start + key_len)?;
        Ok(InternalEntryRef { child, separator })
    }

    /// Returns the index of the child that may hold `key`: the first separator `>= key`, or
    /// `len()` for the right child.
    pub(crate) fn child_index_for_key(&self, key: &[u8]) -> Result<u16> {
        let mut low = 0;
        let mut high = self.len();
        while low < high {
            let mid = low + (high - low) / 2;
            if key <= self.entry(mid)?.separator {
                high = mid;
            } else {
                low = mid + 1;
            }
        }
        Ok(low)
    }

    pub(crate) fn child_at(&self, child_index: u16) -> Result<PageId> {
        match child_index.cmp(&self.len()) {
            Ordering::Less => Ok(self.entry(child_index)?.child),
            Ordering::Equal => Ok(self.header.right_child),
            Ordering::Greater => Err(ShuError::IndexOutOfRange),
        }
    }

    /// Copies every entry out of the page. Allocates one buffer per entry, so keep it off the
    /// per-operation path.
    pub(crate) fn entries(&self) -> Result<InternalEntries> {
        let mut entries = Vec::with_capacity(usize::from(self.len()));
        for index in 0..self.len() {
            let entry = self.entry(index)?;
            entries.push(InternalEntry::new(entry.child, entry.separator));
        }
        Ok(InternalEntries::new(entries, self.header.right_child))
    }
}

/// Mutable internal page view. Keeps a copy of the header and writes it back after each change.
#[derive(Debug)]
pub(crate) struct InternalNodeMut<'a> {
    page: &'a mut Page,
    header: InternalPageHeader,
}

impl<'a> InternalNodeMut<'a> {
    pub(crate) fn from_page(page: &'a mut Page) -> Result<Self> {
        let header = InternalNode::from_page(page)?.header;
        Ok(Self { page, header })
    }

    /// Formats `page` as an empty internal page, discarding its previous contents. The right
    /// child starts as page 0 and must be set before the node is used for lookups.
    pub(crate) fn init(page: &'a mut Page) -> Result<Self> {
        page.set_page_type(PageType::Internal);
        let mut node = Self {
            page,
            header: InternalPageHeader {
                cells: CellArea::EMPTY,
                right_child: PageId::new(0),
            },
        };
        node.write_header()?;
        Ok(node)
    }

    pub(crate) fn as_node(&self) -> InternalNode<'_> {
        InternalNode {
            page: self.page,
            header: self.header,
        }
    }

    /// Contiguous free bytes. Internal pages never leave holes, so this is all unused space.
    pub(crate) fn free_space(&self) -> usize {
        self.header.cells.free_space(INTERNAL_HEADER_SIZE)
    }

    /// Inserts `entry` at `index`, shifting later entries right. The caller keeps separators
    /// ordered.
    pub(crate) fn insert(&mut self, index: u16, entry: &InternalEntry) -> Result<()> {
        let key_len = checked_internal_key_len(self.page.id(), entry.separator.len())?;
        let cell_size = INTERNAL_CELL_HEADER_SIZE + usize::from(key_len);
        let cell_start =
            self.header
                .cells
                .insert_cell(self.page, INTERNAL_HEADER_SIZE, index, cell_size)?;
        let child_end = cell_start + size_of::<PageId>();
        self.page
            .write_body_bytes(cell_start..child_end, bytemuck::bytes_of(&entry.child))?;
        self.page.write_body_u16(child_end, key_len)?;
        let key_start = cell_start + INTERNAL_CELL_HEADER_SIZE;
        self.page.write_body_bytes(
            key_start..key_start + entry.separator.len(),
            &entry.separator,
        )?;
        self.write_header()
    }

    /// Points child slot `child_index` at `child`. Slot `len()` is the right child.
    pub(crate) fn set_child(&mut self, child_index: u16, child: PageId) -> Result<()> {
        match child_index.cmp(&self.header.cells.record_count) {
            Ordering::Less => {
                let cell_start =
                    self.header
                        .cells
                        .cell_start(self.page, INTERNAL_HEADER_SIZE, child_index)?;
                let child_end = cell_start + size_of::<PageId>();
                self.page
                    .write_body_bytes(cell_start..child_end, bytemuck::bytes_of(&child))
            }
            Ordering::Equal => {
                self.header.right_child = child;
                self.write_header()
            }
            Ordering::Greater => Err(ShuError::IndexOutOfRange),
        }
    }

    /// Replaces the page contents with `entries`.
    pub(crate) fn rewrite(&mut self, entries: &InternalEntries) -> Result<()> {
        let needed = entries.space();
        if needed > INTERNAL_ENTRIES_CAPACITY {
            return Err(page_full(self.page.id(), needed, INTERNAL_ENTRIES_CAPACITY));
        }

        self.header.cells = CellArea::EMPTY;
        self.header.right_child = entries.right_child;
        for entry in &entries.entries {
            self.insert(self.header.cells.record_count, entry)?;
        }
        self.write_header()
    }

    fn write_header(&mut self) -> Result<()> {
        self.page.write_body_prefix(&self.header)
    }
}

fn expect_page_type(page: &Page, expected: PageType) -> Result<()> {
    if page.page_type()? != expected {
        return Err(ShuError::InvalidPageType);
    }
    Ok(())
}

fn read_leaf_cell(page: &Page, cell_start: usize) -> Result<LeafEntryRef<'_>> {
    let key_len = usize::from(page.read_body_u16(cell_start)?);
    let value_len = usize::from(page.read_body_u16(cell_start + size_of::<u16>())?);
    let key_start = cell_start + LEAF_CELL_HEADER_SIZE;
    let value_start = key_start + key_len;
    let key = page.read_body_bytes(key_start..value_start)?;
    let value = page.read_body_bytes(value_start..value_start + value_len)?;
    Ok(LeafEntryRef { key, value })
}

fn write_leaf_cell(page: &mut Page, cell_start: usize, key: &[u8], value: &[u8]) -> Result<()> {
    let (key_len, value_len) = checked_leaf_entry_lens(page.id(), key.len(), value.len())?;
    let value_len_start = cell_start + size_of::<u16>();
    let key_start = cell_start + LEAF_CELL_HEADER_SIZE;
    let value_start = key_start + key.len();
    page.write_body_u16(cell_start, key_len)?;
    page.write_body_u16(value_len_start, value_len)?;
    page.write_body_bytes(key_start..value_start, key)?;
    page.write_body_bytes(value_start..value_start + value.len(), value)
}

fn leaf_cell_size(key_len: usize, value_len: usize) -> usize {
    LEAF_CELL_HEADER_SIZE + key_len + value_len
}

fn page_full(page_id: PageId, needed: usize, available: usize) -> ShuError {
    ShuError::PageFull {
        page_id,
        needed,
        available,
    }
}

fn checked_leaf_entry_lens(
    page_id: PageId,
    key_len: usize,
    value_len: usize,
) -> Result<(u16, u16)> {
    match (u16::try_from(key_len), u16::try_from(value_len)) {
        (Ok(key_len), Ok(value_len)) => Ok((key_len, value_len)),
        _ => Err(ShuError::CellTooLarge {
            page_id,
            key_len,
            value_len,
        }),
    }
}

fn checked_internal_key_len(page_id: PageId, key_len: usize) -> Result<u16> {
    u16::try_from(key_len).map_err(|_| ShuError::CellTooLarge {
        page_id,
        key_len,
        value_len: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_leaf() -> Page {
        let mut page = Page::new(PageType::Leaf, PageId::new(1));
        LeafNodeMut::init(&mut page).unwrap();
        page
    }

    fn empty_internal() -> Page {
        let mut page = Page::new(PageType::Internal, PageId::new(2));
        InternalNodeMut::init(&mut page).unwrap();
        page
    }

    fn leaf_keys(page: &Page) -> Vec<Vec<u8>> {
        let entries = LeafNode::from_page(page).unwrap().entries().unwrap();
        entries.into_iter().map(|entry| entry.key).collect()
    }

    #[test]
    fn node_from_page_rejects_meta_pages() {
        let page = Page::new(PageType::Meta, PageId::new(0));

        assert!(matches!(
            Node::from_page(&page),
            Err(ShuError::InvalidPageType)
        ));
    }

    #[test]
    fn typed_views_reject_the_other_kind() {
        let mut leaf = empty_leaf();
        let mut internal = empty_internal();

        assert!(matches!(
            InternalNode::from_page(&leaf),
            Err(ShuError::InvalidPageType)
        ));
        assert!(matches!(
            InternalNodeMut::from_page(&mut leaf),
            Err(ShuError::InvalidPageType)
        ));
        assert!(matches!(
            LeafNode::from_page(&internal),
            Err(ShuError::InvalidPageType)
        ));
        assert!(matches!(
            LeafNodeMut::from_page(&mut internal),
            Err(ShuError::InvalidPageType)
        ));
    }

    #[test]
    fn view_rejects_header_with_pointers_past_content_start() {
        let mut page = empty_leaf();
        let corrupt = CellArea {
            record_count: 4000,
            content_start: BODY_SIZE as u16,
        };
        page.write_body_prefix(&corrupt).unwrap();

        assert!(matches!(
            LeafNode::from_page(&page),
            Err(ShuError::CorruptedPage { .. })
        ));
    }

    #[test]
    fn leaf_entry_rejects_index_past_record_count() {
        let page = empty_leaf();

        let result = LeafNode::from_page(&page).unwrap().entry(0);

        assert!(matches!(result, Err(ShuError::IndexOutOfRange)));
    }

    #[test]
    fn leaf_insert_keeps_entries_sorted_by_key() {
        let mut page = empty_leaf();
        let mut leaf = LeafNodeMut::from_page(&mut page).unwrap();

        for key in [&b"m"[..], b"z", b"a", b"q"] {
            assert_eq!(leaf.insert(key, key).unwrap(), LeafInsert::Inserted);
        }

        assert_eq!(leaf_keys(&page), [b"a", b"m", b"q", b"z"]);
        let entry = LeafNode::from_page(&page).unwrap().entry(1).unwrap();
        assert_eq!(entry.key, b"m");
        assert_eq!(entry.value, b"m");
    }

    #[test]
    fn leaf_search_finds_keys_and_insert_positions() {
        let mut page = empty_leaf();
        let mut leaf = LeafNodeMut::from_page(&mut page).unwrap();
        for key in [&b"a"[..], b"c", b"z"] {
            assert_eq!(leaf.insert(key, b"v").unwrap(), LeafInsert::Inserted);
        }
        let leaf = LeafNode::from_page(&page).unwrap();

        assert_eq!(leaf.search(b"a").unwrap(), LeafSearchResult::Found(0));
        assert_eq!(leaf.search(b"c").unwrap(), LeafSearchResult::Found(1));
        assert_eq!(leaf.search(b"z").unwrap(), LeafSearchResult::Found(2));
        assert_eq!(leaf.search(b"0").unwrap(), LeafSearchResult::Missing(0));
        assert_eq!(leaf.search(b"b").unwrap(), LeafSearchResult::Missing(1));
        assert_eq!(leaf.search(b"d").unwrap(), LeafSearchResult::Missing(2));
        assert_eq!(leaf.search(b"zz").unwrap(), LeafSearchResult::Missing(3));
    }

    #[test]
    fn leaf_search_on_empty_leaf_is_missing_at_zero() {
        let page = empty_leaf();

        let result = LeafNode::from_page(&page).unwrap().search(b"a").unwrap();

        assert_eq!(result, LeafSearchResult::Missing(0));
    }

    #[test]
    fn leaf_replace_with_same_length_overwrites_in_place() {
        let mut page = empty_leaf();
        let mut leaf = LeafNodeMut::from_page(&mut page).unwrap();
        assert_eq!(leaf.insert(b"k", b"old").unwrap(), LeafInsert::Inserted);
        let free_before = leaf.free_space();

        assert_eq!(leaf.insert(b"k", b"new").unwrap(), LeafInsert::Inserted);

        assert_eq!(leaf.free_space(), free_before);
        let entry = leaf.as_node().entry(0).unwrap();
        assert_eq!(entry.value, b"new");
        assert_eq!(leaf.as_node().len(), 1);
    }

    #[test]
    fn leaf_replace_with_new_length_moves_the_cell() {
        let mut page = empty_leaf();
        let mut leaf = LeafNodeMut::from_page(&mut page).unwrap();
        assert_eq!(leaf.insert(b"a", b"1").unwrap(), LeafInsert::Inserted);
        assert_eq!(leaf.insert(b"b", b"2").unwrap(), LeafInsert::Inserted);

        assert_eq!(leaf.insert(b"a", b"longer").unwrap(), LeafInsert::Inserted);

        let node = leaf.as_node();
        assert_eq!(node.len(), 2);
        assert_eq!(node.entry(0).unwrap().value, b"longer");
        assert_eq!(node.entry(1).unwrap().value, b"2");
    }

    #[test]
    fn leaf_insert_reports_no_space_and_leaves_page_unchanged() {
        let mut page = empty_leaf();
        let mut leaf = LeafNodeMut::from_page(&mut page).unwrap();
        let value = vec![7; 2000];
        assert_eq!(leaf.insert(b"a", &value).unwrap(), LeafInsert::Inserted);
        assert_eq!(leaf.insert(b"b", &value).unwrap(), LeafInsert::Inserted);
        let before = page.clone();
        let mut leaf = LeafNodeMut::from_page(&mut page).unwrap();

        assert_eq!(leaf.insert(b"c", &value).unwrap(), LeafInsert::NoSpace);
        assert_eq!(leaf.insert(b"a", &[1; 2001]).unwrap(), LeafInsert::NoSpace);

        assert_eq!(page.as_bytes(), before.as_bytes());
    }

    #[test]
    fn leaf_insert_rejects_key_longer_than_u16() {
        let mut page = empty_leaf();
        let mut leaf = LeafNodeMut::from_page(&mut page).unwrap();
        let key = vec![0; usize::from(u16::MAX) + 1];

        let result = leaf.insert(&key, b"v");

        assert!(matches!(result, Err(ShuError::CellTooLarge { .. })));
    }

    #[test]
    fn leaf_insert_fills_page_to_exact_capacity() {
        let mut page = empty_leaf();
        let mut leaf = LeafNodeMut::from_page(&mut page).unwrap();
        let value_len = LEAF_ENTRIES_CAPACITY - leaf_entry_space(1, 0);

        let first = leaf.insert(b"k", &vec![1; value_len]).unwrap();
        let second = leaf.insert(b"l", b"").unwrap();

        assert_eq!(first, LeafInsert::Inserted);
        assert_eq!(leaf.free_space(), 0);
        assert_eq!(second, LeafInsert::NoSpace);
    }

    #[test]
    fn leaf_rewrite_reclaims_holes() {
        let mut page = empty_leaf();
        let mut leaf = LeafNodeMut::from_page(&mut page).unwrap();
        assert_eq!(leaf.insert(b"a", &[1; 1000]).unwrap(), LeafInsert::Inserted);
        assert_eq!(leaf.insert(b"a", &[2; 1001]).unwrap(), LeafInsert::Inserted);
        let free_with_hole = leaf.free_space();

        let entries = leaf.as_node().entries().unwrap();
        leaf.rewrite(&entries).unwrap();

        assert_eq!(leaf.free_space(), free_with_hole + leaf_cell_size(1, 1000));
        assert_eq!(leaf.as_node().entry(0).unwrap().value, &[2; 1001][..]);
    }

    #[test]
    fn leaf_rewrite_rejects_unsorted_or_duplicate_keys() {
        let mut page = empty_leaf();
        let mut leaf = LeafNodeMut::from_page(&mut page).unwrap();

        let unsorted = [LeafEntry::new(b"b", b""), LeafEntry::new(b"a", b"")];
        let duplicate = [LeafEntry::new(b"a", b""), LeafEntry::new(b"a", b"")];

        assert!(matches!(
            leaf.rewrite(&unsorted),
            Err(ShuError::CorruptedPage { .. })
        ));
        assert!(matches!(
            leaf.rewrite(&duplicate),
            Err(ShuError::CorruptedPage { .. })
        ));
    }

    #[test]
    fn leaf_rewrite_rejects_entries_over_capacity() {
        let mut page = empty_leaf();
        let mut leaf = LeafNodeMut::from_page(&mut page).unwrap();
        let entries = [
            LeafEntry::new(b"a", vec![0; 2100]),
            LeafEntry::new(b"b", vec![0; 2100]),
        ];

        assert!(matches!(
            leaf.rewrite(&entries),
            Err(ShuError::PageFull { .. })
        ));
    }

    #[test]
    fn internal_init_writes_empty_entries() {
        let page = empty_internal();

        let entries = InternalNode::from_page(&page).unwrap().entries().unwrap();

        assert!(entries.entries.is_empty());
        assert_eq!(entries.right_child, PageId::new(0));
    }

    #[test]
    fn internal_insert_writes_child_and_separator() {
        let mut page = empty_internal();
        let mut node = InternalNodeMut::from_page(&mut page).unwrap();

        node.insert(0, &InternalEntry::new(PageId::new(8), b"dog"))
            .unwrap();
        node.insert(0, &InternalEntry::new(PageId::new(7), b"cat"))
            .unwrap();
        node.insert(2, &InternalEntry::new(PageId::new(9), b""))
            .unwrap();

        let node = node.as_node();
        assert_eq!(node.len(), 3);
        let expected = [(7, &b"cat"[..]), (8, b"dog"), (9, b"")];
        for (index, (child, separator)) in (0..).zip(expected) {
            let entry = node.entry(index).unwrap();
            assert_eq!(entry.child, PageId::new(child));
            assert_eq!(entry.separator, separator);
        }
    }

    #[test]
    fn internal_insert_rejects_index_past_record_count() {
        let mut page = empty_internal();
        let mut node = InternalNodeMut::from_page(&mut page).unwrap();

        let result = node.insert(1, &InternalEntry::new(PageId::new(10), b"cat"));

        assert!(matches!(result, Err(ShuError::IndexOutOfRange)));
    }

    #[test]
    fn internal_insert_rejects_entry_larger_than_free_space() {
        let mut page = empty_internal();
        let mut node = InternalNodeMut::from_page(&mut page).unwrap();
        let separator = vec![0; INTERNAL_ENTRIES_CAPACITY];

        let result = node.insert(0, &InternalEntry::new(PageId::new(10), separator));

        assert!(matches!(result, Err(ShuError::PageFull { .. })));
        assert_eq!(node.as_node().len(), 0);
    }

    #[test]
    fn internal_set_child_updates_entries_and_right_child() {
        let mut page = empty_internal();
        let mut node = InternalNodeMut::from_page(&mut page).unwrap();
        node.insert(0, &InternalEntry::new(PageId::new(7), b"cat"))
            .unwrap();

        node.set_child(0, PageId::new(70)).unwrap();
        node.set_child(1, PageId::new(71)).unwrap();

        let node = node.as_node();
        assert_eq!(node.child_at(0).unwrap(), PageId::new(70));
        assert_eq!(node.child_at(1).unwrap(), PageId::new(71));
        assert_eq!(node.entry(0).unwrap().separator, b"cat");
    }

    #[test]
    fn internal_set_child_and_child_at_reject_slots_past_right_child() {
        let mut page = empty_internal();
        let mut node = InternalNodeMut::from_page(&mut page).unwrap();

        assert!(matches!(
            node.set_child(1, PageId::new(3)),
            Err(ShuError::IndexOutOfRange)
        ));
        assert!(matches!(
            node.as_node().child_at(1),
            Err(ShuError::IndexOutOfRange)
        ));
    }

    #[test]
    fn internal_rewrite_replaces_entries_and_right_child() {
        let mut page = empty_internal();
        let mut node = InternalNodeMut::from_page(&mut page).unwrap();
        node.insert(0, &InternalEntry::new(PageId::new(99), b"stale"))
            .unwrap();
        let entries = InternalEntries::new(
            vec![
                InternalEntry::new(PageId::new(10), b"cat"),
                InternalEntry::new(PageId::new(11), b"dog"),
            ],
            PageId::new(12),
        );

        node.rewrite(&entries).unwrap();

        assert_eq!(node.as_node().entries().unwrap(), entries);
    }

    #[test]
    fn child_index_for_key_routes_separator_boundaries() {
        let mut page = empty_internal();
        let mut node = InternalNodeMut::from_page(&mut page).unwrap();
        let entries = InternalEntries::new(
            vec![
                InternalEntry::new(PageId::new(10), b"cat"),
                InternalEntry::new(PageId::new(11), b"dog"),
                InternalEntry::new(PageId::new(12), b"fox"),
            ],
            PageId::new(13),
        );
        node.rewrite(&entries).unwrap();
        let node = InternalNode::from_page(&page).unwrap();
        let child_for = |key: &[u8]| {
            let index = node.child_index_for_key(key).unwrap();
            node.child_at(index).unwrap()
        };

        assert_eq!(child_for(b"ant"), PageId::new(10));
        assert_eq!(child_for(b"cat"), PageId::new(10));
        assert_eq!(child_for(b"cow"), PageId::new(11));
        assert_eq!(child_for(b"dog"), PageId::new(11));
        assert_eq!(child_for(b"elk"), PageId::new(12));
        assert_eq!(child_for(b"fox"), PageId::new(12));
        assert_eq!(child_for(b"zoo"), PageId::new(13));
    }

    #[test]
    fn child_index_for_key_uses_right_child_for_empty_internal_page() {
        let mut page = empty_internal();
        InternalNodeMut::from_page(&mut page)
            .unwrap()
            .set_child(0, PageId::new(9))
            .unwrap();
        let node = InternalNode::from_page(&page).unwrap();

        let index = node.child_index_for_key(b"cat").unwrap();

        assert_eq!(index, 0);
        assert_eq!(node.child_at(index).unwrap(), PageId::new(9));
    }
}
