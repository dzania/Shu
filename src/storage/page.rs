use std::fmt;

use bytemuck::{from_bytes, from_bytes_mut};

use crate::error::ShuError;

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
    Meta = 1,
    Internal = 2,
    Leaf = 3,
}

impl TryFrom<u8> for PageType {
    type Error = ShuError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
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

    pub fn page_type(&self) -> Option<PageType> {
        self.header().page_type.try_into().ok()
    }

    pub fn body(&self) -> &[u8] {
        &self.data[HEADER_SIZE..]
    }

    pub fn body_mut(&mut self) -> &mut [u8] {
        &mut self.data[HEADER_SIZE..]
    }

    pub fn read_body_prefix<T: bytemuck::Pod>(&self) -> T {
        let len = size_of::<T>();
        bytemuck::pod_read_unaligned(&self.body()[..len])
    }

    pub fn write_body_prefix<T: bytemuck::Pod>(&mut self, value: &T) {
        let len = size_of::<T>();
        self.body_mut()[..len].copy_from_slice(bytemuck::bytes_of(value));
    }
}
