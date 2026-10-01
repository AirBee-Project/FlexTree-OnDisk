use std::io;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};
use std::path::Path;

use compio::buf::{IoBuf, IoBufMut, SetLen};
use compio::fs::{File, OpenOptions};

/// DirectI/O でファイルを開く。
pub async fn open_direct(path: impl AsRef<Path>, options: &OpenOptions) -> io::Result<File> {
    let mut options = options.clone();
    #[cfg(not(any(target_vendor = "apple", windows)))]
    options.custom_flags(libc::O_DIRECT);

    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING);
    let file = options.open(path).await?;

    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(file)
}

#[repr(C, align(4096))]
pub struct Page([u8; Page::SIZE]);

const _: () = assert!(align_of::<Page>() == Page::SIZE);

impl Page {
    pub const SIZE: usize = 4096;

    pub fn new() -> Box<Self> {
        unsafe { Box::new_zeroed().assume_init() }
    }
}

impl Deref for Page {
    type Target = [u8; Self::SIZE];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Page {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl IoBuf for Page {
    fn as_init(&self) -> &[u8] {
        &self.0
    }
}

impl IoBufMut for Page {
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        self.0.as_uninit()
    }
}

impl SetLen for Page {
    unsafe fn set_len(&mut self, len: usize) {
        debug_assert!(len <= Self::SIZE);
    }
}
