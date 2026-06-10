use std::mem::size_of;

use crate::{
    error::{Result, ShuError},
    storage::page::{
        HEADER_SIZE, OverflowInternalEntries, OverflowInternalEntry, OverflowLeafEntry, PAGE_SIZE,
        Page, PageId, PageOverflow, PageType,
    },
};

const CELL_POINTER_SIZE: usize = size_of::<u16>();
const LEAF_CELL_HEADER_SIZE: usize = 2 * size_of::<u16>();
const INTERNAL_CELL_HEADER_SIZE: usize = size_of::<PageId>() + size_of::<u16>();

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

    /// Returns the size of key value
    pub(crate) fn size(&self) -> usize {
        self.key.len() + self.value.len()
    }
}

pub(crate) fn leaf_entries_capacity() -> usize {
    PAGE_SIZE - HEADER_SIZE - size_of::<LeafPageHeader>()
}

pub(crate) fn leaf_entry_space(entry: &LeafEntry) -> usize {
    CELL_POINTER_SIZE + LEAF_CELL_HEADER_SIZE + entry.size()
}

pub(crate) struct LeafEntryRef<'a> {
    pub key: &'a [u8],
    pub value: &'a [u8],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InternalEntry {
    pub child: PageId,
    pub separator: Vec<u8>,
}

pub(crate) fn internal_entries_capacity() -> usize {
    PAGE_SIZE - HEADER_SIZE - size_of::<InternalPageHeader>()
}

pub(crate) fn internal_entry_space(entry: &InternalEntry) -> usize {
    CELL_POINTER_SIZE + INTERNAL_CELL_HEADER_SIZE + entry.separator.len()
}

impl InternalEntry {
    pub(crate) fn new(child: PageId, separator: impl Into<Vec<u8>>) -> Self {
        Self {
            child,
            separator: separator.into(),
        }
    }
}

pub(crate) struct InternalEntryRef<'a> {
    pub child: PageId,
    pub separator: &'a [u8],
}

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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeafSearchResult {
    Found(u16),
    Missing(u16),
}

#[derive(Clone, Copy, Debug)]
struct LeafInsertPosition {
    index: u16,
}

