use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    Truncated,
    BadMagic,
    BadVersion,
    TooLarge,
    Malformed(&'static str),
    Reserved,
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::Truncated => write!(f, "截断"),
            ProtoError::BadMagic => write!(f, "魔数不对"),
            ProtoError::BadVersion => write!(f, "版本不合"),
            ProtoError::TooLarge => write!(f, "超过长度上限"),
            ProtoError::Malformed(w) => write!(f, "格式错误: {w}"),
            ProtoError::Reserved => write!(f, "保留字段非零"),
        }
    }
}

impl std::error::Error for ProtoError {}
