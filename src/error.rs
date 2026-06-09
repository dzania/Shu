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
    #[error("index out of range")]
    IndexOutOfRange,
    #[error("invalid page {page_id} body range {start}..{end} for body length {body_len}")]
    InvalidBodyRange {
        page_id: PageId,
        start: usize,
        end: usize,
        body_len: usize,
    },
    #[error(
        "page {page_id} body write length mismatch: range has {range_len} bytes, input has {bytes_len} bytes"
    )]
    BodyWriteLengthMismatch {
        page_id: PageId,
        range_len: usize,
        bytes_len: usize,
    },
    #[error("cell is too large for page {page_id}: key {key_len} bytes, value {value_len} bytes")]
    CellTooLarge {
        page_id: PageId,
        key_len: usize,
        value_len: usize,
    },
    #[error(
        "page {page_id} has insufficient free space: need {needed} bytes, available {available} bytes"
    )]
    PageFull {
        page_id: PageId,
        needed: usize,
        available: usize,
    },
}

pub type Result<T> = std::result::Result<T, ShuError>;
