use crate::heap_page::HeapPage;
use crate::heap_page::HeapPageIntoIter;
use crate::heapfile::HeapFile;
use common::prelude::*;
use std::sync::Arc;

#[allow(dead_code)]
/// The struct for a HeapFileIterator.
/// We use a slightly different approach for HeapFileIterator than
/// standard way of Rust's IntoIter for simplicity (avoiding lifetime issues).
/// This should store the state/metadata required to iterate through the file.
///
/// HINT: This will need an Arc<HeapFile>
pub struct HeapFileIterator {
    hf: Arc<HeapFile>,
    current_pid: PageId,
    page_iter: Option<HeapPageIntoIter>,
    tid: TransactionId,
}

/// Required HeapFileIterator functions
impl HeapFileIterator {
    /// Create a new HeapFileIterator that stores the tid, and heapFile pointer.
    /// This should initialize the state required to iterate through the heap file.
    pub(crate) fn new(tid: TransactionId, hf: Arc<HeapFile>) -> Self {
        HeapFileIterator { hf, current_pid: 0, page_iter: None, tid }
    }

    pub(crate) fn new_from(tid: TransactionId, hf: Arc<HeapFile>, value_id: ValueId) -> Self {
        let pid = value_id.page_id.unwrap_or(0);
        let slot = value_id.slot_id.unwrap_or(0);
        let page_iter = match hf.read_page_from_file(pid) {
            Ok(page) => {
                let mut iter = page.into_iter();

                for _ in 0..slot {
                    if iter.next().is_none() {
                        break;
                    }
                }

                Some(iter)
            }
            Err(_) => None,
        };

        Self { hf, current_pid: pid + 1, page_iter, tid }

    }
}

/// Trait implementation for heap file iterator.
/// Note this will need to iterate through the pages and their respective iterators.
impl Iterator for HeapFileIterator {
    type Item = (Vec<u8>, ValueId);
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(iter) = self.page_iter.as_mut() {
                if let Some((bytes, slot_id)) = iter.next() {
                    let value_id = ValueId {
                        container_id: self.hf.container_id,
                        segment_id: None,
                        page_id: Some(self.current_pid - 1),
                        slot_id: Some(slot_id),
                    };
                    return Some((bytes, value_id));
                }
            }

            if self.current_pid >= self.hf.num_pages() {
                return None;
            }

            match self.hf.read_page_from_file(self.current_pid) {
                Ok(page) => {
                    self.page_iter = Some(page.into_iter());
                    self.current_pid += 1;
                }
                Err(_) => {
                    return None;
                }
            }
        }
    }
}
