//! One Pratt parser over one precedence table.

use super::ast::{
    BinaryOp, CWord, CastForm, Field, Node, NodeId, NodeKind, Path, Segment, Separator, SizeOf,
    Tag, Tree, TypeBase, TypeName, UnaryOp,
};
use super::lexer::{Punct, Token, TokenKind, lex};
use super::{Ambiguity, MAX_AMBIGUITIES, Span};
use crate::eval::error::{ErrorKind, ExpressionError};

/// The deepest nesting accepted. Binding, running, and printing recurse
/// along the tree, so its height is bounded.
pub const MAX_DEPTH: usize = 64;

/// The most nodes one reading may hold.
pub const MAX_NODES: usize = 1024;

/// Words that are never names unless quoted.
pub const RESERVED: [&str; 8] = [
    "true", "false", "null", "as", "sizeof", "nil", "nullptr", "NULL",
];

/// Words that begin or qualify a type, and so are no bare name.
pub const QUALIFIERS: [&str; 3] = ["const", "volatile", "mut"];

/// Whether an unquoted word can be a segment of a path.
pub fn is_name_word(word: &str) -> bool {
    !RESERVED.contains(&word) && !is_type_word(word)
}

/// Whether a word begins or qualifies a type.
pub fn is_type_word(word: &str) -> bool {
    CWord::parse(word).is_some() || Tag::parse(word).is_some() || QUALIFIERS.contains(&word)
}

/// Binding strengths, loosest first.
mod precedence {
    pub const ASSIGN: u8 = 1;
    pub const CONDITIONAL: u8 = 2;
    pub const OR: u8 = 3;
    pub const AND: u8 = 4;
    pub const COMPARISON: u8 = 5;
    pub const BIT_OR: u8 = 6;
    pub const BIT_XOR: u8 = 7;
    pub const BIT_AND: u8 = 8;
    pub const SHIFT: u8 = 9;
    pub const ADDITIVE: u8 = 10;
    pub const MULTIPLICATIVE: u8 = 11;
    pub const AS: u8 = 12;
    pub const PREFIX: u8 = 13;
    pub const POSTFIX: u8 = 14;
}

pub use precedence::{
    ADDITIVE, AND, AS, ASSIGN, BIT_AND, BIT_OR, BIT_XOR, COMPARISON, CONDITIONAL, MULTIPLICATIVE,
    OR, POSTFIX, PREFIX, SHIFT,
};

/// A binary operator's precedence.
pub const fn binary_precedence(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => MULTIPLICATIVE,
        BinaryOp::Add | BinaryOp::Sub => ADDITIVE,
        BinaryOp::Shl | BinaryOp::Shr => SHIFT,
        BinaryOp::BitAnd => BIT_AND,
        BinaryOp::BitXor => BIT_XOR,
        BinaryOp::BitOr => BIT_OR,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            COMPARISON
        }
        BinaryOp::And => AND,
        BinaryOp::Or => OR,
    }
}

pub const fn binary_text(op: BinaryOp) -> &'static str {
    binary_punct(op).text()
}

const fn binary_punct(op: BinaryOp) -> Punct {
    match op {
        BinaryOp::Mul => Punct::Star,
        BinaryOp::Div => Punct::Slash,
        BinaryOp::Rem => Punct::Percent,
        BinaryOp::Add => Punct::Plus,
        BinaryOp::Sub => Punct::Minus,
        BinaryOp::Shl => Punct::Shl,
        BinaryOp::Shr => Punct::Shr,
        BinaryOp::BitAnd => Punct::Amp,
        BinaryOp::BitXor => Punct::Caret,
        BinaryOp::BitOr => Punct::Pipe,
        BinaryOp::Eq => Punct::EqEq,
        BinaryOp::Ne => Punct::Ne,
        BinaryOp::Lt => Punct::Lt,
        BinaryOp::Le => Punct::Le,
        BinaryOp::Gt => Punct::Gt,
        BinaryOp::Ge => Punct::Ge,
        BinaryOp::And => Punct::AndAnd,
        BinaryOp::Or => Punct::OrOr,
    }
}

