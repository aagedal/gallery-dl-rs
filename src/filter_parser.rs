//! Bounded parsing of the supported, non-executable Python expression syntax.
use super::*;
use std::sync::Arc;

const MAX_SOURCE: usize = 16 * 1024;
const MAX_TOKENS: usize = 4096;
const MAX_NODES: usize = 4096;
const MAX_DEPTH: usize = 64;

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Int(i128),
    MinMagnitude,
    Float(f64),
    String(Arc<str>),
    Name(String),
    None,
    True,
    False,
    Not,
    And,
    Or,
    In,
    Is,
    If,
    Else,
    Plus,
    Minus,
    Star,
    Slash,
    FloorDiv,
    Mod,
    Pow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Dot,
    End,
}
fn syntax() -> crate::Error {
    crate::error("unsupported or invalid filter expression syntax")
}

pub(super) fn parse(source: &str) -> Result<Expr> {
    if source.len() > MAX_SOURCE || source.contains('\0') {
        return Err(syntax());
    }
    let tokens = Lexer { source, index: 0 }.tokens()?;
    let mut parser = Parser {
        tokens,
        index: 0,
        nesting: 0,
        nodes: 0,
    };
    let expression = parser.expression()?;
    if parser.peek() != &Token::End {
        return Err(syntax());
    }
    Ok(expression.expr)
}

