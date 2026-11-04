//! Regressions for Git pack files and persistent empty directories.
mod common;

use common::make_test_filesystem_with_client;
use mountpoint_s3_client::failure_client::{CountdownFailureConfig, countdown_failure_client};
use mountpoint_s3_client::mock_client::{MockClient, MockClientError};
use mountpoint_s3_fs::fs::{OpenFlags, RenameFlags, ToErrno};
use mountpoint_s3_fs::memory::PagedPool;
use mountpoint_s3_fs::s3::S3Personality;
use mountpoint_s3_fs::{S3Filesystem, S3FilesystemConfig};
use std::path::Path;
use std::sync::Arc;

fn config(dir: &Path) -> S3FilesystemConfig {
    S3FilesystemConfig {
        experimental_write_cache: Some(dir.to_owned()),
        incremental_upload: true,
        allow_overwrite: true,
        allow_delete: true,
        s3_personality: S3Personality::ExpressOneZone,
        ..Default::default()
    }
}

fn setup(dir: &Path) -> (Arc<MockClient>, S3Filesystem<Arc<MockClient>>) {
    let client = Arc::new(
        MockClient::config()
            .bucket("bucket")
            .part_size(1024 * 1024)
            .enable_rename(true)
            .enable_backpressure(true)
            .initial_read_window_size(256 * 1024)
            .build(),
    );
    let fs = make_test_filesystem_with_client(
        client.clone(),
        PagedPool::new_with_candidate_sizes([1024 * 1024]),
        "bucket",
        &Default::default(),
        config(dir),
    );
    (client, fs)
}

async fn create(fs: &S3Filesystem<Arc<MockClient>>, name: &str, flags: OpenFlags) -> (u64, u64) {
    let ino = fs
        .mknod(1, name.as_ref(), libc::S_IFREG | 0o600, 0, 0)
        .await
        .unwrap()
        .attr
        .ino;
    let fh = fs.open(ino, flags, 0).await.unwrap().fh;
    (ino, fh)
}

#[tokio::test]
async fn pack_read_rewrite_sync_and_remount() {
    let dir = tempfile::tempdir().unwrap();
    let (client, fs) = setup(dir.path());
    let (ino, writer) = create(&fs, "pack", OpenFlags::O_RDWR).await;
    fs.write(ino, writer, 0, b"PACKold-body", 0, 0, None).await.unwrap();
    let reader = fs.open(ino, OpenFlags::empty(), 0).await.unwrap().fh;
    assert_eq!(
        fs.read(ino, reader, 0, 100, 0, None).await.unwrap(),
        &b"PACKold-body"[..]
    );
    assert_eq!(
        fs.write(ino, reader, 0, b"bad", 0, 0, None)
            .await
            .unwrap_err()
            .to_errno(),
        libc::EBADF
    );
    assert!(fs.open(ino, OpenFlags::O_WRONLY, 0).await.is_err());
    assert!(
        fs.rename(1, "pack".as_ref(), 1, "early".as_ref(), RenameFlags::empty())
            .await
            .is_err()
    );
    fs.fsync(ino, reader, false).await.unwrap();
    assert!(client.contains_key("pack"));
    fs.write(ino, writer, 4, b"new", 0, 0, None).await.unwrap();
    assert_eq!(
        fs.read(ino, writer, 0, 100, 0, None).await.unwrap(),
        &b"PACKnew-body"[..]
    );
    assert_eq!(fs.getattr(ino).await.unwrap().attr.size, 12);
    fs.flush(ino, writer, 0, 0).await.unwrap();
    // A reader that opened before close/rename keeps the same file image.
    fs.rename(1, "pack".as_ref(), 1, "final".as_ref(), RenameFlags::empty())
        .await
        .unwrap();
    assert_eq!(
        fs.read(ino, reader, 0, 100, 0, None).await.unwrap(),
        &b"PACKnew-body"[..]
    );
    fs.release(ino, writer, 0, None, false).await.unwrap();
    fs.release(ino, reader, 0, None, false).await.unwrap();
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

    let fresh = make_test_filesystem_with_client(
        client,
        PagedPool::new_with_candidate_sizes([1024 * 1024]),
        "bucket",
        &Default::default(),
        config(dir.path()),
    );
    assert_eq!(
        fresh.lookup(1, "pack".as_ref()).await.unwrap_err().to_errno(),
        libc::ENOENT
    );
    let entry = fresh.lookup(1, "final".as_ref()).await.unwrap();
    let fh = fresh.open(entry.attr.ino, OpenFlags::empty(), 0).await.unwrap().fh;
    assert_eq!(
        fresh.read(entry.attr.ino, fh, 0, 100, 0, None).await.unwrap(),
        &b"PACKnew-body"[..]
    );
    fresh.release(entry.attr.ino, fh, 0, None, false).await.unwrap();
}