const fn binary_op(punct: Punct) -> Option<BinaryOp> {
    Some(match punct {
        Punct::Star => BinaryOp::Mul,
        Punct::Slash => BinaryOp::Div,
        Punct::Percent => BinaryOp::Rem,
        Punct::Plus => BinaryOp::Add,
        Punct::Minus => BinaryOp::Sub,
        Punct::Shl => BinaryOp::Shl,
        Punct::Shr => BinaryOp::Shr,
        Punct::Amp => BinaryOp::BitAnd,
        Punct::Caret => BinaryOp::BitXor,
        Punct::Pipe => BinaryOp::BitOr,
        Punct::EqEq => BinaryOp::Eq,
        Punct::Ne => BinaryOp::Ne,
        Punct::Lt => BinaryOp::Lt,
        Punct::Le => BinaryOp::Le,
        Punct::Gt => BinaryOp::Gt,
        Punct::Ge => BinaryOp::Ge,
        Punct::AndAnd => BinaryOp::And,
        Punct::OrOr => BinaryOp::Or,
        _ => return None,
    })
}

/// A compound assignment's operator.
const fn compound(punct: Punct) -> Option<BinaryOp> {
    Some(match punct {
        Punct::PlusEq => BinaryOp::Add,
        Punct::MinusEq => BinaryOp::Sub,
        Punct::StarEq => BinaryOp::Mul,
        Punct::SlashEq => BinaryOp::Div,
        Punct::PercentEq => BinaryOp::Rem,
        Punct::AmpEq => BinaryOp::BitAnd,
        Punct::PipeEq => BinaryOp::BitOr,
        Punct::CaretEq => BinaryOp::BitXor,
        Punct::ShlEq => BinaryOp::Shl,
        Punct::ShrEq => BinaryOp::Shr,
        _ => return None,
    })
}

pub fn assignment_text(op: Option<BinaryOp>) -> &'static str {
    match op {
        None => "=",
        Some(BinaryOp::Add) => "+=",
        Some(BinaryOp::Sub) => "-=",
        Some(BinaryOp::Mul) => "*=",
        Some(BinaryOp::Div) => "/=",
        Some(BinaryOp::Rem) => "%=",
        Some(BinaryOp::BitAnd) => "&=",
        Some(BinaryOp::BitOr) => "|=",
        Some(BinaryOp::BitXor) => "^=",
        Some(BinaryOp::Shl) => "<<=",
        Some(BinaryOp::Shr) => ">>=",
        Some(_) => unreachable!("only arithmetic and bit operators assign"),
    }
}

/// One reading per combination of an expression's ambiguities.
pub type Readings = Vec<Result<Tree, ExpressionError>>;

/// Every reading of `text`, with the ambiguities that tell them apart.
pub fn parse(text: &str) -> Result<(Vec<Ambiguity>, Readings), ExpressionError> {
    let tokens = lex(text)?;
    let starts = ambiguities(&tokens);
    if starts.len() > MAX_AMBIGUITIES {
        let span = tokens[starts[MAX_AMBIGUITIES]].span;
        return Err(ExpressionError::new(
            ErrorKind::Limit,
            span,
            format!("an expression may hold at most {MAX_AMBIGUITIES} parenthesized names before `-`, `*`, or `&`"),
        )
        .with_hint("write casts as `x as T`"));
    }
    let mut ambiguity_list = Vec::with_capacity(starts.len());
    for &start in &starts {
        let mut parser = Parser::new(text, &tokens, &starts, 0);
        parser.position = start + 1;
        let name = parser.path()?;
        let close = parser.peek_span();
        ambiguity_list.push(Ambiguity {
            name,
            span: tokens[start].span.to(close),
        });
    }
    let readings: Vec<_> = (0..1_usize << starts.len())
        .map(|casts| Parser::new(text, &tokens, &starts, casts).expression_tree())
        .collect();
    if readings.iter().all(Result::is_err) {
        let first = readings.into_iter().next();
        return Err(first
            .and_then(Result::err)
            .expect("there is at least one reading"));
    }
    Ok((ambiguity_list, readings))
}