struct Lexer<'a> {
    source: &'a str,
    index: usize,
}
impl Lexer<'_> {
    fn tokens(mut self) -> Result<Vec<Token>> {
        let mut tokens = Vec::new();
        while self.index < self.source.len() {
            let byte = self.source.as_bytes()[self.index];
            if byte.is_ascii_whitespace() {
                self.index += 1;
                continue;
            }
            if tokens.len() >= MAX_TOKENS - 1 {
                return Err(syntax());
            }
            let token = match byte {
                b'\'' | b'"' => self.string(false)?,
                b'r' | b'R' | b'u' | b'U'
                    if self
                        .source
                        .as_bytes()
                        .get(self.index + 1)
                        .is_some_and(|byte| matches!(byte, b'\'' | b'"')) =>
                {
                    let raw = matches!(byte, b'r' | b'R');
                    self.index += 1;
                    self.string(raw)?
                }
                b'0'..=b'9' => self.number()?,
                b'.' if self
                    .source
                    .as_bytes()
                    .get(self.index + 1)
                    .is_some_and(u8::is_ascii_digit) =>
                {
                    self.number()?
                }
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.name()?,
                b'+' => {
                    self.index += 1;
                    Token::Plus
                }
                b'-' => {
                    self.index += 1;
                    Token::Minus
                }
                b'*' => {
                    self.index += 1;
                    if self.take(b'*') {
                        Token::Pow
                    } else {
                        Token::Star
                    }
                }
                b'/' => {
                    self.index += 1;
                    if self.take(b'/') {
                        Token::FloorDiv
                    } else {
                        Token::Slash
                    }
                }
                b'%' => {
                    self.index += 1;
                    Token::Mod
                }
                b'=' => {
                    self.index += 1;
                    if !self.take(b'=') {
                        return Err(syntax());
                    }
                    Token::Eq
                }
                b'!' => {
                    self.index += 1;
                    if !self.take(b'=') {
                        return Err(syntax());
                    }
                    Token::Ne
                }
                b'<' => {
                    self.index += 1;
                    if self.take(b'=') {
                        Token::Le
                    } else {
                        Token::Lt
                    }
                }
                b'>' => {
                    self.index += 1;
                    if self.take(b'=') {
                        Token::Ge
                    } else {
                        Token::Gt
                    }
                }
                b'(' => {
                    self.index += 1;
                    Token::LParen
                }
                b')' => {
                    self.index += 1;
                    Token::RParen
                }
                b'[' => {
                    self.index += 1;
                    Token::LBracket
                }
                b']' => {
                    self.index += 1;
                    Token::RBracket
                }
                b'{' => {
                    self.index += 1;
                    Token::LBrace
                }
                b'}' => {
                    self.index += 1;
                    Token::RBrace
                }
                b',' => {
                    self.index += 1;
                    Token::Comma
                }
                b':' => {
                    self.index += 1;
                    Token::Colon
                }
                b'.' => {
                    self.index += 1;
                    Token::Dot
                }
                _ => return Err(syntax()),
            };
            tokens.push(token);
        }
        tokens.push(Token::End);
        Ok(tokens)
    }
    fn take(&mut self, byte: u8) -> bool {
        if self.source.as_bytes().get(self.index) == Some(&byte) {
            self.index += 1;
            true
        } else {
            false
        }
    }
    fn name(&mut self) -> Result<Token> {
        let start = self.index;
        while self
            .source
            .as_bytes()
            .get(self.index)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            self.index += 1;
        }
        let name = &self.source[start..self.index];
        Ok(match name {
            "None" => Token::None,
            "True" => Token::True,
            "False" => Token::False,
            "not" => Token::Not,
            "and" => Token::And,
            "or" => Token::Or,
            "in" => Token::In,
            "is" => Token::Is,
            "if" => Token::If,
            "else" => Token::Else,
            "lambda" | "for" | "async" | "await" | "yield" | "import" | "from" | "def"
            | "class" | "del" | "global" | "nonlocal" | "pass" | "raise" | "try" | "except"
            | "while" | "with" | "as" | "assert" | "return" => return Err(syntax()),
            name if name.starts_with('_') && name != "_url" => return Err(syntax()),
            _ => Token::Name(name.into()),
        })
    }
    fn number(&mut self) -> Result<Token> {
        let start = self.index;
        if self.source.as_bytes().get(start) == Some(&b'0')
            && let Some(prefix) = self.source.as_bytes().get(start + 1)
            && matches!(prefix, b'x' | b'X' | b'o' | b'O' | b'b' | b'B')
        {
            let radix = match prefix {
                b'x' | b'X' => 16,
                b'o' | b'O' => 8,
                _ => 2,
            };
            self.index += 2;
            let begin = self.index;
            while self
                .source
                .as_bytes()
                .get(self.index)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                self.index += 1;
            }
            let digits = &self.source[begin..self.index];
            let digits = digits.strip_prefix('_').unwrap_or(digits);
            if digits.is_empty()
                || digits.starts_with('_')
                || digits.ends_with('_')
                || digits.contains("__")
            {
                return Err(syntax());
            }
            return integer_token(&digits.replace('_', ""), radix);
        }
        while self
            .source
            .as_bytes()
            .get(self.index)
            .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'_')
        {
            self.index += 1;
        }
        let mut float = false;
        if self.take(b'.') {
            float = true;
            while self
                .source
                .as_bytes()
                .get(self.index)
                .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'_')
            {
                self.index += 1;
            }
        }
        if self
            .source
            .as_bytes()
            .get(self.index)
            .is_some_and(|byte| matches!(byte, b'e' | b'E'))
        {
            float = true;
            self.index += 1;
            if !self.take(b'+') {
                self.take(b'-');
            }
            let exponent = self.index;
            while self
                .source
                .as_bytes()
                .get(self.index)
                .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'_')
            {
                self.index += 1;
            }
            if exponent == self.index {
                return Err(syntax());
            }
        }
        let raw = &self.source[start..self.index];
        let bytes = raw.as_bytes();
        for (i, byte) in bytes.iter().enumerate() {
            if *byte == b'_'
                && (i == 0
                    || i + 1 == bytes.len()
                    || !bytes[i - 1].is_ascii_digit()
                    || !bytes[i + 1].is_ascii_digit())
            {
                return Err(syntax());
            }
        }
        let cleaned = raw.replace('_', "");
        if float {
            let value: f64 = cleaned.parse().map_err(|_| syntax())?;
            if !value.is_finite() {
                return Err(syntax());
            }
            Ok(Token::Float(value))
        } else {
            if cleaned.len() > 1
                && cleaned.starts_with('0')
                && cleaned.bytes().any(|byte| byte != b'0')
            {
                return Err(syntax());
            }
            integer_token(&cleaned, 10)
        }
    }
    fn string(&mut self, raw: bool) -> Result<Token> {
        let quote = self.source.as_bytes()[self.index];
        self.index += 1;
        let triple =
            self.source.as_bytes().get(self.index..self.index + 2) == Some(&[quote, quote]);
        if triple {
            self.index += 2;
        }
        let mut result = String::new();
        loop {
            let byte = *self.source.as_bytes().get(self.index).ok_or_else(syntax)?;
            if byte == quote
                && (!triple
                    || self.source.as_bytes().get(self.index..self.index + 3)
                        == Some(&[quote, quote, quote]))
            {
                self.index += if triple { 3 } else { 1 };
                return Ok(Token::String(Arc::from(result)));
            }
            if matches!(byte, b'\n' | b'\r') && !triple {
                return Err(syntax());
            }
            if byte == b'\\' {
                self.index += 1;
                let next = self.source[self.index..]
                    .chars()
                    .next()
                    .ok_or_else(syntax)?;
                self.index += next.len_utf8();
                if raw {
                    result.push('\\');
                    result.push(next);
                    continue;
                }
                match next {
                    '\\' | '\'' | '"' => result.push(next),
                    'n' => result.push('\n'),
                    'r' => result.push('\r'),
                    't' => result.push('\t'),
                    'b' => result.push('\u{0008}'),
                    'f' => result.push('\u{000c}'),
                    'v' => result.push('\u{000b}'),
                    'a' => result.push('\u{0007}'),
                    '\n' => {}
                    '\r' => {
                        self.take(b'\n');
                    }
                    'x' | 'u' | 'U' => {
                        let count = match next {
                            'x' => 2,
                            'u' => 4,
                            _ => 8,
                        };
                        let end = self.index.checked_add(count).ok_or_else(syntax)?;
                        let digits = self
                            .source
                            .get(self.index..end)
                            .filter(|digits| digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
                            .ok_or_else(syntax)?;
                        let character = u32::from_str_radix(digits, 16)
                            .ok()
                            .and_then(char::from_u32)
                            .ok_or_else(syntax)?;
                        self.index = end;
                        result.push(character);
                    }
                    // Named Unicode escapes require a Unicode name database;
                    // reject the unsupported form rather than changing its value.
                    'N' => return Err(syntax()),
                    '0'..='7' => {
                        let mut value = next.to_digit(8).unwrap();
                        for _ in 0..2 {
                            let Some(byte @ b'0'..=b'7') = self.source.as_bytes().get(self.index)
                            else {
                                break;
                            };
                            value = value * 8 + u32::from(*byte - b'0');
                            self.index += 1;
                        }
                        result.push(char::from_u32(value).ok_or_else(syntax)?);
                    }
                    _ => {
                        result.push('\\');
                        result.push(next);
                    }
                }
            } else {
                let character = self.source[self.index..]
                    .chars()
                    .next()
                    .ok_or_else(syntax)?;
                result.push(character);
                self.index += character.len_utf8();
            }
        }
    }
}

