use std::ops::Range;
use std::path::PathBuf;

use kasane_logic::spatial_id::collection::flex_tree_2::FlexTreeCore2;
use kasane_logic::{FlexId, RangeId, SingleId};

use super::*;

/// テストごとに別のファイルを用意する。終わったら消す。
struct TempFile(PathBuf);

impl TempFile {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("flextree-ondisk-{name}.kasane"));
        let _ = std::fs::remove_file(&path);
        TempFile(path)
    }
}

impl TempFile {
    /// `page_index` 番目のページを `f` で書き換える。落ちて書き込みが欠けた状態を作るのに使う。
    fn modify_page(&self, page_index: u64, f: impl FnOnce(&mut [u8])) {
        let mut bytes = std::fs::read(&self.0).unwrap();
        f(&mut bytes[page_index as usize * Page::SIZE..][..Page::SIZE]);
        std::fs::write(&self.0, bytes).unwrap();
    }
}

/// 書き換えたページのチェックサムを付け直す。
fn reseal(page: &mut [u8]) {
    let checksum = crc32c::crc32c(&page[..Page::CAPACITY]);
    page[Page::CAPACITY..].copy_from_slice(&checksum.to_le_bytes());
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 値を持つ領域を、FlexTreeCore2 の `iter` と同じ順で返す。
async fn entries(table: &Table) -> Vec<(FlexId, u64)> {
    let mut out = Vec::new();
    // 後に積んだものから取り出すので、ROOTS の先頭が最後になるよう逆順に積む
    let mut stack: Vec<_> = table
        .roots
        .iter()
        .cloned()
        .zip(FlexId::ROOTS)
        .rev()
        .collect();
    while let Some((node, this)) = stack.pop() {
        match node {
            Node::Empty => {}
            Node::Leaf(value) => out.push((this, value)),
            Node::Stored(stored) => stack.push((table.read(stored).await.unwrap(), this)),
            Node::Branch(branch) => {
                let half = |side| this.split_on(branch.dimension, side).unwrap();
                stack.push((branch.upper, half(Side::Upper)));
                stack.push((branch.lower, half(Side::Lower)));
            }
            Node::Skip(skip) => stack.push((skip.child, skip.path.to_absolute(&this).unwrap())),
        }
    }
    out
}

/// 多数の Branch・Skip ができるよう、市松模様に並べた ID。
fn checkerboard() -> Vec<FlexId> {
    (0..16u32)
        .flat_map(|x| (0..16u32).map(move |y| (x, y)))
        .filter(|(x, y)| (x + y) % 2 == 0)
        .flat_map(|(x, y)| SingleId::new(4, 3, x, y).unwrap())
        .collect()
}

fn expected(tree: &FlexTreeCore2<u64>) -> Vec<(FlexId, u64)> {
    tree.iter().map(|(id, value)| (id, *value)).collect()
}

#[compio::test]
async fn single_insert_round_trips() {
    let file = TempFile::new("single");
    let id = SingleId::new(13, 4, 4, 4).unwrap();
    {
        let mut table = Table::open(&file.0).await.unwrap();
        table.insert(id.clone(), 7).await.unwrap();
    }
    let table = Table::open(&file.0).await.unwrap();
    let mut tree = FlexTreeCore2::<u64>::default();
    tree.insert(id, 7);
    assert_eq!(entries(&table).await, expected(&tree));
}

#[compio::test]
async fn matches_in_memory_tree() {
    let file = TempFile::new("random");
    let mut table = Table::open(&file.0).await.unwrap();
    let mut tree = FlexTreeCore2::<u64>::default();

    // 重なりや上書きが起きるよう、狭い範囲から乱数で取る
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound
    };
    for step in 0..300 {
        let z = next(5) as u8 + 1;
        let size = 1u64 << z;
        let f = next(size) as i32 - (size / 2) as i32;
        let (x, y) = (next(size) as u32, next(size) as u32);
        let value = next(4);
        if step % 3 == 0 {
            let x2 = (x + next(3) as u32).min(size as u32 - 1);
            let range = RangeId::new(z, [f, f], [x, x2], [y, y]).unwrap();
            table.insert(range.clone(), value).await.unwrap();
            tree.insert(range, value);
        } else {
            let id = SingleId::new(z, f, x, y).unwrap();
            table.insert(id.clone(), value).await.unwrap();
            tree.insert(id, value);
        }
        assert_eq!(entries(&table).await, expected(&tree), "step {step}");
    }

    drop(table);
    let table = Table::open(&file.0).await.unwrap();
    assert_eq!(entries(&table).await, expected(&tree));
}

/// ディスク上の Branch・Skip の数。
async fn count_records(table: &Table) -> usize {
    let mut count = 0;
    let mut stack: Vec<Node> = table.roots.to_vec();
    while let Some(node) = stack.pop() {
        match node {
            Node::Empty | Node::Leaf(_) => {}
            Node::Stored(stored) => stack.push(table.read(stored).await.unwrap()),
            Node::Branch(branch) => {
                count += 1;
                stack.extend([branch.lower, branch.upper]);
            }
            Node::Skip(skip) => {
                count += 1;
                stack.push(skip.child);
            }
        }
    }
    count
}

