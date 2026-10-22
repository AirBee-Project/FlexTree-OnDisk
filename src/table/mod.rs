use compio::fs::{File, OpenOptions};
use spatial_id::FlexId;
use std::path::Path;

use crate::{Error, disk::open_direct};

pub struct Table {
    file: File,
}

impl Table {
    /// 新しい[Table]を作成する。`path`に既にファイルが存在する場合にはファイルを開く。
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        let file = open_direct(path, &options).await?;
        Ok(Table { file })
    }

    /// `target` の領域に `value` を書き込む。既に値がある場所は上書きされる。
    pub async fn insert(
        &mut self,
        target: impl IntoIterator<Item = FlexId>,
        value: u64,
    ) -> Result<(), Error> {
        todo!();
    }
}
