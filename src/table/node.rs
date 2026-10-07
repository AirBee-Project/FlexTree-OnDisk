use bytemuck::{CheckedBitPattern, NoUninit, Pod, Zeroable, checked};
use kasane_logic::{Dimension, FlexId, RelativeFlexId};

use crate::{Error, disk::Page};

/// Node は自分の領域を持たず、親から渡される領域 `this` との相対的な位置で意味を持つ。
///
/// Branch・Skip は部分木ごとにページへ詰める。ページを読むとその部分木をまとめてメモリ上に展開する。
/// 新しく作った Branch・Skip だけを新しいページへ書き（CoW）、ディスクから読んだまま変えていないものは元の場所を指す。
#[derive(Debug, Clone, Default)]
pub(crate) enum Node {
    #[default]
    Empty,
    Leaf(u64),
    Branch(Box<Branch>),
    Skip(Box<Skip>),
    /// ディスク上にあり、まだ読み込んでいない Branch か Skip。
    Stored(Stored),
}

#[derive(Debug, Clone)]
pub(crate) struct Branch {
    pub(super) dimension: Dimension,
    /// 自身と子孫が割っている次元のビットマスク。自身の `dimension` とlowerとupperの集合を合わせたもの。
    pub(super) split_dimensions: u8,
    pub(super) lower: Node,
    pub(super) upper: Node,
    /// ディスクから読んだものなら、その場所。作り直さずに使い回すので、中身はその場所と常に同じ。
    pub(super) stored: Option<Stored>,
}

#[derive(Debug, Clone)]
pub(crate) struct Skip {
    pub(super) path: RelativeFlexId,
    /// 自身と子孫が割っている次元のビットマスク。行き先が狭い次元（`path` の深くなっている次元）と `child` の集合を合わせたもの。
    pub(super) split_dimensions: u8,
    pub(super) child: Node,
    /// ディスクから読んだものなら、その場所。作り直さずに使い回すので、中身はその場所と常に同じ。
    pub(super) stored: Option<Stored>,
}

/// ディスク上の Branch・Skip の場所。読み込まずに辿り方を決められるよう、割っている次元も持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stored {
    pub(super) page: u64,
    /// ページの中のレコードの番号。
    pub(super) slot: u8,
    pub(super) split_dimensions: u8,
    pub(super) kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Branch(Dimension),
    Skip,
}

impl Node {
    pub(super) fn is_empty(&self) -> bool {
        matches!(self, Node::Empty)
    }

    /// 自身と子孫が割っている次元の集合のビットマスク。ビットマスクは[`Dimension::bit`]のOR。
    pub(super) fn split_dimensions(&self) -> u8 {
        match self {
            Node::Empty | Node::Leaf(_) => 0,
            Node::Branch(branch) => branch.split_dimensions,
            Node::Skip(skip) => skip.split_dimensions,
            Node::Stored(stored) => stored.split_dimensions,
        }
    }

    /// この Node を「最初にどの次元で割るか」。Leaf なら [`None`]。
    pub(super) fn head_dimension(&self, this: &FlexId) -> Option<Dimension> {
        match self {
            Node::Empty | Node::Leaf(_) => None,
            Node::Branch(branch) => Some(branch.dimension),
            Node::Stored(Stored {
                kind: Kind::Branch(dimension),
                ..
            }) => Some(*dimension),
            Node::Skip(_) | Node::Stored(_) => this.coarsest_dimension_in(self.split_dimensions()),
        }
    }

    /// ディスク上の Branch・Skip と同じ中身なら、その場所。
    pub(super) fn stored(&self) -> Option<Stored> {
        match self {
            Node::Empty | Node::Leaf(_) => None,
            Node::Branch(branch) => branch.stored,
            Node::Skip(skip) => skip.stored,
            Node::Stored(stored) => Some(*stored),
        }
    }
}

impl Branch {
    pub(super) fn new(dimension: Dimension, lower: Node, upper: Node) -> Self {
        Branch {
            dimension,
            split_dimensions: dimension.bit() | lower.split_dimensions() | upper.split_dimensions(),
            lower,
            upper,
            stored: None,
        }
    }
}

impl Skip {
    pub(super) fn new(path: RelativeFlexId, child: Node) -> Self {
        Skip {
            split_dimensions: path.deeper_dimensions() | child.split_dimensions(),
            path,
            child,
            stored: None,
        }
    }
}

impl From<Branch> for Node {
    fn from(branch: Branch) -> Self {
        Node::Branch(Box::new(branch))
    }
}

impl From<Skip> for Node {
    fn from(skip: Skip) -> Self {
        Node::Skip(Box::new(skip))
    }
}

