# Mountpoint S3

This repository contains Modal's patched version of
[mountpoint-s3](https://github.com/awslabs/mountpoint-s3).

## Workflow

We create a new branch per release with all patches stacked as commits (e.g.,
`modal/mountpoint-s3-1.21.0`).

When updating, create a new branch and rebase our patches onto the new base.

```bash
# Update from upstream
git remote add upstream https://github.com/awslabs/mountpoint-s3.git
git fetch upstream
# Rebase onto specific version
git rebase --onto upstream/v1.22.0 upstream/v1.21.0 modal/mountpoint-s3-1.22.0
```

## Contribution

Send all patches that can be useful to others upstream.
