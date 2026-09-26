use crate::error::{Error, Result};
use crate::sql::span::SourceSpan;
use crate::sql::token::{Token, TokenKind};

/// Maximum allowable length for an SQL identifier in bytes.
pub const MAX_IDENTIFIER_LEN: usize = 128;

/// Lexical analyzer that tokenizes SQL input text into a sequence of `Token`s.
pub struct Lexer<'a> {
    source: &'a str,
    chars: Vec<(usize, char)>,
    cursor: usize,
}

impl<'a> Lexer<'a> {
    /// Creates a new `Lexer` over the provided SQL string.
    pub fn new(source: &'a str) -> Self {
        let chars: Vec<(usize, char)> = source.char_indices().collect();
        Self {
            source,
            chars,
            cursor: 0,
        }
    }

    /// Tokenizes the entire SQL input into a vector of tokens, ending with `TokenKind::Eof`.
    pub fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut tokens = Vec::new();

        loop {
            self.skip_whitespace_and_comments();

            if self.is_eof() {
                let end = self.source.len();
                tokens.push(Token::new(TokenKind::Eof, SourceSpan::new(end, end)));
                break;
            }

            let token = self.next_token()?;
            tokens.push(token);
        }

        Ok(tokens)
    }

    fn next_token(&mut self) -> Result<Token> {
        let (start, ch) = self.peek_char().unwrap();

        match ch {
            // Punctuation
            '*' => {
                self.advance();
                Ok(Token::new(
                    TokenKind::Asterisk,
                    SourceSpan::new(start, start + 1),
                ))
            }
            ',' => {
                self.advance();
                Ok(Token::new(
                    TokenKind::Comma,
                    SourceSpan::new(start, start + 1),
                ))
            }
            '(' => {
                self.advance();
                Ok(Token::new(
                    TokenKind::LParen,
                    SourceSpan::new(start, start + 1),
                ))
            }
            ')' => {
                self.advance();
                Ok(Token::new(
                    TokenKind::RParen,
                    SourceSpan::new(start, start + 1),
                ))
            }
            ';' => {
                self.advance();
                Ok(Token::new(
                    TokenKind::Semicolon,
                    SourceSpan::new(start, start + 1),
                ))
            }
            '.' => {
                self.advance();
                Ok(Token::new(
                    TokenKind::Dot,
                    SourceSpan::new(start, start + 1),
                ))
            }

            // Operators
            '=' => {
                self.advance();
                Ok(Token::new(TokenKind::Eq, SourceSpan::new(start, start + 1)))
            }
            '+' => {
                self.advance();
                Ok(Token::new(
                    TokenKind::Plus,
                    SourceSpan::new(start, start + 1),
                ))
            }
            '-' => {
                self.advance();
                Ok(Token::new(
                    TokenKind::Minus,
                    SourceSpan::new(start, start + 1),
                ))
            }
            '/' => {
                self.advance();
                Ok(Token::new(
                    TokenKind::Slash,
                    SourceSpan::new(start, start + 1),
                ))
            }
            '!' => {
                self.advance();
                if self.match_char('=') {
                    let end = self.current_byte_offset();
                    Ok(Token::new(TokenKind::NotEq, SourceSpan::new(start, end)))
                } else {
                    Err(Error::SqlLexError {
                        message: "unexpected character '!', expected '=' after '!'".to_string(),
                        start,
                        end: start + 1,
                    })
                }
            }
            '<' => {
                self.advance();
                if self.match_char('>') {
                    let end = self.current_byte_offset();
                    Ok(Token::new(TokenKind::NotEq, SourceSpan::new(start, end)))
                } else if self.match_char('=') {
                    let end = self.current_byte_offset();
                    Ok(Token::new(TokenKind::LtEq, SourceSpan::new(start, end)))
                } else {
                    Ok(Token::new(TokenKind::Lt, SourceSpan::new(start, start + 1)))
                }
            }
            '>' => {
                self.advance();
                if self.match_char('=') {
                    let end = self.current_byte_offset();
                    Ok(Token::new(TokenKind::GtEq, SourceSpan::new(start, end)))
                } else {
                    Ok(Token::new(TokenKind::Gt, SourceSpan::new(start, start + 1)))
                }
            }

            // String literals
            '\'' => self.lex_string(start),

            // Number literals
            '0'..='9' => self.lex_number(start),

            // Identifiers or Keywords
            'a'..='z' | 'A'..='Z' | '_' => self.lex_identifier_or_keyword(start),

            // Unexpected characters
            _ => {
                self.advance();
                let end = self.current_byte_offset();
                Err(Error::SqlLexError {
                    message: format!("unexpected character '{ch}'"),
                    start,
                    end,
                })
            }
        }
    }

    fn lex_string(&mut self, start: usize) -> Result<Token> {
        // Consume opening quote
        self.advance();
        let mut string_val = String::new();

        while let Some((_, ch)) = self.peek_char() {
            if ch == '\'' {
                self.advance();
                // Check if escaped single quote: ''
                if let Some((_, '\'')) = self.peek_char() {
                    self.advance();
                    string_val.push('\'');
                } else {
                    // Closing quote
                    let end = self.current_byte_offset();
                    return Ok(Token::new(
                        TokenKind::String(string_val),
                        SourceSpan::new(start, end),
                    ));
                }
            } else {
                string_val.push(ch);
                self.advance();
            }
        }

        let end = self.source.len();
        Err(Error::SqlLexError {
            message: "unterminated string literal".to_string(),
            start,
            end,
        })
    }

    fn lex_number(&mut self, start: usize) -> Result<Token> {
        while let Some((_, ch)) = self.peek_char() {
            if ch.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }

        // Check if floating point
        let mut is_float = false;
        if let Some((_, '.')) = self.peek_char() {
            // Check if followed by digit
            if self
                .peek_next_char()
                .is_some_and(|(_, ch)| ch.is_ascii_digit())
            {
                is_float = true;
                self.advance(); // consume '.'
                while let Some((_, ch)) = self.peek_char() {
                    if ch.is_ascii_digit() {
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
        }

        let end = self.current_byte_offset();
        let num_str = &self.source[start..end];

        if is_float {
            match num_str.parse::<f64>() {
                Ok(val) => Ok(Token::new(
                    TokenKind::Float(val),
                    SourceSpan::new(start, end),
                )),
                Err(e) => Err(Error::SqlLexError {
                    message: format!("invalid floating point literal '{num_str}': {e}"),
                    start,
                    end,
                }),
            }
        } else {
            match num_str.parse::<i64>() {
                Ok(val) => Ok(Token::new(
                    TokenKind::Integer(val),
                    SourceSpan::new(start, end),
                )),
                Err(e) => Err(Error::SqlLexError {
                    message: format!("invalid integer literal '{num_str}': {e}"),
                    start,
                    end,
                }),
            }
        }
    }

    fn lex_identifier_or_keyword(&mut self, start: usize) -> Result<Token> {
        while let Some((_, ch)) = self.peek_char() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                self.advance();
            } else {
                break;
            }
        }

        let end = self.current_byte_offset();
        let ident_str = &self.source[start..end];

        if ident_str.len() > MAX_IDENTIFIER_LEN {
            return Err(Error::SqlLexError {
                message: format!(
                    "identifier length {} exceeds limit of {}",
                    ident_str.len(),
                    MAX_IDENTIFIER_LEN
                ),
                start,
                end,
            });
        }

        let kind = match ident_str.to_ascii_uppercase().as_str() {
            "SELECT" => TokenKind::Select,
            "FROM" => TokenKind::From,
            "WHERE" => TokenKind::Where,
            "ORDER" => TokenKind::Order,
            "BY" => TokenKind::By,
            "ASC" => TokenKind::Asc,
            "DESC" => TokenKind::Desc,
            "LIMIT" => TokenKind::Limit,
            "OFFSET" => TokenKind::Offset,
            "AND" => TokenKind::And,
            "OR" => TokenKind::Or,
            "NOT" => TokenKind::Not,
            "IS" => TokenKind::Is,
            "NULL" => TokenKind::Null,
            "TRUE" => TokenKind::True,
            "FALSE" => TokenKind::False,
            "AS" => TokenKind::As,
            "GROUP" => TokenKind::Group,
            "HAVING" => TokenKind::Having,
            "JOIN" => TokenKind::Join,
            "INNER" => TokenKind::Inner,
            "LEFT" => TokenKind::Left,
            "OUTER" => TokenKind::Outer,
            "ON" => TokenKind::On,
            "CREATE" => TokenKind::Create,
            "TABLE" => TokenKind::Table,
            "DROP" => TokenKind::Drop,
            "INSERT" => TokenKind::Insert,
            "INTO" => TokenKind::Into,
            "VALUES" => TokenKind::Values,
            "UPDATE" => TokenKind::Update,
            "SET" => TokenKind::Set,
            "DELETE" => TokenKind::Delete,
            "PRIMARY" => TokenKind::Primary,
            "KEY" => TokenKind::Key,
            "IN" => TokenKind::In,
            "EXISTS" => TokenKind::Exists,
            "WITH" => TokenKind::With,
            "RECURSIVE" => TokenKind::Recursive,
            "UNION" => TokenKind::Union,
            "ALL" => TokenKind::All,
            "INTERSECT" => TokenKind::Intersect,
            "EXCEPT" => TokenKind::Except,
            "OVER" => TokenKind::Over,
            "PARTITION" => TokenKind::Partition,
            "ROWS" => TokenKind::Rows,
            "BETWEEN" => TokenKind::Between,
            "PRECEDING" => TokenKind::Preceding,
            "FOLLOWING" => TokenKind::Following,
            "CURRENT" => TokenKind::Current,
            "ROW" => TokenKind::Row,
            "UNBOUNDED" => TokenKind::Unbounded,
            "RANGE" => TokenKind::Range,
            "GROUPS" => TokenKind::Groups,
            "INDEX" => TokenKind::Index,
            "UNIQUE" => TokenKind::Unique,
            "BOOLEAN" | "BOOL" => TokenKind::BooleanType,
            "BIGINT" | "INT64" | "INTEGER" | "INT" => TokenKind::BigIntType,
            "UBIGINT" | "UINT64" | "UINT" => TokenKind::UBigIntType,
            "DOUBLE" | "FLOAT64" | "FLOAT" => TokenKind::DoubleType,
            "TEXT" | "STRING" | "VARCHAR" => TokenKind::TextType,
            "BLOB" | "BYTES" | "BYTEA" => TokenKind::BlobType,
            _ => TokenKind::Ident(ident_str.to_string()),
        };

        Ok(Token::new(kind, SourceSpan::new(start, end)))
    }

    fn skip_whitespace_and_comments(&mut self) {
        while let Some((_, ch)) = self.peek_char() {
            if ch.is_ascii_whitespace() {
                self.advance();
            } else if ch == '-' {
                if let Some((_, '-')) = self.peek_next_char() {
                    // Line comment: consume until '\n' or EOF
                    self.advance(); // consume first '-'
                    self.advance(); // consume second '-'
                    while let Some((_, c)) = self.peek_char() {
                        self.advance();
                        if c == '\n' {
                            break;
                        }
                    }
                } else {
                    break;
                }
            } else {
                break;
            }
        }
    }

    fn is_eof(&self) -> bool {
        self.cursor >= self.chars.len()
    }

    fn peek_char(&self) -> Option<(usize, char)> {
        self.chars.get(self.cursor).copied()
    }

    fn peek_next_char(&self) -> Option<(usize, char)> {
        self.chars.get(self.cursor + 1).copied()
    }

    fn advance(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    fn match_char(&mut self, expected: char) -> bool {
        if let Some((_, ch)) = self.peek_char() {
            if ch == expected {
                self.advance();
                return true;
            }
        }
        false
    }

    fn current_byte_offset(&self) -> usize {
        if let Some((offset, _)) = self.peek_char() {
            offset
        } else {
            self.source.len()
        }
    }
}
