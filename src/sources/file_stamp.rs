use std::{path::Path, time::SystemTime};

/// Identity captured before opening a reader that uses an immutable local file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FileStamp {
    len: u64,
    pub(super) modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl FileStamp {
    pub(super) fn read(path: &Path) -> std::io::Result<Self> {
        Self::from_metadata(&std::fs::metadata(path)?)
    }

    pub(super) fn from_metadata(meta: &std::fs::Metadata) -> std::io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Ok(Self {
            len: meta.len(),
            modified: meta.modified()?,
            #[cfg(unix)]
            identity: (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()),
        })
    }
}
