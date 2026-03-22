use bytemuck::{from_bytes, from_bytes_mut};

pub const PAGE_SIZE: usize = 4096;
pub const HEADER_SIZE: usize = 16;

pub type PageId = u32;

pub enum PageType {
    Meta,
    Internal,
    Leaf,
}

#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct PageHeader {
    pub page_id: u32,        // 4 bytes
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
    pub fn new(page_type: PageType) -> Self {
        let mut page = Page { data: [0_u8; 4096] };
        page.header_mut().page_type = page_type as u8;
        page
    }
    pub fn header(&self) -> &PageHeader {
        from_bytes(&self.data[..HEADER_SIZE])
    }

    pub fn header_mut(&mut self) -> &mut PageHeader {
        from_bytes_mut(&mut self.data[..HEADER_SIZE])
    }

    pub fn body(&self) -> &[u8] {
        &self.data[HEADER_SIZE..]
    }

    pub fn body_mut(&mut self) -> &mut [u8] {
        &mut self.data[HEADER_SIZE..]
    }
}