impl LeafInsertPosition {
    fn pointer_offset(self) -> usize {
        leaf_cell_pointer_offset(self.index)
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

pub(crate) trait BTreePage {
    fn is_underflow(&self) -> Result<bool>;
    fn is_overflow(&self) -> Result<bool>;

    fn leaf_entry(&self, index: u16) -> Result<LeafEntryRef<'_>>;
    fn leaf_entries(&self) -> Result<Vec<LeafEntry>>;
    fn leaf_search(&self, key: &[u8]) -> Result<LeafSearchResult>;
    fn insert_leaf_entry(&mut self, key: &[u8], value: &[u8]) -> Result<()>;
    fn rewrite_leaf_entries(&mut self, entries: &[LeafEntry]) -> Result<()>;

    fn internal_entry(&self, index: u16) -> Result<InternalEntryRef<'_>>;
    fn internal_entries(&self) -> Result<InternalEntries>;
    fn append_internal_entry(&mut self, entry: &InternalEntry) -> Result<()>;
    #[cfg(test)]
    fn insert_internal_entry(&mut self, index: u16, entry: &InternalEntry) -> Result<()>;
    fn rewrite_internal_entries(&mut self, entries: &InternalEntries) -> Result<()>;
    fn child_index_for_key(&self, key: &[u8]) -> Result<u16>;
    fn child_at(&self, child_index: u16) -> Result<PageId>;
    fn set_right_child(&mut self, child: PageId) -> Result<()>;
}

impl BTreePage for Page {
    fn is_underflow(&self) -> Result<bool> {
        match self.page_type()? {
            PageType::Leaf => {
                if matches!(self.overflow, PageOverflow::Leaf(_)) {
                    return Ok(false);
                }

                let header = read_leaf_header(self)?;
                Ok(leaf_free_space(self.id(), &header)? > leaf_usable_space(self) / 2)
            }
            PageType::Internal => {
                if matches!(self.overflow, PageOverflow::Internal(_)) {
                    return Ok(false);
                }

                let header = read_internal_header(self)?;
                Ok(internal_free_space(self.id(), &header)? > internal_usable_space(self) / 2)
            }
            PageType::Meta => Err(ShuError::InvalidPageType),
        }
    }

    fn is_overflow(&self) -> Result<bool> {
        match (self.page_type()?, &self.overflow) {
            (_, PageOverflow::None) => Ok(false),
            (PageType::Leaf, PageOverflow::Leaf(entries)) => Ok(!entries.is_empty()),
            (PageType::Internal, PageOverflow::Internal(entries)) => {
                Ok(!entries.entries.is_empty())
            }
            (PageType::Meta, _) => Err(ShuError::InvalidPageType),
            _ => Err(ShuError::InvalidPageType),
        }
    }

    fn leaf_entry(&self, index: u16) -> Result<LeafEntryRef<'_>> {
        read_leaf_entry(self, index)
    }

    fn leaf_entries(&self) -> Result<Vec<LeafEntry>> {
        if let PageOverflow::Leaf(entries) = &self.overflow {
            return Ok(entries
                .iter()
                .map(|entry| LeafEntry::new(entry.key.as_slice(), entry.value.as_slice()))
                .collect());
        }

        let header = read_leaf_header(self)?;
        let mut entries = Vec::with_capacity(usize::from(header.record_count));

        for index in 0..header.record_count {
            let entry = self.leaf_entry(index)?;
            entries.push(LeafEntry::new(entry.key, entry.value));
        }

        Ok(entries)
    }

    fn leaf_search(&self, key: &[u8]) -> Result<LeafSearchResult> {
        self.assert_page_type(PageType::Leaf)?;
        if matches!(self.overflow, PageOverflow::Leaf(_)) {
            return match self
                .leaf_entries()?
                .binary_search_by(|entry| entry.key.as_slice().cmp(key))
            {
                Ok(index) => Ok(LeafSearchResult::Found(index as u16)),
                Err(index) => Ok(LeafSearchResult::Missing(index as u16)),
            };
        }

        let mut low = 0;
        let mut high = read_leaf_header(self)?.record_count;
        while low < high {
            let mid = (low + high) / 2;
            let entry_key = self.leaf_entry(mid)?.key;
            if entry_key == key {
                return Ok(LeafSearchResult::Found(mid));
            } else if key < entry_key {
                high = mid;
            } else {
                low = mid + 1;
            }
        }

        Ok(LeafSearchResult::Missing(low))
    }

    fn insert_leaf_entry(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        insert_into_leaf(self, key, value)
    }

    fn rewrite_leaf_entries(&mut self, entries: &[LeafEntry]) -> Result<()> {
        if entries.windows(2).any(|pair| pair[0].key > pair[1].key) {
            return Err(ShuError::CorruptedPage { page_id: self.id() });
        }
        if !leaf_entries_fit(entries) {
            return Err(page_full(
                self.id(),
                leaf_entries_space(entries),
                leaf_entries_capacity(),
            ));
        }

        self.overflow = PageOverflow::None;
        init_leaf_page(self)?;
        for entry in entries {
            append_leaf_entry(self, &entry.key, &entry.value)?;
        }

        Ok(())
    }

    fn internal_entry(&self, index: u16) -> Result<InternalEntryRef<'_>> {
        read_internal_entry(self, index)
    }

    fn internal_entries(&self) -> Result<InternalEntries> {
        if let PageOverflow::Internal(entries) = &self.overflow {
            return Ok(InternalEntries::new(
                entries
                    .entries
                    .iter()
                    .map(|entry| InternalEntry::new(entry.child, entry.separator.as_slice()))
                    .collect(),
                entries.right_child,
            ));
        }

        let header = read_internal_header(self)?;
        let mut entries = Vec::with_capacity(usize::from(header.record_count));

        for index in 0..header.record_count {
            let entry = self.internal_entry(index)?;
            entries.push(InternalEntry::new(entry.child, entry.separator));
        }

        Ok(InternalEntries::new(entries, header.right_child))
    }

    fn append_internal_entry(&mut self, entry: &InternalEntry) -> Result<()> {
        append_internal_cell(self, entry.child, &entry.separator)
    }

    #[cfg(test)]
    fn insert_internal_entry(&mut self, index: u16, entry: &InternalEntry) -> Result<()> {
        insert_internal_cell(self, index, entry.child, &entry.separator)
    }

