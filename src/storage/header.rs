use crate::{
    error::{Result, ShuError},
    storage::page::{Page, PageId, PageType},
};

pub const INITIAL_ROOT_PAGE_ID: PageId = PageId::new(1);
pub const FREELIST_DEFAULT: u32 = 0;

#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable, Default)]
#[repr(C)]
pub struct DatabaseHeader {
    pub root_page_id: PageId,
    pub page_count: u32,
    pub freelist_head: u32,
    pub _reserved: [u8; 4], // pad to 16 bytes
}

impl DatabaseHeader {
    pub fn read_from(page: &Page) -> Result<Self> {
        let page_id = page.id();

        if page.page_type()? != PageType::Meta {
            return Err(ShuError::CorruptedPage { page_id });
        }

        let header = page.read_body_prefix::<Self>()?;
        header.validate(page_id)?;
        Ok(header)
    }

    pub fn write_to(&self, page: &mut Page) -> Result<()> {
        let page_id = page.id();

        if page.page_type()? != PageType::Meta {
            return Err(ShuError::CorruptedPage { page_id });
        }

        self.validate(page_id)?;
        page.write_body_prefix(self)?;

        Ok(())
    }

    fn validate(&self, page_id: PageId) -> Result<()> {
        if self.page_count < 2 {
            return Err(ShuError::CorruptedPage { page_id });
        }

        if self.root_page_id == page_id {
            return Err(ShuError::CorruptedPage { page_id });
        }

        if self.root_page_id.get() >= self.page_count {
            return Err(ShuError::CorruptedPage { page_id });
        }

        Ok(())
    }
}
