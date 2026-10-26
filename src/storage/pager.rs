use std::{fs::OpenOptions, path::Path};

use crate::{
    error::{Result, ShuError},
    storage::{
        FileStorage,
        btree_page::{InternalNodeMut, LeafNodeMut, NodeKind},
        header::{DatabaseHeader, FREELIST_DEFAULT, INITIAL_ROOT_PAGE_ID},
        page::{Page, PageId, PageType},
    },
};

const META_PAGE_ID: PageId = PageId::new(0);

#[derive(Debug)]
pub struct Pager {
    pub(crate) page_count: u32,
    /// Indexed by page id. Boxed so growing the vector moves pointers, not 4 KiB pages.
    // TODO: Bound the cache. It holds every page touched since open.
    cache: Vec<Option<Box<CachedPage>>>,
    storage: FileStorage,
    root_page_id: PageId,
    freelist_head: u32,
}

#[derive(Debug)]
struct CachedPage {
    page: Page,
    dirty: bool,
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
                cache: Vec::new(),
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
            LeafNodeMut::init(&mut root_page)?;
            storage.write_page(&root_page)?;

            Ok(Self {
                page_count: 2,
                cache: Vec::new(),
                freelist_head: FREELIST_DEFAULT,
                root_page_id: INITIAL_ROOT_PAGE_ID,
                storage,
            })
        }
    }

    pub(crate) fn root_page_id(&self) -> PageId {
        self.root_page_id
    }

    /// Returns the cached page, reading it from disk on first access. Borrowing instead of
    /// cloning keeps a 4 KiB copy off every tree level of every lookup.
    pub fn read_page(&mut self, page_id: PageId) -> Result<&Page> {
        Ok(&self.cached(page_id)?.page)
    }

    /// Returns the cached page for modification and marks it dirty.
    pub fn get_mut(&mut self, page_id: PageId) -> Result<&mut Page> {
        let cached = self.cached(page_id)?;
        cached.dirty = true;
        Ok(&mut cached.page)
    }

    pub fn write_page(&mut self, page: &Page) -> Result<()> {
        self.get_mut(page.id())?.clone_from(page);
        Ok(())
    }

    /// Appends a formatted, empty page of `kind` and returns its id.
    pub(crate) fn allocate(&mut self, kind: NodeKind) -> Result<PageId> {
        let page_id = PageId::new(self.page_count);
        self.page_count = self
            .page_count
            .checked_add(1)
            .ok_or(ShuError::PageCountOverflow)?;

        let mut cached = Box::new(CachedPage {
            page: Page::new(kind.into(), page_id),
            dirty: true,
        });
        match kind {
            NodeKind::Leaf => {
                LeafNodeMut::init(&mut cached.page)?;
            }
            NodeKind::Internal => {
                InternalNodeMut::init(&mut cached.page)?;
            }
        }
        *cache_slot(&mut self.cache, page_id) = Some(cached);
        self.flush_meta()?;
        Ok(page_id)
    }

    pub fn sync(&mut self) -> Result<()> {
        for cached in self.cache.iter_mut().flatten() {
            if cached.dirty {
                self.storage.write_page(&cached.page)?;
                cached.dirty = false;
            }
        }

        self.storage.sync()
    }

    fn cached(&mut self, page_id: PageId) -> Result<&mut CachedPage> {
        if page_id.get() >= self.page_count {
            return Err(ShuError::PageNotFound { page_id });
        }

        let slot = cache_slot(&mut self.cache, page_id);
        if slot.is_none() {
            let page = self.storage.read_page(page_id)?;
            *slot = Some(Box::new(CachedPage { page, dirty: false }));
        }
        slot.as_deref_mut()
            .ok_or(ShuError::PageNotFound { page_id })
    }

    fn flush_meta(&mut self) -> Result<()> {
        let page_count = self.page_count;
        let root_page_id = self.root_page_id;
        let freelist_head = self.freelist_head;
        let meta_page = self.get_mut(META_PAGE_ID)?;
        let mut header = DatabaseHeader::read_from(meta_page)?;
        header.page_count = page_count;
        header.root_page_id = root_page_id;
        header.freelist_head = freelist_head;
        header.write_to(meta_page)
    }
}

/// Returns the cache slot for `page_id`, growing the cache to cover it. A free function so callers
/// can keep using other `Pager` fields while holding the slot.
fn cache_slot(
    cache: &mut Vec<Option<Box<CachedPage>>>,
    page_id: PageId,
) -> &mut Option<Box<CachedPage>> {
    let index = page_id.as_index();
    if index >= cache.len() {
        cache.resize_with(index + 1, || None);
    }
    &mut cache[index]
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
        assert_eq!(meta.id(), PageId::new(0));
        assert_eq!(meta.page_type().unwrap(), PageType::Meta);
    }

    #[test]
    fn root_page_has_correct_type() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let root = pager.read_page(INITIAL_ROOT_PAGE_ID).unwrap();
        assert_eq!(root.id(), INITIAL_ROOT_PAGE_ID);
        assert_eq!(root.page_type().unwrap(), PageType::Leaf);
    }

    #[test]
    fn meta_page_stores_root_page_id() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let meta = pager.read_page(META_PAGE_ID).unwrap();
        let header = DatabaseHeader::read_from(meta).unwrap();

        assert_eq!(header.root_page_id, INITIAL_ROOT_PAGE_ID);
        assert_eq!(header.page_count, 2);
    }

    #[test]
    fn allocate_increments_page_count() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let page1 = pager.allocate(NodeKind::Leaf).unwrap();
        let page2 = pager.allocate(NodeKind::Leaf).unwrap();

        assert_eq!(page1, PageId::new(2));
        assert_eq!(page2, PageId::new(3));
        assert_eq!(pager.page_count, 4);

        let page = pager.read_page(page2).unwrap();
        assert_eq!(page.id(), page2);
    }

    #[test]
    fn allocate_initializes_internal_pages() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let page_id = pager.allocate(NodeKind::Internal).unwrap();

        let page = pager.read_page(page_id).unwrap();
        assert_eq!(page.page_type().unwrap(), PageType::Internal);
        assert_eq!(page.body()[..8], [0, 0, 240, 15, 0, 0, 0, 0]);
    }

    #[test]
    fn write_and_read_back() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();
        let id = pager.allocate(NodeKind::Leaf).unwrap();

        let mut page = pager.read_page(id).unwrap().clone();
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
            pager.allocate(NodeKind::Leaf).unwrap();
            pager.allocate(NodeKind::Internal).unwrap();
            pager.sync().unwrap();
        }

        let pager = Pager::open(&path).unwrap();
        assert_eq!(pager.page_count, 4); // meta + root + 2 allocated
    }

    #[test]
    fn read_page_rejects_ids_past_page_count() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let mut pager = Pager::open(&path).unwrap();

        assert!(matches!(
            pager.read_page(PageId::new(2)),
            Err(ShuError::PageNotFound { .. })
        ));
        assert!(matches!(
            pager.get_mut(PageId::new(2)),
            Err(ShuError::PageNotFound { .. })
        ));
    }

    #[test]
    fn reopen_reads_synced_page_contents() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();

        let id = {
            let mut pager = Pager::open(&path).unwrap();
            let id = pager.allocate(NodeKind::Leaf).unwrap();
            pager.get_mut(id).unwrap().body_mut()[100..105].copy_from_slice(b"hello");
            pager.sync().unwrap();
            id
        };

        let mut pager = Pager::open(&path).unwrap();
        let page = pager.read_page(id).unwrap();
        assert_eq!(page.id(), id);
        assert_eq!(&page.body()[100..105], b"hello");
    }
}