/// 行き先 `target`・その先が `split_dimensions` で割る Skip と、`region` への書き込みが、
/// 領域 `this` から途中で割らずに一緒に降りられる一番狭い領域。
pub(super) fn common_descent(
    this: &FlexId,
    target: &FlexId,
    split_dimensions: u8,
    region: &FlexId,
) -> FlexId {
    // 行き先が `to`、その先で割っている次元が `child_split` のとき、領域 `at` で最初に割る次元
    let head = |at: &FlexId, to: &FlexId, child_split: u8| {
        let finer = to.finer_dimensions_than(at);
        (finer != 0).then(|| at.coarsest_dimension_in(finer | child_split))?
    };
    let mut at = *this;
    while let (Some(da), Some(db)) = (head(&at, target, split_dimensions), head(&at, region, 0))
        && da == db
    {
        let next = at.split_toward(da, target).unwrap();
        if !next.contains(region) {
            break;
        }
        at = next;
    }
    at
}

/// ディスク上の [Node] の種類。Branch はどの次元で割るかまで含める。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, CheckedBitPattern, NoUninit)]
enum Tag {
    Empty,
    Leaf,
    /// 同じページの後ろのレコード。
    Local,
    Skip,
    BranchF,
    BranchX,
    BranchY,
    BranchT,
}

/// 子を指すディスク上の [Node]。Leaf は値をそのまま埋め込む。
#[repr(C)]
#[derive(Debug, Clone, Copy, CheckedBitPattern, NoUninit)]
pub(crate) struct RawNode {
    tag: Tag,
    /// ページの外の Branch・Skip の `split_dimensions`。
    split_dimensions: u8,
    /// ページの外の Branch・Skip の、そのページの中での番号。
    slot: u8,
    _pad: [u8; 5],
    /// Leaf なら値、Local なら親のレコードから何個後ろか、それ以外ならページ番号。
    payload: u64,
}

/// ページに詰める Branch・Skip。どちらも先頭がタグと `split_dimensions` で、タグに応じて
/// [RawBranch] か [RawSkip] として読む。
#[repr(C, align(8))]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct RawRecord([u8; 40]);

#[repr(C)]
#[derive(Clone, Copy, CheckedBitPattern, NoUninit)]
struct RawBranch {
    tag: Tag,
    split_dimensions: u8,
    _pad: [u8; 6],
    lower: RawNode,
    upper: RawNode,
}

#[repr(C)]
#[derive(Clone, Copy, CheckedBitPattern, NoUninit)]
struct RawSkip {
    tag: Tag,
    split_dimensions: u8,
    path: [u8; RelativeFlexId::ENCODED_LEN],
    _pad: [u8; 2],
    child: RawNode,
}

const _: () = assert!(size_of::<RawBranch>() == size_of::<RawRecord>());
const _: () = assert!(size_of::<RawSkip>() == size_of::<RawRecord>());

/// 1 ページに詰められるレコードの数。
pub(super) const RECORDS_PER_PAGE: usize = Page::SIZE / size_of::<RawRecord>();

const _: () = assert!(RECORDS_PER_PAGE <= u8::MAX as usize);

/// Branch・Skip を詰めたページ。先頭のレコードがそのページの部分木の根になる。
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct RawPage {
    pub(super) records: [RawRecord; RECORDS_PER_PAGE],
}

/// ヘッダーのページ。ある世代の [`FlexId::ROOTS`] の各ルートと、その世代で書いたページの範囲を持つ。
#[repr(C)]
#[derive(Clone, Copy, CheckedBitPattern, NoUninit)]
pub(crate) struct RawHeader {
    pub(super) upper: RawNode,
    pub(super) lower: RawNode,
    pub(super) generation: u64,
    /// この世代で書いたページは `start..end`。
    pub(super) start: u64,
    pub(super) end: u64,
}

impl From<Kind> for Tag {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Skip => Tag::Skip,
            Kind::Branch(Dimension::F) => Tag::BranchF,
            Kind::Branch(Dimension::X) => Tag::BranchX,
            Kind::Branch(Dimension::Y) => Tag::BranchY,
            Kind::Branch(Dimension::T) => Tag::BranchT,
        }
    }
}

impl TryFrom<Tag> for Kind {
    type Error = Error;

    fn try_from(tag: Tag) -> Result<Self, Error> {
        Ok(match tag {
            Tag::Empty | Tag::Leaf | Tag::Local => return Err(Error::Corrupt),
            Tag::Skip => Kind::Skip,
            Tag::BranchF => Kind::Branch(Dimension::F),
            Tag::BranchX => Kind::Branch(Dimension::X),
            Tag::BranchY => Kind::Branch(Dimension::Y),
            Tag::BranchT => Kind::Branch(Dimension::T),
        })
    }
}

