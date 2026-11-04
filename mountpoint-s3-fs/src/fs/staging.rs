//! Local backing for new files that need reads and non-sequential writes before close.
//!
//! The temporary file is unlinked immediately. There is no background write-back timer:
//! fsync/close upload a complete snapshot to S3 and return upload failures to the caller.
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use bytes::Bytes;
use mountpoint_s3_client::ObjectClient;
use mountpoint_s3_client::types::ETag;

use crate::metablock::{InodeError, S3Location};
use crate::sync::{Arc, AsyncMutex, Weak};
use crate::upload::Uploader;

use super::{Error, OpenFlags};

#[derive(Debug)]
pub struct StagedFileRef {
    pub file: Weak<AsyncMutex<StagedFile>>,
    pub location: S3Location,
}

#[derive(Debug)]
pub struct StagedFile {
    file: File,
    size: u64,
    dirty: bool,
    etag: Option<ETag>,
    pub completed: bool,
}

impl StagedFile {
    pub fn new(directory: &Path) -> std::io::Result<Self> {
        let file = tempfile::tempfile_in(directory)?;
        Ok(Self {
            file,
            size: 0,
            dirty: true,
            etag: None,
            completed: false,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    pub fn read(&self, offset: i64, size: u32) -> std::io::Result<Bytes> {
        let offset = u64::try_from(offset).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        let len = (size as u64).min(self.size.saturating_sub(offset)) as usize;
        let mut data = vec![0; len];
        self.file.read_exact_at(&mut data, offset)?;
        Ok(data.into())
    }

    pub fn write(&mut self, offset: i64, data: &[u8]) -> std::io::Result<(usize, usize)> {
        let offset = u64::try_from(offset).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        if data.is_empty() {
            return Ok((0, 0));
        }
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EFBIG))?;
        self.file.write_all_at(data, offset)?;
        let growth = end.saturating_sub(self.size);
        self.size = self.size.max(end);
        self.dirty = true;
        Ok((data.len(), growth as usize))
    }
}

pub fn local_error(error: std::io::Error) -> Error {
    err!(error.raw_os_error().unwrap_or(libc::EIO), source:error, "local write staging failed")
}

#[derive(Debug)]
pub struct StagedUpload<Client: ObjectClient> {
    pub file: Arc<AsyncMutex<StagedFile>>,
    pub readable: bool,
    uploader: Arc<Uploader<Client>>,
    location: S3Location,
}

impl<Client: ObjectClient + Clone + Send + Sync + 'static> StagedUpload<Client> {
    pub fn new(
        directory: &Path,
        flags: OpenFlags,
        uploader: Arc<Uploader<Client>>,
        location: S3Location,
    ) -> std::io::Result<Self> {
        Ok(Self {
            file: Arc::new(AsyncMutex::new(StagedFile::new(directory)?)),
            readable: flags.contains(OpenFlags::O_RDWR),
            uploader,
            location,
        })
    }

    pub async fn upload(&self) -> Result<ETag, InodeError> {
        Self::upload_file(&self.file, &self.uploader, &self.location).await
    }

    pub async fn upload_file(
        file: &Arc<AsyncMutex<StagedFile>>,
        uploader: &Uploader<Client>,
        location: &S3Location,
    ) -> Result<ETag, InodeError> {
        // Hold this lock through upload so fsync publishes one consistent image.
        let mut staged = file.lock().await;
        if staged.completed && staged.dirty {
            return Err(InodeError::ClientError {
                source: Arc::new(std::io::Error::from_raw_os_error(libc::EIO).into()),
                metadata: Default::default(),
            });
        }
        if !staged.dirty {
            return Ok(staged.etag.clone().expect("clean staging file has an uploaded ETag"));
        }
        let mut upload = uploader
            .start_atomic_upload(location.bucket_name().to_owned(), location.full_key().to_string())
            .map_err(|e| InodeError::upload_error(e, location.clone()))?;
        let mut buffer = vec![0; 1024 * 1024];
        let mut offset = 0;
        while offset < staged.size {
            let len = (staged.size - offset).min(buffer.len() as u64) as usize;
            staged
                .file
                .read_exact_at(&mut buffer[..len], offset)
                .map_err(|e| InodeError::ClientError {
                    source: Arc::new(e.into()),
                    metadata: Default::default(),
                })?;
            upload
                .write(offset as i64, &buffer[..len])
                .await
                .map_err(|e| InodeError::upload_error(e, location.clone()))?;
            offset += len as u64;
        }
        let result = upload
            .complete()
            .await
            .map_err(|e| InodeError::upload_error(e, location.clone()))?;
        staged.dirty = false;
        staged.etag = Some(result.etag.clone());
        Ok(result.etag)
    }
}