    fn rewrite_internal_entries(&mut self, entries: &InternalEntries) -> Result<()> {
        if !internal_entries_fit(entries) {
            self.overflow = PageOverflow::Internal(OverflowInternalEntries::new(
                entries
                    .entries
                    .iter()
                    .map(|entry| {
                        OverflowInternalEntry::new(entry.child, entry.separator.as_slice())
                    })
                    .collect(),
                entries.right_child,
            ));
            return Ok(());
        }

        self.overflow = PageOverflow::None;
        init_internal_page(self)?;

        for entry in &entries.entries {
            self.append_internal_entry(entry)?;
        }

        self.set_right_child(entries.right_child)
    }

    fn child_index_for_key(&self, key: &[u8]) -> Result<u16> {
        self.assert_page_type(PageType::Internal)?;
        let mut low = 0;
        let mut high = read_internal_header(self)?.record_count;

        while low < high {
            let mid = low + (high - low) / 2;
            let separator = self.internal_entry(mid)?.separator;
            if key <= separator {
                high = mid
            } else {
                low = mid + 1
            }
        }

        Ok(low)
    }

    fn child_at(&self, child_index: u16) -> Result<PageId> {
        self.assert_page_type(PageType::Internal)?;
        let header = read_internal_header(self)?;

        if child_index < header.record_count {
            return Ok(self.internal_entry(child_index)?.child);
        }
        if child_index == header.record_count {
            return Ok(header.right_child);
        }

        Err(ShuError::IndexOutOfRange)
    }

    fn set_right_child(&mut self, child: PageId) -> Result<()> {
        let mut header = read_internal_header(self)?;
        header.right_child = child;
        write_internal_header(self, &header)
    }
}

pub(crate) fn init_leaf_page(page: &mut Page) -> Result<()> {
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

pub(crate) fn init_internal_page(page: &mut Page) -> Result<()> {
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

    leaf_free_space(page.id(), &header)?;
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

fn leaf_usable_space(page: &Page) -> usize {
    page.body().len() - size_of::<LeafPageHeader>()
}

fn internal_usable_space(page: &Page) -> usize {
    page.body().len() - size_of::<InternalPageHeader>()
}

fn validate_internal_header(page: &Page, header: &InternalPageHeader) -> Result<()> {
    let record_content_start = usize::from(header.record_content_start);
    if record_content_start > page.body().len() {
        return Err(ShuError::CorruptedPage { page_id: page.id() });
    }

    internal_free_space(page.id(), header)?;
    Ok(())
}

fn read_leaf_entry(page: &Page, index: u16) -> Result<LeafEntryRef<'_>> {
    page.assert_page_type(PageType::Leaf)?;
    let header = read_leaf_header(page)?;
    if index >= header.record_count {
        return Err(ShuError::IndexOutOfRange);
    }

    let pointer_offset = leaf_cell_pointer_offset(index);
    let cell_start = page.read_body_u16(pointer_offset)? as usize;
    let key_len = page.read_body_u16(cell_start)?;
    let value_len = page.read_body_u16(cell_start + size_of::<u16>())?;
    let layout = LeafCellLayout::new(cell_start, key_len, value_len);
    let key = page.read_body_bytes(layout.key_start..layout.key_end)?;
    let value = page.read_body_bytes(layout.value_start..layout.value_end)?;

    Ok(LeafEntryRef { key, value })
}

fn read_internal_entry(page: &Page, index: u16) -> Result<InternalEntryRef<'_>> {
    page.assert_page_type(PageType::Internal)?;
    let header = read_internal_header(page)?;
    if index >= header.record_count {
        return Err(ShuError::IndexOutOfRange);
    }

    let pointer_offset = internal_cell_pointer_offset(index);
    let cell_start = page.read_body_u16(pointer_offset)? as usize;
    let child = read_internal_cell_child_page_id(page, cell_start)?;
    let key_len = page.read_body_u16(cell_start + size_of::<PageId>())?;
    let layout = InternalCellLayout::new(cell_start, key_len);
    let separator = page.read_body_bytes(layout.key_start..layout.key_end)?;

    Ok(InternalEntryRef { child, separator })
}

fn insert_into_leaf(page: &mut Page, key: &[u8], value: &[u8]) -> Result<()> {
    page.assert_page_type(PageType::Leaf)?;
    let mut entries = page.leaf_entries()?;
    upsert_leaf_entry(&mut entries, key, value);

    if !leaf_entries_fit(&entries) {
        page.overflow = PageOverflow::Leaf(
            entries
                .into_iter()
                .map(|entry| OverflowLeafEntry::new(entry.key, entry.value))
                .collect(),
        );
        return Ok(());
    }

    page.rewrite_leaf_entries(&entries)
}

