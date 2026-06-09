use std::{fs::OpenOptions, path::Path};

use crate::{
    error::{Result, ShuError},
    storage::{
        FileStorage,
        btree::{init_internal_page, init_leaf_page},
        header::{DatabaseHeader, FREELIST_DEFAULT, INITIAL_ROOT_PAGE_ID},
        page::{Page, PageId, PageType},
    },
};

const META_PAGE_ID: PageId = PageId::new(0);

#[derive(Debug)]
pub struct Pager {
    pub(crate) page_count: u32,
    storage: FileStorage,
    root_page_id: PageId,
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
            let meta_page = storage.read_page(META_PAGE_ID)?;
            let header = DatabaseHeader::read_from(&meta_page)?;

            Ok(Self {
                page_count: header.page_count,
                freelist_head: header.freelist_head,
                root_page_id: header.root_page_id,
                storage,
            })
        } else {
            // New database — write initial meta page
            let mut meta_page = Page::new(PageType::Meta, META_PAGE_ID);
            let header = DatabaseHeader {
                root_page_id: INITIAL_ROOT_PAGE_ID,
                page_count: 2,
                freelist_head: FREELIST_DEFAULT,
                _reserved: [0; 4],
            };
            header.write_to(&mut meta_page)?;
            storage.write_page(&meta_page)?;
            let mut root_page = Page::new(PageType::Leaf, INITIAL_ROOT_PAGE_ID);
            init_leaf_page(&mut root_page)?;
            storage.write_page(&root_page)?;

            Ok(Self {
                page_count: 2,
                freelist_head: FREELIST_DEFAULT,
                root_page_id: INITIAL_ROOT_PAGE_ID,
                storage,
            })
        }
    }

    pub(crate) fn root_page_id(&self) -> PageId {
        self.root_page_id
    }

    pub fn read_page(&mut self, page_id: PageId) -> Result<Page> {
        if page_id.get() >= self.page_count {
            return Err(ShuError::PageNotFound { page_id });
        }

        self.storage.read_page(page_id)
    }

    fn read_meta(&mut self) -> Result<Page> {
        self.storage.read_page(META_PAGE_ID)
    }

    fn write_meta(&mut self, page: &Page) -> Result<()> {
        self.storage.write_page(page)
    }

    pub fn write_page(&mut self, page: &Page) -> Result<()> {
        let page_id = page.id();
        if page_id.get() >= self.page_count {
            return Err(ShuError::PageNotFound { page_id });
        }

        self.storage.write_page(page)
    }

    pub fn allocate(&mut self, page_type: PageType) -> Result<Page> {
        let page_id = PageId::new(self.page_count);
        let mut page = Page::new(page_type, page_id);
        match page_type {
            PageType::Internal => init_internal_page(&mut page)?,
            PageType::Leaf => init_leaf_page(&mut page)?,
            PageType::Meta => {}
        }
        self.storage.write_page(&page)?;
        self.page_count += 1;
        self.flush_meta()?;
        Ok(page)
    }

    pub fn sync(&mut self) -> Result<()> {
        self.storage.sync()
    }

    fn flush_meta(&mut self) -> Result<()> {
        let mut meta_page = self.read_meta()?;
        let mut header = DatabaseHeader::read_from(&meta_page)?;
        header.page_count = self.page_count;
        header.root_page_id = self.root_page_id;
        header.freelist_head = self.freelist_head;
        header.write_to(&mut meta_page)?;
        self.write_meta(&meta_page)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn new_database_has_meta_and_root_pages() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        // Remove the file so Pager creates a fresh one
        std::fs::remove_file(&path).unwrap();

        let pager = Pager::open(&path).unwrap();
        assert_eq!(pager.page_count, 2);
    }

    #[test]
    fn meta_page_has_correct_type() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let meta = pager.read_page(PageId::new(0)).unwrap();
        assert_eq!(meta.header().page_id, PageId::new(0));
        assert_eq!(meta.header().page_type, PageType::Meta as u8);
    }

    #[test]
    fn root_page_has_correct_type() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let root = pager.read_page(INITIAL_ROOT_PAGE_ID).unwrap();
        assert_eq!(root.header().page_id, INITIAL_ROOT_PAGE_ID);
        assert_eq!(root.header().page_type, PageType::Leaf as u8);
    }

    #[test]
    fn meta_page_stores_root_page_id() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let meta = pager.read_page(META_PAGE_ID).unwrap();
        let header = DatabaseHeader::read_from(&meta).unwrap();

        assert_eq!(header.root_page_id, INITIAL_ROOT_PAGE_ID);
        assert_eq!(header.page_count, 2);
    }

    #[test]
    fn allocate_increments_page_count() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let page1 = pager.allocate(PageType::Leaf).unwrap();
        let page2 = pager.allocate(PageType::Leaf).unwrap();

        assert_eq!(page1.id(), PageId::new(2));
        assert_eq!(page2.id(), PageId::new(3));
        assert_eq!(pager.page_count, 4);

        let page = pager.read_page(page2.id()).unwrap();
        assert_eq!(page.header().page_id, page2.id());
    }

    #[test]
    fn allocate_initializes_internal_pages() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let page_id = pager.allocate(PageType::Internal).unwrap().id();

        let page = pager.read_page(page_id).unwrap();
        assert_eq!(page.header().page_type, PageType::Internal as u8);
        assert_eq!(page.body()[..8], [0, 0, 240, 15, 0, 0, 0, 0]);
    }

    #[test]
    fn write_and_read_back() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let id = pager.allocate(PageType::Leaf).unwrap().id();

        let mut page = pager.read_page(id).unwrap();
        page.body_mut()[..5].copy_from_slice(b"hello");
        pager.write_page(&page).unwrap();

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
        assert_eq!(pager.page_count, 4); // meta + root + 2 allocated
    }
}
