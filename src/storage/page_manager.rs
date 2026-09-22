use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::storage::metadata::{DbHeader, DEFAULT_METADATA_PAGE_ID, HEADER_PAGE_ID};
use crate::storage::page::{Page, PageId, PageType, PAGE_SIZE};

/// Manages physical page persistence on disk.
///
/// `PageManager` directly interfaces with the underlying single database file,
/// performing structured I/O for fixed-size 4096-byte pages, tracking database
/// header metadata, and allocating pages in an append-only sequence.
#[derive(Debug)]
pub struct PageManager {
    file: File,
    path: PathBuf,
    header: DbHeader,
}

impl PageManager {
    /// Creates a brand new database file at `path`, initializing Page 0 (Header)
    /// and Page 1 (Metadata).
    ///
    /// Fails if the file already exists or cannot be created.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path_buf)?;

        let header = DbHeader::new();
        let header_bytes = header.serialize();
        file.write_all(&header_bytes)?;

        let metadata_page = Page::new(DEFAULT_METADATA_PAGE_ID, PageType::Metadata);
        file.write_all(metadata_page.as_bytes())?;

        file.sync_all()?;

        Ok(Self {
            file,
            path: path_buf,
            header,
        })
    }

    /// Opens an existing database file at `path`, reading and validating the
    /// database header on Page 0.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new().read(true).write(true).open(&path_buf)?;

        let metadata = file.metadata()?;
        let file_len = metadata.len();

        let min_expected_len = (2 * PAGE_SIZE) as u64;
        if file_len < min_expected_len {
            return Err(Error::InvalidDatabase(format!(
                "file size {file_len} bytes is smaller than minimum required database size ({min_expected_len} bytes)"
            )));
        }

        let mut header_bytes = [0u8; PAGE_SIZE];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut header_bytes)?;

        let header = DbHeader::deserialize(&header_bytes)?;

        let expected_min_len = (header.page_count as u64) * (PAGE_SIZE as u64);
        if file_len < expected_min_len {
            return Err(Error::InvalidDatabase(format!(
                "database file truncated: file length {file_len} bytes is less than expected {expected_min_len} bytes for {} pages",
                header.page_count
            )));
        }

        Ok(Self {
            file,
            path: path_buf,
            header,
        })
    }

    /// Reads an individual page by ID from disk.
    pub fn read_page(&mut self, page_id: PageId) -> Result<Page> {
        if page_id >= self.header.page_count {
            return Err(Error::InvalidPage(page_id));
        }

        let offset = (page_id as u64) * (PAGE_SIZE as u64);
        self.file.seek(SeekFrom::Start(offset))?;

        let mut raw = [0u8; PAGE_SIZE];
        self.file.read_exact(&mut raw)?;

        if page_id == HEADER_PAGE_ID {
            return Ok(Page::from_raw(raw));
        }

        Page::from_bytes(raw)
    }

    /// Writes an individual page to its corresponding disk offset.
    pub fn write_page(&mut self, page: &Page) -> Result<()> {
        let page_id = page.id();
        if page_id >= self.header.page_count {
            return Err(Error::InvalidPage(page_id));
        }

        let offset = (page_id as u64) * (PAGE_SIZE as u64);
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(page.as_bytes())?;

        Ok(())
    }

    /// Allocates a page by reusing a free page from the freelist, or appending to the file if empty.
    ///
    /// Updates the Database Header on disk.
    pub fn allocate_page(&mut self) -> Result<PageId> {
        // 1. Try recycling a free page from the freelist first
        if let Some(reused_page_id) =
            crate::storage::freelist::FreePageManager::allocate_page(self)?
        {
            return Ok(reused_page_id);
        }

        // 2. Fall back to appending a new page at the end of the file
        let new_page_id = self.header.page_count;
        let new_page = Page::new(new_page_id, PageType::Free);

        let offset = (new_page_id as u64) * (PAGE_SIZE as u64);
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(new_page.as_bytes())?;

        self.header.page_count += 1;
        self.write_header_to_disk()?;

        Ok(new_page_id)
    }

    /// Reclaims a page into the persistent freelist.
    pub fn free_page(&mut self, page_id: PageId) -> Result<()> {
        crate::storage::freelist::FreePageManager::free_page(self, page_id)
    }

    /// Validates the structural integrity of the persistent freelist.
    pub fn validate_freelist(&mut self) -> Result<()> {
        crate::storage::freelist::FreePageManager::validate(self)
    }

    /// Flushes all pending writes and synchronizes file state to physical media.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    /// Returns the total number of allocated pages in the database.
    pub fn page_count(&self) -> u32 {
        self.header.page_count
    }

    /// Returns a reference to the active Database Header.
    pub fn header(&self) -> &DbHeader {
        &self.header
    }

    /// Returns a mutable reference to the active Database Header.
    pub fn header_mut(&mut self) -> &mut DbHeader {
        &mut self.header
    }

    /// Returns the path to the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Updates the root page ID in the header and persists it to disk.
    pub fn set_root_page_id(&mut self, root_page_id: PageId) -> Result<()> {
        if root_page_id >= self.header.page_count {
            return Err(Error::InvalidPage(root_page_id));
        }
        self.header.root_page_id = root_page_id;
        self.write_header_to_disk()
    }

    /// Updates the last checkpoint LSN in the header and persists it to disk.
    pub fn set_last_checkpoint_lsn(&mut self, lsn: crate::wal::lsn::Lsn) -> Result<()> {
        self.header.last_checkpoint_lsn = lsn;
        self.write_header_to_disk()
    }

    /// Writes the serialized header directly to Page 0 on disk.
    pub fn write_header_to_disk(&mut self) -> Result<()> {
        let header_bytes = self.header.serialize();
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header_bytes)?;
        Ok(())
    }
}
