use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::Result;
use crate::storage::header::{DatabaseHeader, FREELIST_DEFAULT, ROOT_PAGE_ID_DEFAULT};
use crate::storage::page::{HEADER_SIZE, PAGE_SIZE, Page, PageId, PageType};

#[derive(Debug)]
pub struct FileStorage {
    file: File,
}

impl FileStorage {
    fn new(file: File) -> Self {
        Self { file }
    }
}

#[derive(Debug)]
pub struct Pager {
    pub(crate) page_count: u32,
    storage: FileStorage,
    freelist_head: u32,
}

impl Pager {
    pub fn open(path: &Path) -> Result<Self> {
        let exists = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        let mut storage = FileStorage::new(file);

        if exists && storage.file_len()? > 0 {
            // Existing database — read meta page to recover state
            let meta_page = storage.read_page(0)?;
            let header: &DatabaseHeader =
                bytemuck::from_bytes(&meta_page.body()[..size_of::<DatabaseHeader>()]);

            Ok(Self {
                page_count: header.page_count,
                freelist_head: header.freelist_head,
                storage,
            })
        } else {
            // New database — write initial meta page
            let mut meta_page = Page::new(PageType::Meta);
            let header = DatabaseHeader {
                root_page_id: ROOT_PAGE_ID_DEFAULT,
                page_count: 1,
                freelist_head: FREELIST_DEFAULT,
                _reserved: [0; 4],
            };
            meta_page.body_mut()[..size_of::<DatabaseHeader>()]
                .copy_from_slice(bytemuck::bytes_of(&header));
            storage.write_page(0, &meta_page)?;

            Ok(Self {
                page_count: 1,
                freelist_head: FREELIST_DEFAULT,
                storage,
            })
        }
    }

    pub fn read_page(&mut self, page_id: PageId) -> Result<Page> {
        self.storage.read_page(page_id)
    }

    pub fn write_page(&mut self, page_id: PageId, page: &Page) -> Result<()> {
        self.storage.write_page(page_id, page)
    }

    pub fn allocate(&mut self, page_type: PageType) -> Result<PageId> {
        let page_id = self.page_count;
        let page = Page::new(page_type);
        self.storage.write_page(page_id, &page)?;
        self.page_count += 1;
        self.flush_meta()?;
        Ok(page_id)
    }

    pub fn sync(&mut self) -> Result<()> {
        self.storage.file.sync_all()?;
        Ok(())
    }

    fn flush_meta(&mut self) -> Result<()> {
        let mut meta_page = self.storage.read_page(0)?;
        let header: &mut DatabaseHeader =
            bytemuck::from_bytes_mut(&mut meta_page.body_mut()[..size_of::<DatabaseHeader>()]);
        header.page_count = self.page_count;
        header.freelist_head = self.freelist_head;
        self.storage.write_page(0, &meta_page)?;
        Ok(())
    }
}

impl FileStorage {
    fn read_page(&mut self, page_id: PageId) -> Result<Page> {
        let offset = page_id as u64 * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        let mut page = Page {
            data: [0u8; PAGE_SIZE],
        };
        self.file.read_exact(&mut page.data)?;
        Ok(page)
    }

    fn write_page(&mut self, page_id: PageId, page: &Page) -> Result<()> {
        let offset = page_id as u64 * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&page.data)?;
        Ok(())
    }

    fn file_len(&mut self) -> Result<u64> {
        Ok(self.file.seek(SeekFrom::End(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn new_database_has_one_page() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        // Remove the file so Pager creates a fresh one
        std::fs::remove_file(&path).unwrap();

        let pager = Pager::open(&path).unwrap();
        assert_eq!(pager.page_count, 1);
    }

    #[test]
    fn meta_page_has_correct_type() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let meta = pager.read_page(0).unwrap();
        assert_eq!(meta.header().page_type, PageType::Meta as u8);
    }

    #[test]
    fn allocate_increments_page_count() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let id1 = pager.allocate(PageType::Leaf).unwrap();
        let id2 = pager.allocate(PageType::Leaf).unwrap();

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(pager.page_count, 3);
    }

    #[test]
    fn write_and_read_back() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let id = pager.allocate(PageType::Leaf).unwrap();

        let mut page = pager.read_page(id).unwrap();
        page.body_mut()[..5].copy_from_slice(b"hello");
        pager.write_page(id, &page).unwrap();

        let page2 = pager.read_page(id).unwrap();
        assert_eq!(&page2.body()[..5], b"hello");
    }

    #[test]
    fn reopen_preserves_state() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        {
            let mut pager = Pager::open(&path).unwrap();
            pager.allocate(PageType::Leaf).unwrap();
            pager.allocate(PageType::Internal).unwrap();
            pager.sync().unwrap();
        }

        let pager = Pager::open(&path).unwrap();
        assert_eq!(pager.page_count, 3); // meta + 2 allocated
    }
}
