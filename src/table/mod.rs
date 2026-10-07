use std::borrow::Cow;
use std::cmp::Reverse;
use std::mem;
use std::ops::Range;
use std::path::Path;

use bytemuck::Zeroable;
use compio::fs::{File, OpenOptions};
use kasane_logic::{Dimension, FlexId, Side};

use crate::{
    Error,
    disk::{Page, open_direct},
};
use node::{
    Branch, Kind, Node, Part, RECORDS_PER_PAGE, RawHeader, RawNode, RawPage, RawRecord, Skip,
    Stored, common_descent,
};

mod node;
#[cfg(test)]
mod tests;

/// ヘッダーを交互に書くページの数。世代 `g` のヘッダーはページ `g % HEADER_PAGES` に書く。
const HEADER_PAGES: u64 = 2;

/// 空間IDと値を対応させた[SpatialIdTable](kasane_logic::SpatialIdTable)のOnDisk実装。
///
/// 先頭の [`HEADER_PAGES`] ページは、世代ごとに交互に書くヘッダー。それ以降の各ページには、つながった部分木の
/// Branch・Skip を [`RECORDS_PER_PAGE`] 個まで詰める。新しく作った Branch・Skip だけを新しいページへ詰めて追記し、
/// 次の世代のヘッダーを書いて反映する（CoW）。変えていない部分は、他のページの途中も含めて元の場所を指す。
///
/// どのページも末尾にチェックサムを持つ。書き込みの途中で落ちると、新しい世代のヘッダーかその世代のページが
/// 欠けるので、開くときに検証が通る一番新しい世代へ戻る。
pub struct Table {
    file: File,
    /// [`FlexId::ROOTS`] の各領域のルート。
    roots: [Node; 2],
    generation: u64,
    /// 次に書くページの番号。どのヘッダーの範囲とも重ならないよう、ファイルの末尾より後ろだけを使う。
    page_count: u64,
}

impl Table {
    /// 新しい[Table]を作成する。`path`に既にファイルが存在する場合にはファイルを開く。
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        let file = open_direct(path, &options).await?;

        let len = file.metadata().await?.len().div_ceil(Page::SIZE as u64);

        let mut headers = Vec::new();
        for page_index in 0..HEADER_PAGES {
            if let Some(page) = Page::read_at_page(&file, page_index).await?
                && let Ok(header) = page.try_head_as::<RawHeader>()
            {
                headers.push(*header);
            }
        }
        // 書きかけの世代のページも含め、どのヘッダーが指す範囲にも新しいページを書かない。
        // 重ねて書くと、その古いヘッダーが新しいページを自分のものとして検証を通してしまう。
        let page_count = headers
            .iter()
            .map(|header| header.end)
            .fold(len.max(HEADER_PAGES), u64::max);
        headers.sort_by_key(|header| Reverse(header.generation));
        for header in headers {
            if intact(&file, header.start..header.end).await? {
                return Ok(Table {
                    file,
                    roots: [header.upper.try_into()?, header.lower.try_into()?],
                    generation: header.generation,
                    page_count,
                });
            }
        }

