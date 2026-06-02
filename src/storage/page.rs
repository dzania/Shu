use std::{fmt, ops::Range};

use bytemuck::{from_bytes, from_bytes_mut};

use crate::error::{Result, ShuError};

pub const PAGE_SIZE: usize = 4096;
pub const HEADER_SIZE: usize = 16;

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
#[repr(transparent)]
pub struct PageId(u32);

impl PageId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }

    pub const fn as_u64(self) -> u64 {
        self.0 as u64
    }
}

impl fmt::Display for PageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PageType {
    /// FIXME: Remove meta page
    Meta = 1,
    Internal = 2,
    Leaf = 3,
}

impl TryFrom<u8> for PageType {
    type Error = ShuError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Meta),
            2 => Ok(Self::Internal),
            3 => Ok(Self::Leaf),
            _ => Err(ShuError::InvalidPageType),
        }
    }
}

impl From<PageType> for u8 {
    fn from(value: PageType) -> Self {
        value as u8
    }
}

#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct PageHeader {
    pub page_id: PageId,     // 4 bytes
    pub page_type: u8,       // 1 byte
    pub _reserved: [u8; 3],  // 3 bytes padding
    pub checksum: u32,       // 4 bytes
    pub _reserved2: [u8; 4], // 4 bytes
} // total: 16 byte

// Lowest level page frame
// TODO: Add domain enum so we don't need to assert PageType
#[repr(C, align(4))]
pub struct Page {
    pub data: [u8; PAGE_SIZE],
}

impl Page {
    pub fn new(page_type: PageType, page_id: PageId) -> Self {
        let mut page = Page {
            data: [0_u8; PAGE_SIZE],
        };
        page.header_mut().page_type = page_type as u8;
        page.header_mut().page_id = page_id;
        page
    }
    pub fn header(&self) -> &PageHeader {
        from_bytes(&self.data[..HEADER_SIZE])
    }

    pub fn header_mut(&mut self) -> &mut PageHeader {
        from_bytes_mut(&mut self.data[..HEADER_SIZE])
    }

    pub fn id(&self) -> PageId {
        self.header().page_id
    }

    pub fn page_type(&self) -> Result<PageType> {
        self.header().page_type.try_into()
    }

    pub fn assert_page_type(&self, expected: PageType) -> Result<()> {
        if self.page_type()? != expected {
            return Err(ShuError::InvalidPageType);
        }
        Ok(())
    }

    pub fn body(&self) -> &[u8] {
        &self.data[HEADER_SIZE..]
    }

    pub fn body_mut(&mut self) -> &mut [u8] {
        &mut self.data[HEADER_SIZE..]
    }

    pub fn write_body_u16(&mut self, offset: usize, value: u16) -> Result<()> {
        let end = offset
            .checked_add(size_of::<u16>())
            .ok_or_else(|| self.invalid_body_range(offset, usize::MAX))?;
        self.write_body_bytes(offset..end, &value.to_le_bytes())
    }

    pub fn copy_body_within(&mut self, range: Range<usize>, dst_start: usize) -> Result<()> {
        self.validate_body_range(&range)?;
        if range
            .len()
            .checked_add(dst_start)
            .ok_or_else(|| self.invalid_body_range(range.start, range.end))?
            > self.body().len()
        {
            return Err(ShuError::BodyWriteLengthMismatch {
                page_id: self.id(),
                range_len: range.len(),
                bytes_len: dst_start,
            });
        }
        self.body_mut().copy_within(range, dst_start);
        Ok(())
    }

    pub fn write_body_bytes(&mut self, range: Range<usize>, bytes: &[u8]) -> Result<()> {
        self.validate_body_range(&range)?;
        let range_len = range.end - range.start;
        if range_len != bytes.len() {
            return Err(ShuError::BodyWriteLengthMismatch {
                page_id: self.id(),
                range_len,
                bytes_len: bytes.len(),
            });
        }

        let body = self.body_mut();
        body[range].copy_from_slice(bytes);
        Ok(())
    }

    pub(crate) fn read_body_u16(&self, offset: usize) -> Result<u16> {
        let page_id = self.id();
        let end = offset
            .checked_add(size_of::<u16>())
            .ok_or_else(|| self.invalid_body_range(offset, usize::MAX))?;
        let body_len = self.body().len();
        if end > body_len {
            return Err(ShuError::InvalidBodyRange {
                page_id,
                start: offset,
                end,
                body_len,
            });
        }
        let range = u16::from_le_bytes(
            self.body()[offset..end]
                .try_into()
                .map_err(|_| ShuError::PageNotFound { page_id })?,
        );
        Ok(range)
    }

    pub(crate) fn read_body_bytes(&self, range: Range<usize>) -> Result<&[u8]> {
        self.validate_body_range(&range)?;
        let bytes = &self.body()[range];
        Ok(bytes)
    }

    fn validate_body_range(&self, range: &Range<usize>) -> Result<()> {
        if range.start > range.end || range.end > self.body().len() {
            return Err(self.invalid_body_range(range.start, range.end));
        }

        Ok(())
    }

    fn invalid_body_range(&self, start: usize, end: usize) -> ShuError {
        ShuError::InvalidBodyRange {
            page_id: self.id(),
            start,
            end,
            body_len: self.body().len(),
        }
    }

    pub fn read_body_prefix<T: bytemuck::Pod>(&self) -> Result<T> {
        let len = size_of::<T>();
        if len > self.body().len() {
            return Err(ShuError::CorruptedPage { page_id: self.id() });
        }

        Ok(bytemuck::pod_read_unaligned(&self.body()[..len]))
    }

    pub fn write_body_prefix<T: bytemuck::Pod>(&mut self, value: &T) -> Result<()> {
        let len = size_of::<T>();
        if len > self.body().len() {
            return Err(ShuError::CorruptedPage { page_id: self.id() });
        }

        self.body_mut()[..len].copy_from_slice(bytemuck::bytes_of(value));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_body_bytes_rejects_range_past_body_end() {
        let mut page = Page::new(PageType::Leaf, PageId::new(7));
        let body_len = page.body().len();

        let result = page.write_body_bytes(body_len..body_len + 1, &[1]);

        assert!(matches!(
            result,
            Err(ShuError::InvalidBodyRange {
                page_id: PageId(7),
                start,
                end,
                body_len: len,
            }) if start == body_len && end == body_len + 1 && len == body_len
        ));
    }

    #[test]
    fn write_body_bytes_rejects_length_mismatch() {
        let mut page = Page::new(PageType::Leaf, PageId::new(7));

        let result = page.write_body_bytes(0..2, &[1]);

        assert!(matches!(
            result,
            Err(ShuError::BodyWriteLengthMismatch {
                page_id: PageId(7),
                range_len: 2,
                bytes_len: 1,
            })
        ));
    }

    #[test]
    fn read_body_u16_allows_value_ending_at_body_end() {
        let mut page = Page::new(PageType::Leaf, PageId::new(7));
        let offset = page.body().len() - size_of::<u16>();

        page.write_body_u16(offset, 0x1234).unwrap();

        assert_eq!(page.read_body_u16(offset).unwrap(), 0x1234);
    }
}