/// The tokens that begin a parenthesized name before `-`, `*`, or `&`.
fn ambiguities(tokens: &[Token]) -> Vec<usize> {
    let ident = |index: usize| match tokens.get(index).map(|token| &token.kind) {
        Some(TokenKind::Ident(word)) => Some(word.as_str()),
        _ => None,
    };
    let punct = |index: usize| match tokens.get(index).map(|token| &token.kind) {
        Some(TokenKind::Punct(punct)) => Some(*punct),
        _ => None,
    };
    let segment = |index: usize| match tokens.get(index).map(|token| &token.kind) {
        Some(TokenKind::Quoted(_)) => true,
        Some(TokenKind::Ident(word)) => is_name_word(word),
        _ => false,
    };
    let mut starts = Vec::new();
    for open in 0..tokens.len() {
        if punct(open) != Some(Punct::OpenParen)
            || open > 0 && matches!(ident(open - 1), Some("sizeof" | "len"))
        {
            continue;
        }
        let mut index = open + 1;
        if punct(index) == Some(Punct::ColonColon) {
            index += 1;
        }
        if !segment(index) {
            continue;
        }
        index += 1;
        while matches!(punct(index), Some(Punct::ColonColon | Punct::Dot)) && segment(index + 1) {
            index += 2;
        }
        if punct(index) == Some(Punct::CloseParen)
            && matches!(
                punct(index + 1),
                Some(Punct::Minus | Punct::Star | Punct::Amp)
            )
        {
            starts.push(open);
        }
    }
    starts
}

struct Parser<'tokens> {
    text: &'tokens str,
    tokens: &'tokens [Token],
    position: usize,
    nodes: Vec<Node>,
    depth: usize,
    /// The tokens that begin ambiguities, in order.
    ambiguities: &'tokens [usize],
    /// Bit `i` set reads ambiguity `i` as a cast.
    casts: usize,
}

impl<'tokens> Parser<'tokens> {
    const fn new(
        text: &'tokens str,
        tokens: &'tokens [Token],
        ambiguities: &'tokens [usize],
        casts: usize,
    ) -> Self {
        Self {
            text,
            tokens,
            position: 0,
            nodes: Vec::new(),
            depth: 0,
            ambiguities,
            casts,
        }
    }

    fn expression_tree(mut self) -> Result<Tree, ExpressionError> {
        let root = self.expression(ASSIGN)?;
        if !matches!(self.peek(), TokenKind::End) {
            return Err(self.unexpected("an operator or the end of the expression"));
        }
        let tree = Tree {
            nodes: self.nodes,
            root,
        };
        // A range only ever is the whole expression.
        for (index, node) in tree.nodes.iter().enumerate() {
            if matches!(node.kind, NodeKind::Range { .. }) && index != tree.root.0 as usize {
                return Err(ExpressionError::syntax(
                    node.span,
                    "a range must be the whole expression",
                ));
            }
        }
        Ok(tree)
    }

    fn peek(&self) -> &TokenKind {
        &self.tokens[self.position.min(self.tokens.len() - 1)].kind
    }

    fn peek_at(&self, offset: usize) -> &TokenKind {
        &self.tokens[(self.position + offset).min(self.tokens.len() - 1)].kind
    }

    fn peek_span(&self) -> Span {
        self.tokens[self.position.min(self.tokens.len() - 1)].span
    }

    fn peek_punct(&self) -> Option<Punct> {
        match self.peek() {
            TokenKind::Punct(punct) => Some(*punct),
            _ => None,
        }
    }

    fn peek_word(&self) -> Option<&str> {
        match self.peek() {
            TokenKind::Ident(word) => Some(word),
            _ => None,
        }
    }

    const fn previous_span(&self) -> Span {
        self.tokens[self.position.saturating_sub(1)].span
    }

    fn advance(&mut self) -> Span {
        let span = self.peek_span();
        self.position = (self.position + 1).min(self.tokens.len() - 1);
        span
    }

