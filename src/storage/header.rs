pub const ROOT_PAGE_ID_DEFAULT: u32 = 0;
pub const FREELIST_DEFAULT: u32 = 0;

#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable, Default)]
#[repr(C)]
pub struct DatabaseHeader {
    pub root_page_id: u32,
    pub page_count: u32,
    pub freelist_head: u32,
    pub _reserved: [u8; 4], // pad to 16 bytes
}
