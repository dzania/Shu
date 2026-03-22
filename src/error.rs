use thiserror::Error;

#[derive(Debug, Error)]
pub enum ShuError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupted page {page_id}")]
    CorruptedPage { page_id: u32 },
    #[error("page {page_id} not found")]
    PageNotFound { page_id: u32 },
}

pub type Result<T> = std::result::Result<T, ShuError>;