#[tokio::test]
async fn empty_close_then_write_and_empty_lock_rename() {
    let dir = tempfile::tempdir().unwrap();
    let (client, fs) = setup(dir.path());
    let (ino, fh) = create(&fs, "lock", OpenFlags::O_WRONLY).await;
    fs.flush(ino, fh, 0, 0).await.unwrap(); // dup/close before first write
    fs.write(ino, fh, 0, b"data", 0, 0, None).await.unwrap();
    assert_eq!(
        fs.read(ino, fh, 0, 4, 0, None).await.unwrap_err().to_errno(),
        libc::EBADF
    );
    fs.flush(ino, fh, 0, 0).await.unwrap();
    fs.rename(1, "lock".as_ref(), 1, "config".as_ref(), RenameFlags::empty())
        .await
        .unwrap();
    fs.release(ino, fh, 0, None, false).await.unwrap();
    assert!(!client.contains_key("lock"));
    assert!(client.contains_key("config"));
    let (ino, fh) = create(&fs, "empty.lock", OpenFlags::O_WRONLY).await;
    fs.flush(ino, fh, 0, 0).await.unwrap();
    fs.rename(1, "empty.lock".as_ref(), 1, "empty".as_ref(), RenameFlags::empty())
        .await
        .unwrap();
    fs.release(ino, fh, 0, None, false).await.unwrap();
    assert!(!client.contains_key("empty.lock"));
    assert!(client.contains_key("empty"));
    let (ino, fh) = create(&fs, "delete.lock", OpenFlags::O_WRONLY).await;
    fs.flush(ino, fh, 0, 0).await.unwrap();
    fs.unlink(1, "delete.lock".as_ref()).await.unwrap();
    fs.release(ino, fh, 0, None, false).await.unwrap();
    assert!(!client.contains_key("delete.lock"));
}

#[tokio::test]
async fn directory_markers_survive_last_child_removal_and_remount() {
    let dir = tempfile::tempdir().unwrap();
    let (client, fs) = setup(dir.path());
    let refs = fs.mkdir(1, "refs".as_ref(), 0o755, 0).await.unwrap().attr.ino;
    fs.mkdir(refs, "tags".as_ref(), 0o755, 0).await.unwrap();
    let ino = fs
        .mknod(refs, "main".as_ref(), libc::S_IFREG | 0o600, 0, 0)
        .await
        .unwrap()
        .attr
        .ino;
    let fh = fs.open(ino, OpenFlags::O_WRONLY, 0).await.unwrap().fh;
    fs.write(ino, fh, 0, b"sha", 0, 0, None).await.unwrap();
    fs.flush(ino, fh, 0, 0).await.unwrap();
    fs.release(ino, fh, 0, None, false).await.unwrap();
    assert_eq!(
        fs.rmdir(1, "refs".as_ref()).await.unwrap_err().to_errno(),
        libc::ENOTEMPTY
    );
    fs.unlink(refs, "main".as_ref()).await.unwrap();
    let fresh = make_test_filesystem_with_client(
        client.clone(),
        PagedPool::new_with_candidate_sizes([1024 * 1024]),
        "bucket",
        &Default::default(),
        config(dir.path()),
    );
    let refs = fresh.lookup(1, "refs".as_ref()).await.unwrap().attr.ino;
    fresh.lookup(refs, "tags".as_ref()).await.unwrap();
    fresh.rmdir(refs, "tags".as_ref()).await.unwrap();
    fresh.rmdir(1, "refs".as_ref()).await.unwrap();
    assert!(!client.contains_key("refs/"));
    assert!(!client.contains_key("refs/tags/"));
}