        // ヘッダー以外のページが無いなら、まだ何も書いていないので空の表として作り直してよい
        if len > HEADER_PAGES {
            return Err(Error::Corrupt);
        }
        let mut table = Table {
            file,
            roots: [Node::Empty, Node::Empty],
            generation: 0,
            page_count: HEADER_PAGES,
        };
        table
            .write_header(RawHeader {
                upper: RawNode::EMPTY,
                lower: RawNode::EMPTY,
                generation: 0,
                start: HEADER_PAGES,
                end: HEADER_PAGES,
            })
            .await?;
        Ok(table)
    }

    /// `target` の領域に `value` を書き込む。既に値がある場所は上書きされる。
    pub async fn insert(
        &mut self,
        target: impl IntoIterator<Item = FlexId>,
        value: u64,
    ) -> Result<(), Error> {
        // 途中で失敗しても手元のルートが壊れないよう、複製に書き込んでから差し替える
        let mut roots = self.roots.clone();
        for id in target {
            let i = id.root_index();
            roots[i] = self
                .write(FlexId::ROOTS[i], mem::take(&mut roots[i]), id, value)
                .await?;
        }
        self.commit(roots).await
    }

    /// メモリ上の Node を新しいページへ書き、それを指す次の世代のヘッダーを書く。
    async fn commit(&mut self, [upper, lower]: [Node; 2]) -> Result<(), Error> {
        let start = self.page_count;
        let header = RawHeader {
            upper: self.store(upper).await?,
            lower: self.store(lower).await?,
            generation: self.generation + 1,
            start,
            end: self.page_count,
        };
        self.write_header(header).await?;
        self.generation = header.generation;
        self.roots = [header.upper.try_into()?, header.lower.try_into()?];
        Ok(())
    }

    /// `header` をその世代の場所に書き、それまでに書いたページとあわせて永続化する。
    ///
    /// ページとヘッダーの永続化の順序は問わない。どちらかが欠けたまま落ちても、開くときの検証で前の世代へ戻る。
    async fn write_header(&mut self, header: RawHeader) -> Result<(), Error> {
        Page::of(&header)
            .write_at_page(&mut self.file, header.generation % HEADER_PAGES)
            .await?;
        self.file.sync_data().await?;
        Ok(())
    }

    /// `node` のうち新しく作った部分を新しいページへ詰めて書き、`node` を指す RawNode を返す。
    async fn store(&mut self, node: Node) -> Result<RawNode, Error> {
        match self.layout(node).await? {
            Part::Done(node) => Ok(node),
            Part::Open(records) => self.write_page(records).await,
        }
    }

    /// `node` のうち新しく作った部分を、子の側からページに割り付ける。根の側の書いていない部分を返す。
    async fn layout(&mut self, node: Node) -> Result<Part, Error> {
        Ok(match node {
            Node::Empty => Part::Done(RawNode::EMPTY),
            Node::Leaf(value) => Part::Done(RawNode::leaf(value)),
            Node::Stored(stored) => Part::Done(stored.into()),
            Node::Branch(branch) => match *branch {
                Branch {
                    stored: Some(stored),
                    ..
                } => Part::Done(stored.into()),
                Branch {
                    dimension,
                    split_dimensions,
                    lower,
                    upper,
                    stored: None,
                } => {
                    let children = [
                        Box::pin(self.layout(lower)).await?,
                        Box::pin(self.layout(upper)).await?,
                    ];
                    let (mut records, [lower, upper]) = self.pack(children).await?;
                    records[0] = RawRecord::branch(dimension, split_dimensions, lower, upper);
                    Part::Open(records)
                }
            },
            Node::Skip(skip) => match *skip {
                Skip {
                    stored: Some(stored),
                    ..
                } => Part::Done(stored.into()),
                Skip {
                    path,
                    split_dimensions,
                    child,
                    stored: None,
                } => {
                    let children = [Box::pin(self.layout(child)).await?];
                    let (mut records, [child]) = self.pack(children).await?;
                    records[0] = RawRecord::skip(path, split_dimensions, child);
                    Part::Open(records)
                }
            },
        })
    }

    /// 根と `children` の書いていない部分が 1 ページに収まるまで、大きい子から順にページとして書いて閉じ、
    /// 残りを根の後ろに並べる。根の場所を空けたレコードの列と、根から各子への参照を返す。
    ///
    /// 子の側からこうして閉じていくと、ページ数が最小になる。
    async fn pack<const N: usize>(
        &mut self,
        mut children: [Part; N],
    ) -> Result<(Vec<RawRecord>, [RawNode; N]), Error> {
        while 1 + children.iter().map(Part::len).sum::<usize>() > RECORDS_PER_PAGE {
            let largest = children.iter_mut().max_by_key(|part| part.len()).unwrap();
            if let Part::Open(records) = largest {
                *largest = Part::Done(self.write_page(mem::take(records)).await?);
            }
        }
        let mut records = vec![RawRecord::zeroed()];
        let nodes = children.map(|child| match child {
            Part::Done(node) => node,
            Part::Open(part) => {
                let node = RawNode::local(records.len());
                records.extend(part);
                node
            }
        });
        Ok((records, nodes))
    }

    /// 先頭を根とするレコードの列を新しいページに書き、そのページを指す RawNode を返す。
    async fn write_page(&mut self, records: Vec<RawRecord>) -> Result<RawNode, Error> {
        let page_index = self.page_count;
        let node = records[0].stored_at(page_index, 0)?.into();
        let mut page = RawPage::zeroed();
        page.records[..records.len()].copy_from_slice(&records);
        Page::of(&page)
            .write_at_page(&mut self.file, page_index)
            .await?;
        self.page_count += 1;
        Ok(node)
    }

    /// `stored` のページを読み、そこから下でそのページにある部分木をメモリ上に展開する。ページの外の子は [`Node::Stored`] のまま。
    async fn read(&self, stored: Stored) -> Result<Node, Error> {
        Page::read_at_page(&self.file, stored.page)
            .await?
            .ok_or(Error::Corrupt)?
            .try_head_as::<RawPage>()?
            .decode(stored.page, usize::from(stored.slot))
    }

    /// `node` が `kind` の [`Node::Stored`] なら読み込む。
    async fn load(&self, node: Node, kind: Kind) -> Result<Node, Error> {
        match node {
            Node::Stored(stored) if stored.kind == kind => self.read(stored).await,
            node => Ok(node),
        }
    }

    /// 領域 `this` の `node` のうち、`region` を `value` で上書きした Node を返す。
    async fn write(
        &self,
        this: FlexId,
        node: Node,
        region: FlexId,
        value: u64,
    ) -> Result<Node, Error> {
        if region == this {
            return Ok(Node::Leaf(value));
        }
        let node = match self.load(node, Kind::Skip).await? {
            Node::Empty => {
                return Ok(Skip::new(region.relative_to(&this)?, Node::Leaf(value)).into());
            }
            // 行き先が分かれるところまで1段ずつ割らずに一気に降りる
            Node::Skip(skip) => {
                let target = skip.path.to_absolute(&this)?;
                let at = common_descent(&this, &target, skip.child.split_dimensions(), &region);
                if at == target {
                    let child = Box::pin(self.write(at, skip.child, region, value)).await?;
                    return self.rebase(child, &at, &this).await;
                }
                if at != this {
                    let inner = self.rebase(Node::Skip(skip), &this, &at).await?;
                    let inner = Box::pin(self.write(at, inner, region, value)).await?;
                    return self.rebase(inner, &at, &this).await;
                }
                Node::Skip(skip)
            }
            node => node,
        };

        // 結果もカノニカル形（粗い次元から割る）になるよう、両方の最初の次元のうち粗い方で割る
        let heads = node.head_dimension(&this).map_or(0, Dimension::bit)
            | region.finer_dimensions_than(&this);
        let dimension = this
            .coarsest_dimension_in(heads)
            .expect("region が this より狭いので割る次元がある");
        let halves = [Side::Lower, Side::Upper].map(|side| this.split_on(dimension, side).unwrap());

        let mut children = match self.load(node, Kind::Branch(dimension)).await? {
            Node::Branch(branch) if branch.dimension == dimension => [branch.lower, branch.upper],
            node => self.split(node, &this, dimension).await?,
        };
        for (child, half) in children.iter_mut().zip(halves) {
            if let Some(region) = half.intersection(&region) {
                *child = Box::pin(self.write(half, mem::take(child), region, value)).await?;
            }
        }
        let [lower, upper] = children;
        self.join(&this, dimension, lower, upper).await
    }

    /// 領域 `this` の `node` を `dimension` で割った `[下, 上]`（`join` の逆）。
    ///
    /// `dimension` で割っていなければ、両側にこの Node を返す。
    async fn split(
        &self,
        node: Node,
        this: &FlexId,
        dimension: Dimension,
    ) -> Result<[Node; 2], Error> {
        let node = self.load(node, Kind::Skip).await?;
        if let Node::Skip(skip) = &node
            && skip.path.depth_on(dimension) > 0
        {
            let region = skip.path.to_absolute(this)?;
            let next = this.split_toward(dimension, &region).unwrap();
            let rest = self.rebase(node, this, &next).await?;
            return Ok(if next == this.split_on(dimension, Side::Upper).unwrap() {
                [Node::Empty, rest]
            } else {
                [rest, Node::Empty]
            });
        }
        // その次元では中身が変わらないので、下も上も同じ中身になる
        Ok([node.clone(), node])
    }

    /// 領域 `this` を `dimension` で割った左右 `lower`,`upper` を、カノニカル形の1つの[Node]に合わせる。
    async fn join(
        &self,
        this: &FlexId,
        dimension: Dimension,
        lower: Node,
        upper: Node,
    ) -> Result<Node, Error> {
        if lower.is_empty() {
            let upper_id = this.split_on(dimension, Side::Upper).unwrap();
            return self.rebase(upper, &upper_id, this).await;
        }
        if upper.is_empty() {
            let lower_id = this.split_on(dimension, Side::Lower).unwrap();
            return self.rebase(lower, &lower_id, this).await;
        }
        // Node は領域との相対位置で意味を持つので、子孫が `dimension` で割っている Node を
        // 広い領域へ持ち上げると意味が変わる。左右が同じでも、そのときは割ったままにする。
        if lower.split_dimensions() & dimension.bit() == 0 && self.same(&lower, &upper).await? {
            return Ok(lower);
        }
        Ok(Branch::new(dimension, lower, upper).into())
    }

    /// 領域 `from` から見た `node` を、領域 `to` から見た Node にする。`node` の中身は `to` に収まっていること。
    async fn rebase(&self, node: Node, from: &FlexId, to: &FlexId) -> Result<Node, Error> {
        if from == to {
            return Ok(node);
        }
        Ok(match self.load(node, Kind::Skip).await? {
            Node::Empty => Node::Empty,
            // Skip の先へさらに Skip しないよう、Skip の行き先から直接張り直す
            Node::Skip(skip) => {
                let region = skip.path.to_absolute(from)?;
                if region == *to {
                    skip.child
                } else {
                    Skip::new(region.relative_to(to)?, skip.child).into()
                }
            }
            node => Skip::new(from.relative_to(to)?, node).into(),
        })
    }

    /// `a` と `b` が同じ中身なら true。ディスク上の同じ場所と同じなら読まずに判断する。
    async fn same(&self, a: &Node, b: &Node) -> Result<bool, Error> {
        if let (Some(a), Some(b)) = (a.stored(), b.stored())
            && a == b
        {
            return Ok(true);
        }
        if a.split_dimensions() != b.split_dimensions() {
            return Ok(false);
        }
        let (a, b) = (self.view(a).await?, self.view(b).await?);
        Ok(match (&*a, &*b) {
            (Node::Empty, Node::Empty) => true,
            (Node::Leaf(a), Node::Leaf(b)) => a == b,
            (Node::Branch(a), Node::Branch(b)) => {
                a.dimension == b.dimension
                    && Box::pin(self.same(&a.lower, &b.lower)).await?
                    && Box::pin(self.same(&a.upper, &b.upper)).await?
            }
            (Node::Skip(a), Node::Skip(b)) => {
                a.path == b.path && Box::pin(self.same(&a.child, &b.child)).await?
            }
            _ => false,
        })
    }

    /// `node` の中身を見られる形にする。[`Node::Stored`] なら読み込む。
    async fn view<'a>(&self, node: &'a Node) -> Result<Cow<'a, Node>, Error> {
        Ok(match node {
            Node::Stored(stored) => Cow::Owned(self.read(*stored).await?),
            node => Cow::Borrowed(node),
        })
    }
}

/// `pages` のページがすべて書き終わっていて、チェックサムが合うなら true。
async fn intact(file: &File, pages: Range<u64>) -> Result<bool, Error> {
    for page_index in pages {
        match Page::read_at_page(file, page_index).await? {
            Some(page) if page.try_head_as::<RawPage>().is_ok() => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}