impl RawNode {
    pub(super) const EMPTY: RawNode = RawNode::new(Tag::Empty, 0);

    const fn new(tag: Tag, payload: u64) -> Self {
        RawNode {
            tag,
            split_dimensions: 0,
            slot: 0,
            _pad: [0; 5],
            payload,
        }
    }

    pub(super) const fn leaf(value: u64) -> Self {
        RawNode::new(Tag::Leaf, value)
    }

    /// 親のレコードから `offset` 個後ろのレコード。
    pub(super) const fn local(offset: usize) -> Self {
        RawNode::new(Tag::Local, offset as u64)
    }
}

impl From<Stored> for RawNode {
    fn from(stored: Stored) -> Self {
        RawNode {
            split_dimensions: stored.split_dimensions,
            slot: stored.slot,
            ..RawNode::new(stored.kind.into(), stored.page)
        }
    }
}

impl TryFrom<RawNode> for Node {
    type Error = Error;

    /// ページの外を指すものだけを変換する。同じページの中を指すものは [`RawPage::decode`] で読む。
    fn try_from(raw: RawNode) -> Result<Self, Error> {
        Ok(match raw.tag {
            Tag::Empty => Node::Empty,
            Tag::Leaf => Node::Leaf(raw.payload),
            tag => Node::Stored(Stored {
                page: raw.payload,
                slot: raw.slot,
                split_dimensions: raw.split_dimensions,
                kind: tag.try_into()?,
            }),
        })
    }
}

impl RawRecord {
    pub(super) fn branch(
        dimension: Dimension,
        split_dimensions: u8,
        lower: RawNode,
        upper: RawNode,
    ) -> Self {
        RawRecord(bytemuck::cast(RawBranch {
            tag: Kind::Branch(dimension).into(),
            split_dimensions,
            _pad: [0; 6],
            lower,
            upper,
        }))
    }

    pub(super) fn skip(path: RelativeFlexId, split_dimensions: u8, child: RawNode) -> Self {
        RawRecord(bytemuck::cast(RawSkip {
            tag: Tag::Skip,
            split_dimensions,
            path: path.encode(),
            _pad: [0; 2],
            child,
        }))
    }

    /// このレコードを `page` の `slot` 番目に書いたときの場所。
    pub(super) fn stored_at(&self, page: u64, slot: usize) -> Result<Stored, Error> {
        let [tag, split_dimensions, ..] = self.0;
        Ok(Stored {
            page,
            slot: u8::try_from(slot).map_err(|_| Error::Corrupt)?,
            split_dimensions,
            kind: checked::try_cast::<u8, Tag>(tag)?.try_into()?,
        })
    }
}

impl RawPage {
    /// このページ（ページ番号 `page`）の `slot` 番目から下を、メモリ上に展開する。
    /// ページの外を指す子は読まずに [`Node::Stored`] のままにする。
    pub(super) fn decode(&self, page: u64, slot: usize) -> Result<Node, Error> {
        let record = self.records.get(slot).ok_or(Error::Corrupt)?;
        let stored = record.stored_at(page, slot)?;
        let child = |raw: RawNode| match raw.tag {
            // 子は親より後ろにしか置かないので、壊れたファイルでも循環しない
            Tag::Local => match usize::try_from(raw.payload) {
                Ok(offset) if offset > 0 => self.decode(page, slot + offset),
                _ => Err(Error::Corrupt),
            },
            _ => Node::try_from(raw),
        };
        Ok(match stored.kind {
            Kind::Branch(dimension) => {
                let raw: &RawBranch = checked::try_from_bytes(&record.0)?;
                Branch {
                    stored: Some(stored),
                    ..Branch::new(dimension, child(raw.lower)?, child(raw.upper)?)
                }
                .into()
            }
            Kind::Skip => {
                let raw: &RawSkip = checked::try_from_bytes(&record.0)?;
                Skip {
                    stored: Some(stored),
                    ..Skip::new(RelativeFlexId::decode(&raw.path)?, child(raw.child)?)
                }
                .into()
            }
        })
    }
}

/// 子の側から割り付けている途中の部分木。
pub(super) enum Part {
    /// ページに書き終えたか、ページに書かない Node。
    Done(RawNode),
    /// まだページに書いていないレコードの列。先頭が根。
    Open(Vec<RawRecord>),
}

impl Part {
    /// まだページに書いていないレコードの数。
    pub(super) fn len(&self) -> usize {
        match self {
            Part::Done(_) => 0,
            Part::Open(records) => records.len(),
        }
    }
}
