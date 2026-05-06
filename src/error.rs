use thiserror::Error;

use crate::storage::page::PageId;

#[derive(Debug, Error)]
pub enum ShuError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupted page {page_id}")]
    CorruptedPage { page_id: PageId },
    #[error("page {page_id} not found")]
    PageNotFound { page_id: PageId },
    #[error("page type byte invalid")]
    InvalidPageType,
}

pub type Result<T> = std::result::Result<T, ShuError>;