#[tokio::test]
async fn failed_upload_is_reported_by_close() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(MockClient::config().bucket("bucket").part_size(1024 * 1024).build());
    let failure = countdown_failure_client(
        client.clone(),
        CountdownFailureConfig {
            put_failures: [(
                1,
                Err(mountpoint_s3_client::error::ObjectClientError::ClientError(
                    MockClientError("injected upload failure".into()),
                )),
            )]
            .into(),
            ..Default::default()
        },
    );
    let fs = make_test_filesystem_with_client(
        Arc::new(failure),
        PagedPool::new_with_candidate_sizes([1024 * 1024]),
        "bucket",
        &Default::default(),
        config(dir.path()),
    );
    let ino = fs
        .mknod(1, "pack".as_ref(), libc::S_IFREG | 0o600, 0, 0)
        .await
        .unwrap()
        .attr
        .ino;
    let fh = fs.open(ino, OpenFlags::O_RDWR, 0).await.unwrap().fh;
    fs.write(ino, fh, 0, b"contents", 0, 0, None).await.unwrap();
    assert_eq!(fs.flush(ino, fh, 0, 0).await.unwrap_err().to_errno(), libc::EIO);
    assert!(!client.contains_key("pack"));
    assert!(
        fs.rename(1, "pack".as_ref(), 1, "final".as_ref(), RenameFlags::empty())
            .await
            .is_err()
    );
    assert!(!client.contains_key("final"));
    let _ = fs.release(ino, fh, 0, None, false).await;
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn truncate_replaces_existing_contents() {
    let dir = tempfile::tempdir().unwrap();
    let (client, fs) = setup(dir.path());
    let (ino, fh) = create(&fs, "existing", OpenFlags::O_RDWR).await;
    fs.write(ino, fh, 0, b"long previous contents", 0, 0, None)
        .await
        .unwrap();
    fs.flush(ino, fh, 0, 0).await.unwrap();
    fs.release(ino, fh, 0, None, false).await.unwrap();
    let fh = fs
        .open(ino, OpenFlags::O_RDWR | OpenFlags::O_TRUNC, 0)
        .await
        .unwrap()
        .fh;
    fs.write(ino, fh, 0, b"new", 0, 0, None).await.unwrap();
    assert_eq!(fs.getattr(ino).await.unwrap().attr.size, 3);
    fs.fsync(ino, fh, false).await.unwrap();
    fs.write(ino, fh, 1, b"EW", 0, 0, None).await.unwrap();
    fs.flush(ino, fh, 0, 0).await.unwrap();
    fs.release(ino, fh, 0, None, false).await.unwrap();
    let fresh = make_test_filesystem_with_client(
        client,
        PagedPool::new_with_candidate_sizes([1024 * 1024]),
        "bucket",
        &Default::default(),
        config(dir.path()),
    );
    let ino = fresh.lookup(1, "existing".as_ref()).await.unwrap().attr.ino;
    let fh = fresh.open(ino, OpenFlags::empty(), 0).await.unwrap().fh;
    assert_eq!(fresh.read(ino, fh, 0, 100, 0, None).await.unwrap(), &b"nEW"[..]);
    fresh.release(ino, fh, 0, None, false).await.unwrap();
}

#[tokio::test]
async fn failed_fsync_can_be_retried_without_losing_data() {
    let dir = tempfile::tempdir().unwrap();
    let (client, _) = setup(dir.path());
    let failure = countdown_failure_client(
        client.clone(),
        CountdownFailureConfig {
            put_failures: [(
                1,
                Err(mountpoint_s3_client::error::ObjectClientError::ClientError(
                    MockClientError("injected".into()),
                )),
            )]
            .into(),
            ..Default::default()
        },
    );
    let fs = make_test_filesystem_with_client(
        Arc::new(failure),
        PagedPool::new_with_candidate_sizes([1024 * 1024]),
        "bucket",
        &Default::default(),
        config(dir.path()),
    );
    let ino = fs
        .mknod(1, "pack".as_ref(), libc::S_IFREG | 0o600, 0, 0)
        .await
        .unwrap()
        .attr
        .ino;
    let fh = fs.open(ino, OpenFlags::O_RDWR, 0).await.unwrap().fh;
    fs.write(ino, fh, 0, b"contents", 0, 0, None).await.unwrap();
    assert_eq!(fs.fsync(ino, fh, false).await.unwrap_err().to_errno(), libc::EIO);
    assert!(!client.contains_key("pack"));
    assert_eq!(fs.read(ino, fh, 0, 100, 0, None).await.unwrap(), &b"contents"[..]);
    fs.fsync(ino, fh, false).await.unwrap();
    assert!(client.contains_key("pack"));
    fs.flush(ino, fh, 0, 0).await.unwrap();
    fs.release(ino, fh, 0, None, false).await.unwrap();
}
