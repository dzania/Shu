use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
};

use crate::{
    error::Result,
    storage::page::{PAGE_SIZE, Page, PageId},
};

pub mod btree;
pub(crate) mod btree_page;
pub mod header;
pub mod page;
pub mod pager;

#[derive(Debug)]
// TODO: This should be a trait for in memory storage too
pub(crate) struct FileStorage {
    file: File,
}

impl FileStorage {
    pub(crate) fn new(file: File) -> Self {
        Self { file }
    }

    pub(crate) fn read_page(&mut self, page_id: PageId) -> Result<Page> {
        let offset = page_id.as_u64() * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        let mut data = [0u8; PAGE_SIZE];
        self.file.read_exact(&mut data)?;
        Ok(Page::from_data(data))
    }

    pub(crate) fn write_page(&mut self, page: &Page) -> Result<()> {
        let page_id = page.id();
        let offset = page_id.as_u64() * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&page.data)?;
        Ok(())
    }

    pub(crate) fn file_len(&mut self) -> Result<u64> {
        Ok(self.file.seek(SeekFrom::End(0))?)
    }

    pub(crate) fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }
}
