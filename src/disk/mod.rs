use std::io;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};
use std::path::Path;

use bytemuck::{CheckedBitPattern, NoUninit, Zeroable};
use compio::buf::{IoBuf, IoBufMut, SetLen};
use compio::fs::{File, OpenOptions};
use compio::io::{AsyncReadAtExt, AsyncWriteAtExt};

use crate::Error;

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
#[derive(Debug, Zeroable)]
pub struct Page([u8; Page::SIZE]);

const _: () = assert!(align_of::<Page>() == Page::SIZE);

impl Page {
    pub const SIZE: usize = 4096;

    /// 中身に使える大きさ。末尾の 4 バイトは中身の CRC32C。
    pub const CAPACITY: usize = Self::SIZE - size_of::<u32>();

    pub fn new() -> Box<Self> {
        bytemuck::zeroed_box()
    }

    /// 先頭に `value` を書き、残りをゼロで埋めて、末尾にチェックサムを付けたページ。
    pub fn of<T: NoUninit>(value: &T) -> Box<Self> {
        const { assert!(size_of::<T>() <= Page::CAPACITY) };
        let bytes = bytemuck::bytes_of(value);
        let mut page = Self::new();
        page[..bytes.len()].copy_from_slice(bytes);
        let checksum = page.checksum();
        page[Self::CAPACITY..].copy_from_slice(&checksum.to_le_bytes());
        page
    }

    /// 先頭を `T` として読む。チェックサムが合わない、または `T` として正しくないならエラーを返す。
    pub fn try_head_as<T: CheckedBitPattern>(&self) -> Result<&T, Error> {
        // 1Pageに収まることをコンパイル時に検証
        const { assert!(size_of::<T>() <= Page::CAPACITY) };

        if self[Self::CAPACITY..] != self.checksum().to_le_bytes() {
            return Err(Error::Corrupt);
        }
        Ok(bytemuck::checked::try_from_bytes(&self[..size_of::<T>()])?)
    }

    /// `file` の `page_index` 番目のページを読む。ファイルがそこまで無ければ [`None`]。
    pub async fn read_at_page(file: &File, page_index: u64) -> io::Result<Option<Box<Self>>> {
        let (result, page) = file
            .read_exact_at(Self::new(), page_index * Self::SIZE as u64)
            .await
            .into();
        match result {
            Ok(()) => Ok(Some(page)),
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// `file` の `page_index` 番目のページに書く。
    pub async fn write_at_page(
        self: Box<Self>,
        file: &mut File,
        page_index: u64,
    ) -> io::Result<()> {
        file.write_all_at(self, page_index * Self::SIZE as u64)
            .await
            .0
    }

    fn checksum(&self) -> u32 {
        crc32c::crc32c(&self[..Self::CAPACITY])
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