struct Node {
    expr: Expr,
    depth: usize,
}
struct Parser {
    tokens: Vec<Token>,
    index: usize,
    nesting: usize,
    nodes: usize,
}
impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.index]
    }
    fn take(&mut self, token: &Token) -> bool {
        if self.peek() == token {
            self.index += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, token: &Token) -> Result<()> {
        if self.take(token) {
            Ok(())
        } else {
            Err(syntax())
        }
    }
    fn node(&mut self, expr: Expr, depth: usize) -> Result<Node> {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES {
            return Err(syntax());
        }
        Ok(Node { expr, depth })
    }
    fn nested(&mut self, parser: fn(&mut Self) -> Result<Node>) -> Result<Node> {
        self.nesting += 1;
        if self.nesting > MAX_DEPTH {
            return Err(syntax());
        }
        let result = parser(self);
        self.nesting -= 1;
        result
    }
    fn expression(&mut self) -> Result<Node> {
        self.nested(Self::conditional)
    }
    fn conditional(&mut self) -> Result<Node> {
        let yes = self.or()?;
        if !self.take(&Token::If) {
            return Ok(yes);
        }
        let test = self.or()?;
        self.expect(&Token::Else)?;
        let no = self.expression()?;
        let depth = 1 + yes.depth.max(test.depth).max(no.depth);
        self.node(
            Expr::Conditional {
                test: Box::new(test.expr),
                yes: Box::new(yes.expr),
                no: Box::new(no.expr),
            },
            depth,
        )
    }
    fn or(&mut self) -> Result<Node> {
        let mut left = self.and()?;
        while self.take(&Token::Or) {
            let right = self.and()?;
            left = self.binary(left, BinaryOp::Or, right)?;
        }
        Ok(left)
    }
    fn and(&mut self) -> Result<Node> {
        let mut left = self.not()?;
        while self.take(&Token::And) {
            let right = self.not()?;
            left = self.binary(left, BinaryOp::And, right)?;
        }
        Ok(left)
    }
    fn not(&mut self) -> Result<Node> {
        if self.take(&Token::Not) {
            let inner = self.nested(Self::not)?;
            self.unary(UnaryOp::Not, inner)
        } else {
            self.compare()
        }
    }
    fn compare(&mut self) -> Result<Node> {
        let left = self.add()?;
        let mut rest = Vec::new();
        let mut depth = left.depth;
        loop {
            let operator = if self.take(&Token::Eq) {
                CompareOp::Eq
            } else if self.take(&Token::Ne) {
                CompareOp::Ne
            } else if self.take(&Token::Lt) {
                CompareOp::Lt
            } else if self.take(&Token::Le) {
                CompareOp::Le
            } else if self.take(&Token::Gt) {
                CompareOp::Gt
            } else if self.take(&Token::Ge) {
                CompareOp::Ge
            } else if self.take(&Token::In) {
                CompareOp::In
            } else if self.take(&Token::Is) {
                if self.take(&Token::Not) {
                    CompareOp::IsNot
                } else {
                    CompareOp::Is
                }
            } else if self.take(&Token::Not) {
                self.expect(&Token::In)?;
                CompareOp::NotIn
            } else {
                break;
            };
            let right = self.add()?;
            depth = depth.max(right.depth);
            rest.push((operator, right.expr));
        }
        if rest.is_empty() {
            Ok(left)
        } else {
            self.node(Expr::Compare(Box::new(left.expr), rest), depth + 1)
        }
    }
    fn add(&mut self) -> Result<Node> {
        let mut left = self.product()?;
        loop {
            let operator = if self.take(&Token::Plus) {
                BinaryOp::Add
            } else if self.take(&Token::Minus) {
                BinaryOp::Sub
            } else {
                break;
            };
            let right = self.product()?;
            left = self.binary(left, operator, right)?;
        }
        Ok(left)
    }
    fn product(&mut self) -> Result<Node> {
        let mut left = self.factor()?;
        loop {
            let operator = if self.take(&Token::Star) {
                BinaryOp::Mul
            } else if self.take(&Token::Slash) {
                BinaryOp::Div
            } else if self.take(&Token::FloorDiv) {
                BinaryOp::FloorDiv
            } else if self.take(&Token::Mod) {
                BinaryOp::Mod
            } else {
                break;
            };
            let right = self.factor()?;
            left = self.binary(left, operator, right)?;
        }
        Ok(left)
    }
    fn factor(&mut self) -> Result<Node> {
        if self.take(&Token::Minus) {
            if self.peek() == &Token::MinMagnitude
                && self.tokens.get(self.index + 1) != Some(&Token::Pow)
            {
                self.index += 1;
                return self.node(Expr::Literal(Value::Int(i128::MIN)), 1);
            }
            let inner = self.nested(Self::factor)?;
            self.unary(UnaryOp::Neg, inner)
        } else if self.take(&Token::Plus) {
            let inner = self.nested(Self::factor)?;
            self.unary(UnaryOp::Pos, inner)
        } else {
            self.power()
        }
    }
    fn power(&mut self) -> Result<Node> {
        let left = self.postfix()?;
        if self.take(&Token::Pow) {
            let right = self.nested(Self::factor)?;
            self.binary(left, BinaryOp::Pow, right)
        } else {
            Ok(left)
        }
    }
    fn unary(&mut self, operator: UnaryOp, inner: Node) -> Result<Node> {
        self.node(Expr::Unary(operator, Box::new(inner.expr)), inner.depth + 1)
    }
    fn binary(&mut self, left: Node, operator: BinaryOp, right: Node) -> Result<Node> {
        let depth = 1 + left.depth.max(right.depth);
        self.node(
            Expr::Binary(Box::new(left.expr), operator, Box::new(right.expr)),
            depth,
        )
    }
    fn postfix(&mut self) -> Result<Node> {
        let mut value = self.atom()?;
        loop {
            if self.take(&Token::LBracket) {
                let index = self.expression()?;
                self.expect(&Token::RBracket)?;
                let depth = 1 + value.depth.max(index.depth);
                value = self.node(
                    Expr::Index(Box::new(value.expr), Box::new(index.expr)),
                    depth,
                )?;
            } else if self.take(&Token::LParen) {
                let Expr::Name(name) = value.expr else {
                    return Err(syntax());
                };
                if !safe_call(&name) {
                    return Err(syntax());
                }
                let arguments = self.arguments(Token::RParen)?;
                let depth = 1 + arguments
                    .iter()
                    .map(|argument| argument.depth)
                    .max()
                    .unwrap_or(0);
                value = self.node(
                    Expr::Call(
                        name,
                        arguments
                            .into_iter()
                            .map(|argument| argument.expr)
                            .collect(),
                    ),
                    depth,
                )?;
            } else if self.take(&Token::Dot) {
                let Token::Name(name) = self.peek().clone() else {
                    return Err(syntax());
                };
                self.index += 1;
                if !safe_method(&name) {
                    return Err(syntax());
                }
                self.expect(&Token::LParen)?;
                let arguments = self.arguments(Token::RParen)?;
                let depth = 1 + value.depth.max(
                    arguments
                        .iter()
                        .map(|argument| argument.depth)
                        .max()
                        .unwrap_or(0),
                );
                value = self.node(
                    Expr::Method(
                        Box::new(value.expr),
                        name,
                        arguments
                            .into_iter()
                            .map(|argument| argument.expr)
                            .collect(),
                    ),
                    depth,
                )?;
            } else {
                break;
            }
        }
        Ok(value)
    }
    fn arguments(&mut self, close: Token) -> Result<Vec<Node>> {
        let mut values = Vec::new();
        if self.take(&close) {
            return Ok(values);
        }
        loop {
            values.push(self.expression()?);
            if self.take(&close) {
                return Ok(values);
            }
            self.expect(&Token::Comma)?;
            if self.take(&close) {
                return Ok(values);
            }
        }
    }
    fn atom(&mut self) -> Result<Node> {
        let token = self.peek().clone();
        self.index += 1;
        match token {
            Token::None => self.node(Expr::Literal(Value::None), 1),
            Token::True => self.node(Expr::Literal(Value::Bool(true)), 1),
            Token::False => self.node(Expr::Literal(Value::Bool(false)), 1),
            Token::Int(value) => self.node(Expr::Literal(Value::Int(value)), 1),
            Token::Float(value) => self.node(Expr::Literal(Value::Float(value)), 1),
            Token::String(mut value) => {
                while let Token::String(next) = self.peek() {
                    value = Arc::from(format!("{value}{next}"));
                    self.index += 1;
                }
                self.node(Expr::Literal(Value::String(value)), 1)
            }
            Token::Name(name) => self.node(Expr::Name(name), 1),
            Token::LBracket => {
                let values = self.arguments(Token::RBracket)?;
                self.list(values)
            }
            Token::LParen => {
                if self.take(&Token::RParen) {
                    return self.tuple(Vec::new());
                }
                let first = self.expression()?;
                if self.take(&Token::RParen) {
                    return Ok(first);
                }
                self.expect(&Token::Comma)?;
                let mut values = vec![first];
                if !self.take(&Token::RParen) {
                    values.extend(self.arguments(Token::RParen)?);
                }
                self.tuple(values)
            }
            Token::LBrace => {
                let mut values = Vec::new();
                let mut depth = 0;
                if !self.take(&Token::RBrace) {
                    loop {
                        let Token::String(key) = self.peek().clone() else {
                            return Err(syntax());
                        };
                        self.index += 1;
                        self.expect(&Token::Colon)?;
                        let value = self.expression()?;
                        depth = depth.max(value.depth);
                        values.push((key.to_string(), value.expr));
                        if self.take(&Token::RBrace) {
                            break;
                        }
                        self.expect(&Token::Comma)?;
                        if self.take(&Token::RBrace) {
                            break;
                        }
                    }
                }
                self.node(Expr::Map(values), depth + 1)
            }
            _ => Err(syntax()),
        }
    }
    fn list(&mut self, values: Vec<Node>) -> Result<Node> {
        let depth = 1 + values.iter().map(|value| value.depth).max().unwrap_or(0);
        self.node(
            Expr::List(values.into_iter().map(|value| value.expr).collect()),
            depth,
        )
    }
    fn tuple(&mut self, values: Vec<Node>) -> Result<Node> {
        let depth = 1 + values.iter().map(|value| value.depth).max().unwrap_or(0);
        self.node(
            Expr::Tuple(values.into_iter().map(|value| value.expr).collect()),
            depth,
        )
    }
}
fn integer_token(digits: &str, radix: u32) -> Result<Token> {
    let magnitude = u128::from_str_radix(digits, radix).map_err(|_| syntax())?;
    if magnitude == 1u128 << 127 {
        Ok(Token::MinMagnitude)
    } else {
        i128::try_from(magnitude)
            .map(Token::Int)
            .map_err(|_| syntax())
    }
}
fn safe_method(name: &str) -> bool {
    matches!(
        name,
        "get"
            | "lower"
            | "upper"
            | "startswith"
            | "endswith"
            | "strip"
            | "lstrip"
            | "rstrip"
            | "replace"
            | "split"
            | "join"
            | "find"
            | "index"
            | "count"
    )
}

