use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    SpatialId(kasane_logic::Error),
    /// ファイルの中身が想定した形式になっていない。
    Corrupt,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::SpatialId(err) => write!(f, "spatial id error: {err}"),
            Self::Corrupt => write!(f, "corrupt file"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::SpatialId(err) => Some(err),
            Self::Corrupt => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<bytemuck::checked::CheckedCastError> for Error {
    fn from(_: bytemuck::checked::CheckedCastError) -> Self {
        Self::Corrupt
    }
}

impl From<kasane_logic::Error> for Error {
    fn from(err: kasane_logic::Error) -> Self {
        Self::SpatialId(err)
    }
}
