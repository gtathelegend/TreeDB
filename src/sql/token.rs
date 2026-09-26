use std::fmt;

use crate::sql::span::SourceSpan;

/// Enumeration of token categories recognized by the SQL lexer.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Keywords (case-insensitive)
    Select,
    From,
    Where,
    Order,
    By,
    Asc,
    Desc,
    Limit,
    Offset,
    And,
    Or,
    Not,
    Is,
    Null,
    True,
    False,
    As,
    Group,
    Having,
    Join,
    Inner,
    Left,
    Outer,
    On,
    Create,
    Table,
    Drop,
    Insert,
    Into,
    Values,
    Update,
    Set,
    Delete,
    Primary,
    Key,
    In,
    Exists,
    With,
    Recursive,
    Union,
    All,
    Intersect,
    Except,
    Over,
    Partition,
    Rows,
    Between,
    Preceding,
    Following,
    Current,
    Row,
    Unbounded,
    Range,
    Groups,
    Index,
    Unique,

    // Data Type Keywords
    BooleanType,
    BigIntType,
    UBigIntType,
    DoubleType,
    TextType,
    BlobType,

    // Identifiers & Literals
    Ident(String),
    Integer(i64),
    Float(f64),
    String(String),

    // Operators
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Plus,
    Minus,
    Slash,

    // Punctuation
    Asterisk,
    Comma,
    Dot,

    LParen,
    RParen,
    Semicolon,

    // Control
    Eof,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenKind::Select => write!(f, "SELECT"),
            TokenKind::From => write!(f, "FROM"),
            TokenKind::Where => write!(f, "WHERE"),
            TokenKind::Order => write!(f, "ORDER"),
            TokenKind::By => write!(f, "BY"),
            TokenKind::Asc => write!(f, "ASC"),
            TokenKind::Desc => write!(f, "DESC"),
            TokenKind::Limit => write!(f, "LIMIT"),
            TokenKind::Offset => write!(f, "OFFSET"),
            TokenKind::And => write!(f, "AND"),
            TokenKind::Or => write!(f, "OR"),
            TokenKind::Not => write!(f, "NOT"),
            TokenKind::Is => write!(f, "IS"),
            TokenKind::Null => write!(f, "NULL"),
            TokenKind::True => write!(f, "TRUE"),
            TokenKind::False => write!(f, "FALSE"),
            TokenKind::As => write!(f, "AS"),
            TokenKind::Group => write!(f, "GROUP"),
            TokenKind::Having => write!(f, "HAVING"),
            TokenKind::Join => write!(f, "JOIN"),
            TokenKind::Inner => write!(f, "INNER"),
            TokenKind::Left => write!(f, "LEFT"),
            TokenKind::Outer => write!(f, "OUTER"),
            TokenKind::On => write!(f, "ON"),
            TokenKind::Create => write!(f, "CREATE"),
            TokenKind::Table => write!(f, "TABLE"),
            TokenKind::Drop => write!(f, "DROP"),
            TokenKind::Insert => write!(f, "INSERT"),
            TokenKind::Into => write!(f, "INTO"),
            TokenKind::Values => write!(f, "VALUES"),
            TokenKind::Update => write!(f, "UPDATE"),
            TokenKind::Set => write!(f, "SET"),
            TokenKind::Delete => write!(f, "DELETE"),
            TokenKind::Primary => write!(f, "PRIMARY"),
            TokenKind::Key => write!(f, "KEY"),
            TokenKind::In => write!(f, "IN"),
            TokenKind::Exists => write!(f, "EXISTS"),
            TokenKind::With => write!(f, "WITH"),
            TokenKind::Recursive => write!(f, "RECURSIVE"),
            TokenKind::Union => write!(f, "UNION"),
            TokenKind::All => write!(f, "ALL"),
            TokenKind::Intersect => write!(f, "INTERSECT"),
            TokenKind::Except => write!(f, "EXCEPT"),
            TokenKind::Over => write!(f, "OVER"),
            TokenKind::Partition => write!(f, "PARTITION"),
            TokenKind::Rows => write!(f, "ROWS"),
            TokenKind::Between => write!(f, "BETWEEN"),
            TokenKind::Preceding => write!(f, "PRECEDING"),
            TokenKind::Following => write!(f, "FOLLOWING"),
            TokenKind::Current => write!(f, "CURRENT"),
            TokenKind::Row => write!(f, "ROW"),
            TokenKind::Unbounded => write!(f, "UNBOUNDED"),
            TokenKind::Range => write!(f, "RANGE"),
            TokenKind::Groups => write!(f, "GROUPS"),
            TokenKind::Index => write!(f, "INDEX"),
            TokenKind::Unique => write!(f, "UNIQUE"),
            TokenKind::BooleanType => write!(f, "BOOLEAN"),
            TokenKind::BigIntType => write!(f, "BIGINT"),
            TokenKind::UBigIntType => write!(f, "UBIGINT"),
            TokenKind::DoubleType => write!(f, "DOUBLE"),
            TokenKind::TextType => write!(f, "TEXT"),
            TokenKind::BlobType => write!(f, "BLOB"),
            TokenKind::Ident(s) => write!(f, "identifier '{s}'"),
            TokenKind::Integer(i) => write!(f, "integer '{i}'"),
            TokenKind::Float(v) => write!(f, "float '{v}'"),
            TokenKind::String(s) => write!(f, "string '{s}'"),
            TokenKind::Eq => write!(f, "'='"),
            TokenKind::NotEq => write!(f, "'!='"),
            TokenKind::Lt => write!(f, "'<'"),
            TokenKind::LtEq => write!(f, "'<='"),
            TokenKind::Gt => write!(f, "'>'"),
            TokenKind::GtEq => write!(f, "'>='"),
            TokenKind::Plus => write!(f, "'+'"),
            TokenKind::Minus => write!(f, "'-'"),
            TokenKind::Slash => write!(f, "'/'"),
            TokenKind::Asterisk => write!(f, "'*'"),
            TokenKind::Comma => write!(f, "','"),
            TokenKind::Dot => write!(f, "'.'"),
            TokenKind::LParen => write!(f, "'('"),
            TokenKind::RParen => write!(f, "')'"),
            TokenKind::Semicolon => write!(f, "';'"),
            TokenKind::Eof => write!(f, "end of input"),
        }
    }
}

/// A lexical token pairing a `TokenKind` with its source span.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// Token classification.
    pub kind: TokenKind,
    /// Byte offset range in the source query string.
    pub span: SourceSpan,
}

impl Token {
    /// Creates a new `Token` with the given kind and span.
    pub const fn new(kind: TokenKind, span: SourceSpan) -> Self {
        Self { kind, span }
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}", self.kind, self.span)
    }
}