fn safe_call(name: &str) -> bool {
    matches!(
        name,
        "len"
            | "bool"
            | "str"
            | "int"
            | "float"
            | "abs"
            | "round"
            | "contains"
            | "any"
            | "all"
            | "min"
            | "max"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn int(value: i128) -> Expr {
        Expr::Literal(Value::Int(value))
    }
    fn name(value: &str) -> Expr {
        Expr::Name(value.into())
    }
    fn binary(left: Expr, operator: BinaryOp, right: Expr) -> Expr {
        Expr::Binary(Box::new(left), operator, Box::new(right))
    }

    #[test]
    fn boolean_not_and_chained_comparisons_follow_python_precedence() {
        let expected = binary(
            binary(
                Expr::Unary(
                    UnaryOp::Not,
                    Box::new(Expr::Compare(
                        Box::new(int(1)),
                        vec![(CompareOp::Lt, int(2)), (CompareOp::Le, int(3))],
                    )),
                ),
                BinaryOp::And,
                Expr::Compare(
                    Box::new(name("x")),
                    vec![(CompareOp::NotIn, Expr::List(vec![int(4), int(5)]))],
                ),
            ),
            BinaryOp::Or,
            Expr::Compare(
                Box::new(name("y")),
                vec![(CompareOp::IsNot, Expr::Literal(Value::None))],
            ),
        );
        assert_eq!(
            parse("not 1 < 2 <= 3 and x not in [4, 5] or y is not None").unwrap(),
            expected
        );
    }

    #[test]
    fn power_is_right_associative_and_binds_differently_from_unary_signs() {
        assert_eq!(
            parse("-2**2").unwrap(),
            Expr::Unary(
                UnaryOp::Neg,
                Box::new(binary(int(2), BinaryOp::Pow, int(2)))
            )
        );
        assert_eq!(
            parse("2**-2").unwrap(),
            binary(
                int(2),
                BinaryOp::Pow,
                Expr::Unary(UnaryOp::Neg, Box::new(int(2)))
            )
        );
        assert_eq!(
            parse("2**3**4").unwrap(),
            binary(int(2), BinaryOp::Pow, binary(int(3), BinaryOp::Pow, int(4)))
        );
        assert_eq!(
            parse("1 + 2 * 3 // 4 % 5 - 6 / 7").unwrap(),
            binary(
                binary(
                    int(1),
                    BinaryOp::Add,
                    binary(
                        binary(
                            binary(int(2), BinaryOp::Mul, int(3)),
                            BinaryOp::FloorDiv,
                            int(4)
                        ),
                        BinaryOp::Mod,
                        int(5)
                    )
                ),
                BinaryOp::Sub,
                binary(int(6), BinaryOp::Div, int(7))
            )
        );
    }

    #[test]
    fn conditionals_associate_to_the_right_with_or_in_the_condition() {
        assert_eq!(
            parse("a if b or c else d if e else f").unwrap(),
            Expr::Conditional {
                yes: Box::new(name("a")),
                test: Box::new(binary(name("b"), BinaryOp::Or, name("c"))),
                no: Box::new(Expr::Conditional {
                    yes: Box::new(name("d")),
                    test: Box::new(name("e")),
                    no: Box::new(name("f"))
                })
            }
        );
    }

    #[test]
    fn literals_support_bounded_numeric_forms_unicode_escapes_raw_strings_and_adjacent_strings() {
        assert_eq!(
            parse("0x_FF + 0b101 + 0o17 + 1_000").unwrap(),
            binary(
                binary(
                    binary(int(255), BinaryOp::Add, int(5)),
                    BinaryOp::Add,
                    int(15)
                ),
                BinaryOp::Add,
                int(1000)
            )
        );
        assert_eq!(
            parse(".5 + 1. + 2.5e-3").unwrap(),
            binary(
                binary(
                    Expr::Literal(Value::Float(0.5)),
                    BinaryOp::Add,
                    Expr::Literal(Value::Float(1.0))
                ),
                BinaryOp::Add,
                Expr::Literal(Value::Float(0.0025))
            )
        );
        assert_eq!(
            parse(r#"'a\n\x42\u263a\U0001f600' r'\d+'"#).unwrap(),
            Expr::Literal(Value::String(Arc::from("a\nB☺😀\\d+")))
        );
        assert_eq!(
            parse("'''a\nb'''\n'c'").unwrap(),
            Expr::Literal(Value::String(Arc::from("a\nbc")))
        );
        assert_eq!(
            parse("(True, False, None,)").unwrap(),
            Expr::Tuple(vec![
                Expr::Literal(Value::Bool(true)),
                Expr::Literal(Value::Bool(false)),
                Expr::Literal(Value::None)
            ])
        );
    }

    #[test]
    fn safe_calls_methods_maps_and_indexing_preserve_expression_structure() {
        let expression = parse("len({'name': ['One', 'Two']}['name'][0].lower()) == 3").unwrap();
        assert!(matches!(expression,Expr::Compare(_,ref rest) if rest.len()==1));
        assert!(parse("metadata.get('width', 0) >= 100").is_ok());
        assert!(parse("'_private' in _url").is_ok());
        assert!(parse("extension in ('jpg', 'png')").is_ok());
        assert_eq!(parse("()").unwrap(), Expr::Tuple(Vec::new()));
        assert_eq!(parse("(42)").unwrap(), int(42));
        assert_eq!(parse("(42,)").unwrap(), Expr::Tuple(vec![int(42)]));
    }

    #[test]
    fn signed_minimum_literal_is_supported_without_accepting_positive_overflow() {
        let magnitude = 1u128 << 127;
        for positive in [
            magnitude.to_string(),
            format!("0x{magnitude:x}"),
            format!("0o{magnitude:o}"),
            format!("0b{magnitude:b}"),
        ] {
            assert_eq!(parse(&format!("-{positive}")).unwrap(), int(i128::MIN));
            assert!(parse(&positive).is_err());
            assert!(parse(&format!("+{positive}")).is_err());
            assert!(parse(&format!("-{positive} ** 0")).is_err());
            assert!(parse(&format!("-({positive})")).is_err());
        }
        assert!(parse("-170141183460469231731687303715884105729").is_err());
        assert!(parse("-170141183460469231731687303715884105728 == int('-170141183460469231731687303715884105728')").is_ok());
    }

    #[test]
    fn unsupported_python_execution_attributes_and_malformed_input_are_rejected_privately() {
        for source in [
            "",
            "a +",
            "[1,",
            "{'key':}",
            "f'private{value}'",
            "lambda x: x",
            "[x for x in xs]",
            "{x: x for x in xs}",
            "__import__('private')",
            "open('private')",
            "getattr(x, 'private')",
            "obj.__class__",
            "obj.private",
            "obj.private()",
            "{'a':1}.keys()",
            "{'a':1}.values()",
            "{'a':1}.items()",
            "re.search('private', caption)",
            "'ß'.casefold()",
            "'²'.isdigit()",
            "a[1:2]",
            "len(value=1)",
            "1e999",
            "1__0",
            "012",
            "0x",
            "0x__f",
            "1e+",
            "'\\uD800'",
            "'\\N{LATIN CAPITAL LETTER A}'",
            "'actual\0nul'",
            "'unterminated",
        ] {
            let error = parse(source).unwrap_err().to_string();
            assert!(!error.contains("private"), "{source}");
            assert!(!error.contains("unterminated"));
        }
    }

    #[test]
    fn source_tokens_recursive_nesting_and_left_chains_are_bounded() {
        assert!(parse(&"x".repeat(MAX_SOURCE + 1)).is_err());
        assert!(parse(&format!("[{}]", vec!["1"; 2200].join(","))).is_err());
        assert!(parse(&format!("{}1{}", "(".repeat(70), ")".repeat(70))).is_err());
        assert!(parse(&format!("{}True", "not ".repeat(70))).is_err());
        assert!(parse(&vec!["1"; 70].join("+")).is_err());
        assert!(parse(&vec!["1"; 70].join("**")).is_err());
        assert!(parse(&format!("[{}]", vec!["1"; 2000].join(","))).is_ok());
        assert!(parse(&vec!["1"; 1000].join(" < ")).is_ok());
    }
}