fn append_leaf_entry(page: &mut Page, key: &[u8], value: &[u8]) -> Result<()> {
    page.assert_page_type(PageType::Leaf)?;
    let mut header = read_leaf_header(page)?;
    let (key_len, value_len) = checked_leaf_entry_lens(page.id(), key.len(), value.len())?;
    let position = LeafInsertPosition {
        index: header.record_count,
    };
    let cell_start = allocate_leaf_cell_start(page.id(), &header, key_len, value_len)?;
    let layout = LeafCellLayout::new(cell_start, key_len, value_len);

    write_leaf_cell(page, position, &layout, key_len, value_len, key, value)?;

    header.record_count += 1;
    header.record_content_start = layout.cell_start as u16;
    page.write_body_prefix(&header)
}

fn append_internal_cell(page: &mut Page, child: PageId, key: &[u8]) -> Result<()> {
    page.assert_page_type(PageType::Internal)?;
    let mut header = read_internal_header(page)?;
    let key_len = checked_internal_key_len(page.id(), key.len())?;
    let cell_start = allocate_internal_cell_start(page.id(), &header, key_len)?;
    let layout = InternalCellLayout::new(cell_start, key_len);

    write_internal_cell(page, header.record_count, &layout, child, key_len, key)?;

    header.record_count += 1;
    header.record_content_start = layout.cell_start as u16;
    write_internal_header(page, &header)
}

#[cfg(test)]
fn insert_internal_cell(
    page: &mut Page,
    index: u16,
    child: PageId,
    separator: &[u8],
) -> Result<()> {
    page.assert_page_type(PageType::Internal)?;
    let mut header = read_internal_header(page)?;
    if index > header.record_count {
        return Err(ShuError::IndexOutOfRange);
    }

    let key_len = checked_internal_key_len(page.id(), separator.len())?;
    let cell_start = allocate_internal_cell_start(page.id(), &header, key_len)?;
    shift_internal_pointers(page, index, header.record_count)?;
    let layout = InternalCellLayout::new(cell_start, key_len);

    write_internal_cell(page, index, &layout, child, key_len, separator)?;

    header.record_count += 1;
    header.record_content_start = layout.cell_start as u16;
    write_internal_header(page, &header)
}

fn allocate_leaf_cell_start(
    page_id: PageId,
    header: &LeafPageHeader,
    key_len: u16,
    value_len: u16,
) -> Result<usize> {
    let cell_size = leaf_cell_size(key_len, value_len);
    usize::from(header.record_content_start)
        .checked_sub(cell_size)
        .ok_or_else(|| page_full(page_id, cell_size, 0))
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

    usize::from(header.record_content_start)
        .checked_sub(cell_size)
        .ok_or_else(|| page_full(page_id, cell_size, 0))
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
    child: PageId,
    key_len: u16,
    key: &[u8],
) -> Result<()> {
    page.write_body_u16(
        internal_cell_pointer_offset(index),
        layout.cell_start as u16,
    )?;
    page.write_body_bytes(
        layout.child_page_id_start..layout.key_len_start,
        bytemuck::bytes_of(&child),
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

#[cfg(test)]
fn shift_internal_pointers(page: &mut Page, start_index: u16, record_count: u16) -> Result<()> {
    let start = internal_cell_pointer_offset(start_index);
    let end = internal_cell_pointer_offset(record_count);
    let dst_start = start + CELL_POINTER_SIZE;
    page.copy_body_within(start..end, dst_start)
}

fn leaf_entries_space(entries: &[LeafEntry]) -> usize {
    entries.iter().map(leaf_entry_space).sum()
}

fn leaf_entries_fit(entries: &[LeafEntry]) -> bool {
    leaf_entries_space(entries) <= leaf_entries_capacity()
}

fn internal_entries_space(entries: &InternalEntries) -> usize {
    entries.entries.iter().map(internal_entry_space).sum()
}

fn internal_entries_fit(entries: &InternalEntries) -> bool {
    internal_entries_space(entries) <= internal_entries_capacity()
}

fn upsert_leaf_entry(entries: &mut Vec<LeafEntry>, key: &[u8], value: &[u8]) {
    match entries.binary_search_by(|entry| entry.key.as_slice().cmp(key)) {
        Ok(index) => entries[index].value = value.to_owned(),
        Err(index) => entries.insert(index, LeafEntry::new(key, value)),
    }
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