    fn eat(&mut self, punct: Punct) -> bool {
        let found = self.peek_punct() == Some(punct);
        if found {
            self.advance();
        }
        found
    }

    fn expect(&mut self, punct: Punct, context: &str) -> Result<Span, ExpressionError> {
        if self.peek_punct() == Some(punct) {
            return Ok(self.advance());
        }
        Err(self.unexpected(&format!("`{}` {context}", punct.text())))
    }

    fn unexpected(&self, expected: &str) -> ExpressionError {
        let found = match self.peek() {
            TokenKind::End => "the end of the expression".to_owned(),
            _ => format!("`{}`", self.peek_span().text(self.text)),
        };
        ExpressionError::syntax(
            self.peek_span(),
            format!("expected {expected}, found {found}"),
        )
    }

    fn push(&mut self, kind: NodeKind, span: Span) -> Result<NodeId, ExpressionError> {
        if self.nodes.len() >= MAX_NODES {
            return Err(ExpressionError::new(
                ErrorKind::Limit,
                span,
                format!("an expression may have at most {MAX_NODES} parts"),
            ));
        }
        let id = NodeId(u32::try_from(self.nodes.len()).unwrap_or(u32::MAX));
        self.nodes.push(Node { kind, span });
        Ok(id)
    }

    fn span(&self, id: NodeId) -> Span {
        self.nodes[id.0 as usize].span
    }

