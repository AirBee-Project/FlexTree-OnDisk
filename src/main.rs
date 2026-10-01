use FlexTree_OnDisk::disk::{Page, open_direct};
use compio::BufResult;
use compio::fs::OpenOptions;
use compio::io::{AsyncReadAtExt, AsyncWriteAtExt};

#[compio::main]
async fn main() -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);

    let mut file = open_direct("data.bin", &options).await?;

    let mut page = Page::new();
    page[..5].copy_from_slice(b"hello");
    file.write_all_at(page, 0).await.0?;

    let BufResult(result, page) = file.read_exact_at(Page::new(), 0).await;
    result?;
    println!("{}", String::from_utf8_lossy(&page[..5]));

    Ok(())
}
