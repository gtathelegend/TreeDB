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
    Minus,

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
            TokenKind::Minus => write!(f, "'-'"),
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