    fn enter(&mut self) -> Result<(), ExpressionError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ExpressionError::new(
                ErrorKind::Limit,
                self.peek_span(),
                format!("an expression may nest at most {MAX_DEPTH} deep"),
            ));
        }
        Ok(())
    }

    const fn leave(&mut self) {
        self.depth -= 1;
    }

    /// An expression whose operators bind at least as tightly as
    /// `minimum`.
    fn expression(&mut self, minimum: u8) -> Result<NodeId, ExpressionError> {
        self.enter()?;
        let result = self.expression_inner(minimum);
        self.leave();
        result
    }

    fn expression_inner(&mut self, minimum: u8) -> Result<NodeId, ExpressionError> {
        let mut left = self.prefix()?;
        loop {
            if self.peek_word() == Some("as") {
                if AS < minimum {
                    break;
                }
                self.advance();
                let (ty, _) = self.type_name(false)?;
                let span = self.span(left).to(ty.span);
                left = self.push(
                    NodeKind::Cast {
                        ty,
                        operand: left,
                        form: CastForm::As,
                    },
                    span,
                )?;
                continue;
            }
            let Some(punct) = self.peek_punct() else {
                break;
            };
            if let Some(op) = binary_op(punct) {
                let strength = binary_precedence(op);
                if strength < minimum {
                    break;
                }
                self.advance();
                let right = self.expression(strength + 1)?;
                let span = self.span(left).to(self.span(right));
                left = self.push(NodeKind::Binary { op, left, right }, span)?;
                if strength == COMPARISON
                    && self.peek_punct().and_then(binary_op).map(binary_precedence)
                        == Some(COMPARISON)
                {
                    return Err(ExpressionError::syntax(
                        self.peek_span(),
                        "comparisons do not chain",
                    )
                    .with_hint("join comparisons with `&&`"));
                }
            } else if punct == Punct::Question {
                if CONDITIONAL < minimum {
                    break;
                }
                self.advance();
                let then = self.expression(CONDITIONAL)?;
                self.expect(Punct::Colon, "to separate the branches of `?`")?;
                let otherwise = self.expression(CONDITIONAL)?;
                let span = self.span(left).to(self.span(otherwise));
                left = self.push(
                    NodeKind::Conditional {
                        condition: left,
                        then,
                        otherwise,
                    },
                    span,
                )?;
            } else if punct == Punct::Assign || compound(punct).is_some() {
                let op = compound(punct);
                if ASSIGN < minimum {
                    break;
                }
                self.advance();
                let value = self.expression(ASSIGN)?;
                let span = self.span(left).to(self.span(value));
                left = self.push(
                    NodeKind::Assign {
                        op,
                        target: left,
                        value,
                    },
                    span,
                )?;
            } else {
                break;
            }
        }
        Ok(left)
    }

    /// A prefix operator's operand, or a postfix expression.
    fn prefix(&mut self) -> Result<NodeId, ExpressionError> {
        self.enter()?;
        let result = self.prefix_inner();
        self.leave();
        result
    }

    fn prefix_inner(&mut self) -> Result<NodeId, ExpressionError> {
        let start = self.peek_span();
        let op = match self.peek_punct() {
            Some(Punct::Minus) => Some(UnaryOp::Neg),
            Some(Punct::Bang) => Some(UnaryOp::Not),
            Some(Punct::Tilde) => Some(UnaryOp::BitNot),
            Some(Punct::Star) => Some(UnaryOp::Deref),
            Some(Punct::Amp) => Some(UnaryOp::AddressOf),
            Some(Punct::OpenParen) => return self.parenthesized(),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let operand = self.prefix()?;
            let span = start.to(self.span(operand));
            return self.push(NodeKind::Unary { op, operand }, span);
        }
        if self.peek_word() == Some("sizeof") {
            return self.size_of();
        }
        let primary = self.primary()?;
        self.postfix(primary)
    }

    fn size_of(&mut self) -> Result<NodeId, ExpressionError> {
        let start = self.advance();
        self.expect(Punct::OpenParen, "after `sizeof`")?;
        let checkpoint = self.position;
        if let Ok((ty, certain)) = self.type_name(true)
            && self.peek_punct() == Some(Punct::CloseParen)
            && certain
        {
            let close = self.advance();
            return self.push(NodeKind::SizeOf(SizeOf::Type(ty)), start.to(close));
        }
        self.position = checkpoint;
        let operand = self.expression(ASSIGN)?;
        let close = self.expect(Punct::CloseParen, "to close `sizeof(`")?;
        self.push(NodeKind::SizeOf(SizeOf::Operand(operand)), start.to(close))
    }

    /// `(` begins a cast or a parenthesized expression.
    fn parenthesized(&mut self) -> Result<NodeId, ExpressionError> {
        let open = self.position;
        let start = self.peek_span();
        if let Some(index) = self.ambiguities.iter().position(|&token| token == open) {
            if self.casts & (1 << index) != 0 {
                self.advance();
                let (ty, _) = self.type_name(true)?;
                self.expect(Punct::CloseParen, "to close the cast's type")?;
                return self.prefix_cast(start, ty);
            }
            let inner = self.grouped()?;
            return self.postfix(inner);
        }

        self.advance();
        if let Ok((ty, certain)) = self.type_name(true)
            && self.peek_punct() == Some(Punct::CloseParen)
            && (certain || begins_operand_only(self.peek_at(1)))
        {
            self.advance();
            return self.prefix_cast(start, ty);
        }
        self.position = open;
        let inner = self.grouped()?;
        self.postfix(inner)
    }

    fn prefix_cast(&mut self, start: Span, ty: TypeName) -> Result<NodeId, ExpressionError> {
        if !begins_operand(self.peek()) {
            return Err(self.unexpected(&format!(
                "an operand for the cast to `{}`",
                super::print::type_text(&ty)
            )));
        }
        let operand = self.prefix()?;
        let span = start.to(self.span(operand));
        self.push(
            NodeKind::Cast {
                ty,
                operand,
                form: CastForm::Prefix,
            },
            span,
        )
    }

    /// `( expression )`; the expression's span takes in the parentheses, so
    /// whatever contains it points at balanced text.
    fn grouped(&mut self) -> Result<NodeId, ExpressionError> {
        let open = self.expect(Punct::OpenParen, "")?;
        let inner = self.expression(ASSIGN)?;
        let close = self.expect(Punct::CloseParen, "to close `(`")?;
        self.nodes[inner.0 as usize].span = open.to(close);
        Ok(inner)
    }

    fn primary(&mut self) -> Result<NodeId, ExpressionError> {
        let span = self.peek_span();
        let kind = match self.peek().clone() {
            TokenKind::Ident(word) => match word.as_str() {
                "true" | "false" => {
                    self.advance();
                    NodeKind::Bool(word == "true")
                }
                "null" => {
                    self.advance();
                    NodeKind::Null
                }
                "nil" | "nullptr" | "NULL" => {
                    return Err(ExpressionError::syntax(
                        span,
                        format!("`{word}` is spelled `null`"),
                    )
                    .with_hint("write `null`"));
                }
                "len" if self.peek_at(1) == &TokenKind::Punct(Punct::OpenParen) => {
                    self.advance();
                    self.advance();
                    let operand = self.expression(ASSIGN)?;
                    let close = self.expect(Punct::CloseParen, "to close `len(`")?;
                    return self.push(NodeKind::Len(operand), span.to(close));
                }
                "as" | "sizeof" => return Err(self.unexpected("an operand")),
                _ => NodeKind::Name(self.path()?),
            },
            TokenKind::Quoted(_) | TokenKind::Punct(Punct::ColonColon) => {
                NodeKind::Name(self.path()?)
            }
            TokenKind::Register(name) => {
                self.advance();
                NodeKind::Register(name)
            }
            TokenKind::Integer { value, suffix } => {
                self.advance();
                NodeKind::Integer { value, suffix }
            }
            TokenKind::Float(value) => {
                self.advance();
                NodeKind::Float(value)
            }
            TokenKind::Char(value) => {
                self.advance();
                NodeKind::Char(value)
            }
            TokenKind::Text(bytes) => {
                self.advance();
                NodeKind::Text(bytes)
            }
            TokenKind::Punct(_) | TokenKind::End => return Err(self.unexpected("an operand")),
        };
        let span = span.to(self.previous_span());
        if matches!(self.peek(), TokenKind::Punct(Punct::OpenParen))
            && matches!(kind, NodeKind::Name(_))
        {
            return Err(ExpressionError::syntax(
                span.to(self.peek_span()),
                "expressions cannot call functions",
            ));
        }
        self.push(kind, span)
    }

    /// A name and the names `::` or `.` join to it.
    fn path(&mut self) -> Result<Path, ExpressionError> {
        let global = self.eat(Punct::ColonColon);
        let mut segments = vec![self.segment(Separator::Colons)?];
        loop {
            let separator = match self.peek_punct() {
                Some(Punct::ColonColon) => Separator::Colons,
                Some(Punct::Dot) if is_segment(self.peek_at(1)) => Separator::Dot,
                _ => break,
            };
            self.advance();
            segments.push(self.segment(separator)?);
        }
        Ok(Path { global, segments })
    }

    fn segment(&mut self, separator: Separator) -> Result<Segment, ExpressionError> {
        let span = self.peek_span();
        let name = match self.peek() {
            TokenKind::Quoted(name) => name.clone(),
            TokenKind::Ident(word) if is_name_word(word) => word.clone(),
            _ => return Err(self.unexpected("a name")),
        };
        self.advance();
        Ok(Segment {
            separator,
            name,
            span,
        })
    }

    fn postfix(&mut self, mut base: NodeId) -> Result<NodeId, ExpressionError> {
        loop {
            match self.peek_punct() {
                Some(punct @ (Punct::Dot | Punct::Arrow)) => {
                    self.advance();
                    let field_span = self.peek_span();
                    let field = match self.peek().clone() {
                        TokenKind::Ident(word) if is_name_word(&word) => Field::Named(word),
                        TokenKind::Quoted(name) => Field::Named(name),
                        TokenKind::Integer {
                            value,
                            suffix: None,
                        } => Field::Index(u32::try_from(value).unwrap_or(u32::MAX)),
                        _ => return Err(self.unexpected("a member name")),
                    };
                    self.advance();
                    let span = self.span(base).to(field_span);
                    base = self.push(
                        NodeKind::Member {
                            base,
                            field,
                            field_span,
                            arrow: punct == Punct::Arrow,
                        },
                        span,
                    )?;
                }
                Some(Punct::OpenBracket) => {
                    self.advance();
                    let index = self.expression(ASSIGN)?;
                    if self.eat(Punct::DotDot) {
                        let end = self.expression(ASSIGN)?;
                        let close = self.expect(Punct::CloseBracket, "to close the range")?;
                        let span = self.span(base).to(close);
                        base = self.push(
                            NodeKind::Range {
                                base,
                                start: index,
                                end,
                            },
                            span,
                        )?;
                    } else {
                        let close = self.expect(Punct::CloseBracket, "to close `[`")?;
                        let span = self.span(base).to(close);
                        base = self.push(NodeKind::Index { base, index }, span)?;
                    }
                }
                _ => return Ok(base),
            }
        }
    }

    /// A type name, and whether it can only be a type: a bare path could
    /// also name a value. A pointer is `T*` inside a cast's parentheses,
    /// where `(*p)` dereferences, and `*T` after `as`, where a trailing `*`
    /// multiplies.
    fn type_name(&mut self, parenthesized: bool) -> Result<(TypeName, bool), ExpressionError> {
        let start = self.peek_span();
        let mut pointers = 0_u8;
        let mut qualified = false;
        loop {
            if !parenthesized && self.eat(Punct::Star) {
                pointers = pointers.saturating_add(1);
            } else if self
                .peek_word()
                .is_some_and(|word| QUALIFIERS.contains(&word))
            {
                self.advance();
                qualified = true;
            } else {
                break;
            }
        }
        let base = if let Some(tag) = self.peek_word().and_then(Tag::parse) {
            self.advance();
            TypeBase::Tagged(tag, self.path()?)
        } else if self.peek_word().and_then(CWord::parse).is_some() {
            let mut words = Vec::new();
            loop {
                if let Some(word) = self.peek_word().and_then(CWord::parse) {
                    words.push(word);
                } else if self
                    .peek_word()
                    .is_some_and(|word| QUALIFIERS.contains(&word))
                {
                    qualified = true;
                } else {
                    break;
                }
                self.advance();
            }
            words.sort_unstable();
            TypeBase::CWords(words)
        } else {
            TypeBase::Named(self.path()?)
        };
        let mut end = self.previous_span();
        loop {
            if self
                .peek_word()
                .is_some_and(|word| QUALIFIERS.contains(&word))
            {
                qualified = true;
            } else if parenthesized && self.peek_punct() == Some(Punct::Star) {
                pointers = pointers.saturating_add(1);
            } else {
                break;
            }
            end = self.advance();
        }
        let ty = TypeName {
            base,
            pointers,
            span: start.to(end),
        };
        let certain = qualified || !is_bare(&ty);
        Ok((ty, certain))
    }
}

