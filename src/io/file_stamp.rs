use std::{fs, io, time::SystemTime};

/// Identity captured before opening a reader that uses an immutable local file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileStamp {
    len: u64,
    pub(crate) modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
    #[cfg(windows)]
    identity: (u64, [u8; 16], i64),
}

impl FileStamp {
    #[cfg(any(
        feature = "mbtiles",
        feature = "duckdb",
        feature = "parquet-archive",
        feature = "agtiff"
    ))]
    pub(crate) fn read(path: &std::path::Path) -> io::Result<Self> {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;

            let file = fs::OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES)
                .open(path)?;
            Self::from_file(&file)
        }
        #[cfg(not(windows))]
        Self::from_metadata(&fs::metadata(path)?)
    }

    #[cfg(all(not(windows), any(feature = "pmtiles", feature = "parquet-archive")))]
    pub(crate) fn from_file(file: &fs::File) -> io::Result<Self> {
        Self::from_metadata(&file.metadata()?)
    }

    #[cfg(not(windows))]
    fn from_metadata(meta: &fs::Metadata) -> io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Ok(Self {
            len: meta.len(),
            modified: meta.modified()?,
            #[cfg(unix)]
            identity: (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()),
        })
    }

    #[cfg(windows)]
    pub(crate) fn from_file(file: &fs::File) -> io::Result<Self> {
        use std::mem::size_of;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FILE_ID_INFO, FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx,
        };

        let meta = file.metadata()?;
        let handle = file.as_raw_handle();
        let mut file_id = FILE_ID_INFO::default();
        let mut basic = FILE_BASIC_INFO::default();
        // SAFETY: the open handle outlives both calls and each buffer matches its information class.
        unsafe {
            if GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                (&raw mut file_id).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            ) == 0
                || GetFileInformationByHandleEx(
                    handle,
                    FileBasicInfo,
                    (&raw mut basic).cast(),
                    size_of::<FILE_BASIC_INFO>() as u32,
                ) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self {
            len: meta.len(),
            modified: meta.modified()?,
            identity: (
                file_id.VolumeSerialNumber,
                file_id.FileId.Identifier,
                basic.ChangeTime,
            ),
        })
    }

    #[cfg(feature = "mbtiles")]
    pub(crate) fn len(&self) -> u64 {
        self.len
    }
}
