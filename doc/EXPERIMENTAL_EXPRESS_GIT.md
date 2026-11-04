# Experimental Git support on S3 Express

`--experimental-write-cache DIR` adds local staging for new/truncated files and
persistent directory markers. It requires an S3 Express directory bucket and
`--incremental-upload --allow-overwrite --allow-delete`. It is off by default.
This is not a general POSIX filesystem mode.

```sh
mkdir -p /mnt/repo /tmp/s3-write-staging
mount-s3 --allow-delete --allow-overwrite --incremental-upload \
  --experimental-write-cache /tmp/s3-write-staging \
  --prefix workspace/ BUCKET /mnt/repo
```

Use a local directory outside the mount for staging. Size its disk for all
simultaneously open staged files, including Git pack files. The ordinary `--cache`
option remains a separate read cache. This binary can run in a Modal VM Sandbox
with FUSE; using it in `CloudBucketMount` requires a runtime flag and binary rollout.

## Why these changes are needed

* Express has native atomic file rename and append. `--incremental-upload` enables
  append for Git reflogs. Close-to-rename/unlink also needs to complete any pending
  upload (the fix in #24).
* `git index-pack` reads its output before closing it. Thin-pack repair also rewrites
  the header. A local backing file supports these reads and positional writes;
  the existing uploader publishes the file to S3 on fsync/close.
* S3 does not retain empty directories automatically. `mkdir` writes a zero-byte
  trailing-slash marker, so `.git/refs/tags` and `refs` survive pruning the last
  loose ref and mounting in a different sandbox. `rmdir` checks for children before
  deleting only that marker. Marker requests carry the mount's SSE/KMS settings.

Markers are written for directories created in this mode. Existing implicit
directories are not migrated; use a fresh repository/prefix for the trial.

## Persistence and limits

A successful fsync uploads a snapshot without closing the staged writer. Later
writes require another fsync or close. Upload errors are returned by fsync/flush;
applications must check them. As with existing Mountpoint behavior, final FUSE
release errors cannot be returned to an application that has already closed its
file descriptor.

Staging files are anonymous and temporary. Killing the mount or sandbox can lose
writes that have not been uploaded. There is no recovery journal, multi-file
transaction, or guarantee that killing Git midway through a commit leaves a
usable repository. A successful Git command is not a guarantee for unrelated open
files. This option does not provide a shared writable repository: use one writer
per repository. Directory empty-check plus marker deletion is not atomic across
mounts.

Staging applies to new/truncated files opened for writing without `O_APPEND`.
Existing nonempty files still require truncate-and-rewrite or append; arbitrary
in-place editing of an existing object is unsupported. Native append keeps its
existing restrictions. Hard links, symlinks, directory rename, executable-bit
persistence, and general SQLite/database workloads remain unsupported. Git can
use fallbacks for some of these; this does not imply every Git command works.

The mode retains Mountpoint's existing duplicated-descriptor close restrictions:
closing a descriptor after writing may complete the writer before a duplicate is
finished. It supports the existing close-before-first-write and child-process
close handling.

## Validation

Regression tests:

```sh
cargo test -p mountpoint-s3-fs --test express_staging
```

See the PR description for measured real-S3 results and remaining limitations.