#[compio::test]
async fn packs_subtree_into_pages() {
    let file = TempFile::new("pack");
    let mut table = Table::open(&file.0).await.unwrap();
    let mut tree = FlexTreeCore2::<u64>::default();

    let ids = checkerboard();
    table.insert(ids.clone(), 1).await.unwrap();
    tree.insert(ids, 1);

    let records = count_records(&table).await;
    let pages = (table.page_count - HEADER_PAGES) as usize;
    assert!(records > RECORDS_PER_PAGE * 2, "records {records}");
    assert!(
        pages <= records.div_ceil(RECORDS_PER_PAGE) + 1,
        "records {records}, pages {pages}"
    );

    drop(table);
    let table = Table::open(&file.0).await.unwrap();
    assert_eq!(entries(&table).await, expected(&tree));
}

#[compio::test]
async fn small_insert_shares_unchanged_records() {
    let file = TempFile::new("share");
    let mut table = Table::open(&file.0).await.unwrap();
    let mut tree = FlexTreeCore2::<u64>::default();

    let ids = checkerboard();
    table.insert(ids.clone(), 1).await.unwrap();
    tree.insert(ids, 1);

    // 変わるのは根から書き込み先までの経路だけなので、1 ページに収まる
    let before = table.page_count;
    let id = SingleId::new(4, 3, 0, 1).unwrap();
    table.insert(id.clone(), 2).await.unwrap();
    tree.insert(id, 2);
    assert_eq!(table.page_count - before, 1);
    assert_eq!(entries(&table).await, expected(&tree));
}

#[compio::test]
async fn deepest_path_round_trips() {
    let file = TempFile::new("deep");
    let mut table = Table::open(&file.0).await.unwrap();
    let mut tree = FlexTreeCore2::<u64>::default();

    let max = (1u32 << 30) - 1;
    let id = FlexId::new(30, max as i32, 30, max, 30, max)
        .unwrap()
        .with_time(35, (1 << 35) - 1)
        .unwrap();
    table.insert(id, 3).await.unwrap();
    tree.insert(id, 3);

    drop(table);
    let table = Table::open(&file.0).await.unwrap();
    assert_eq!(entries(&table).await, expected(&tree));
}

/// 2 回 insert して、1 回目だけを反映した FlexTreeCore2 と、2 回目の世代で書いたページの範囲を返す。
async fn two_generations(file: &TempFile) -> (FlexTreeCore2<u64>, Range<u64>) {
    let mut table = Table::open(&file.0).await.unwrap();
    let mut tree = FlexTreeCore2::<u64>::default();
    let first = checkerboard();
    table.insert(first.clone(), 1).await.unwrap();
    tree.insert(first, 1);
    let start = table.page_count;
    table
        .insert(SingleId::new(4, 3, 0, 1).unwrap(), 2)
        .await
        .unwrap();
    (tree, start..table.page_count)
}

/// 開き直して `tree` と一致すること、その後も書き足せることを確かめる。
async fn assert_recovers(file: &TempFile, mut tree: FlexTreeCore2<u64>) {
    let mut table = Table::open(&file.0).await.unwrap();
    assert_eq!(entries(&table).await, expected(&tree));

    let id = SingleId::new(4, -3, 5, 5).unwrap();
    table.insert(id.clone(), 3).await.unwrap();
    tree.insert(id, 3);
    drop(table);
    let table = Table::open(&file.0).await.unwrap();
    assert_eq!(entries(&table).await, expected(&tree));
}

#[compio::test]
async fn falls_back_when_newest_header_is_torn() {
    let file = TempFile::new("torn-header");
    let (tree, _) = two_generations(&file).await;
    // 2 世代目のヘッダーはページ 0
    file.modify_page(0, |page| page[0] ^= 1);
    assert_recovers(&file, tree).await;
}

#[compio::test]
async fn falls_back_when_newest_pages_are_missing() {
    let file = TempFile::new("torn-pages");
    let (tree, pages) = two_generations(&file).await;
    file.modify_page(pages.start, |page| page.fill(0));
    assert_recovers(&file, tree).await;
}

#[compio::test]
async fn never_overwrites_pages_of_unfinished_generation() {
    let file = TempFile::new("truncated");
    let (tree, pages) = two_generations(&file).await;
    // 2 世代目のページがファイルを伸ばす前に落ちた状態
    let len = pages.start * Page::SIZE as u64;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&file.0)
        .unwrap()
        .set_len(len)
        .unwrap();
    let table = Table::open(&file.0).await.unwrap();
    assert!(table.page_count >= pages.end);
    drop(table);
    assert_recovers(&file, tree).await;
}

#[compio::test]
async fn rejects_when_no_header_is_valid() {
    let file = TempFile::new("no-header");
    two_generations(&file).await;
    for page_index in 0..HEADER_PAGES {
        file.modify_page(page_index, |page| page[0] ^= 1);
    }
    assert!(matches!(Table::open(&file.0).await, Err(Error::Corrupt)));
}

#[compio::test]
async fn rejects_unknown_tag() {
    let file = TempFile::new("unknown-tag");
    let mut table = Table::open(&file.0).await.unwrap();
    table
        .insert(SingleId::new(4, 3, 0, 1).unwrap(), 1)
        .await
        .unwrap();
    let Node::Stored(root) = table.roots[0] else {
        panic!("ルートはディスク上にある")
    };
    drop(table);
    // チェックサムは正しいが、根のレコードのタグが不正
    file.modify_page(root.page, |page| {
        page[0] = u8::MAX;
        reseal(page);
    });
    let table = Table::open(&file.0).await.unwrap();
    assert!(matches!(table.read(root).await, Err(Error::Corrupt)));
}