/// Whether a type is a bare path, which could also be a value's name.
pub const fn is_bare(ty: &TypeName) -> bool {
    ty.pointers == 0 && matches!(ty.base, TypeBase::Named(_))
}

fn is_segment(token: &TokenKind) -> bool {
    match token {
        TokenKind::Quoted(_) => true,
        TokenKind::Ident(word) => is_name_word(word),
        _ => false,
    }
}

/// Whether a token can begin an operand.
fn begins_operand(token: &TokenKind) -> bool {
    begins_operand_only(token)
        || matches!(
            token,
            TokenKind::Punct(Punct::Minus | Punct::Star | Punct::Amp)
        )
}

/// Whether a token can begin an operand but cannot continue an expression.
fn begins_operand_only(token: &TokenKind) -> bool {
    match token {
        TokenKind::Ident(word) => word != "as",
        TokenKind::Quoted(_)
        | TokenKind::Register(_)
        | TokenKind::Integer { .. }
        | TokenKind::Float(_)
        | TokenKind::Char(_)
        | TokenKind::Text(_) => true,
        TokenKind::Punct(punct) => matches!(
            punct,
            Punct::OpenParen | Punct::Bang | Punct::Tilde | Punct::ColonColon
        ),
        TokenKind::End => false,
    }
}
