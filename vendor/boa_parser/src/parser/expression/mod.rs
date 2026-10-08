//! Expression parsing.
//!
//! More information:
//!  - [MDN documentation][mdn]
//!  - [ECMAScript specification][spec]
//!
//! [mdn]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Operators
//! [spec]: https://tc39.es/ecma262/#sec-ecmascript-language-expressions

mod assignment;
mod fpl_or_exp;
mod identifiers;
mod left_hand_side;
mod primary;
mod unary;
mod update;

pub(in crate::parser) mod await_expr;

#[cfg(test)]
mod tests;

use crate::{
    Error,
    lexer::{InputElement, TokenKind},
    parser::{
        AllowAwait, AllowIn, AllowYield, Cursor, OrAbrupt, ParseResult, TokenParser,
        expression::assignment::ExponentiationExpression,
    },
    source::ReadChar,
};
use boa_ast::{
    self as ast, Keyword, Position, Punctuator, Spanned,
    expression::{
        Identifier,
        operator::{
            Binary, BinaryInPrivate,
            binary::{BinaryOp, LogicalOp},
        },
    },
    function::PrivateName,
};
use boa_interner::{Interner, Sym};

pub(super) use self::{assignment::AssignmentExpression, primary::Initializer};
pub(in crate::parser) use {
    fpl_or_exp::FormalParameterListOrExpression,
    identifiers::{BindingIdentifier, LabelIdentifier},
    left_hand_side::LeftHandSideExpression,
    primary::object_initializer::{
        AsyncGeneratorMethod, AsyncMethod, GeneratorMethod, PropertyName,
    },
};

/// Expression parsing.
///
/// More information:
///  - [MDN documentation][mdn]
///  - [ECMAScript specification][spec]
///
/// [mdn]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Operators
/// [spec]: https://tc39.es/ecma262/#prod-Expression
#[derive(Debug, Clone, Copy)]
pub(super) struct Expression {
    allow_in: AllowIn,
    allow_yield: AllowYield,
    allow_await: AllowAwait,
}

impl Expression {
    /// Creates a new `Expression` parser.
    pub(super) fn new<I, Y, A>(allow_in: I, allow_yield: Y, allow_await: A) -> Self
    where
        I: Into<AllowIn>,
        Y: Into<AllowYield>,
        A: Into<AllowAwait>,
    {
        Self {
            allow_in: allow_in.into(),
            allow_yield: allow_yield.into(),
            allow_await: allow_await.into(),
        }
    }
}

impl<R> TokenParser<R> for Expression
where
    R: ReadChar,
{
    type Output = ast::Expression;

    fn parse(self, cursor: &mut Cursor<R>, interner: &mut Interner) -> ParseResult<Self::Output> {
        let mut lhs = AssignmentExpression::new(self.allow_in, self.allow_yield, self.allow_await)
            .parse(cursor, interner)?;
        while let Some(tok) = cursor.peek(0, interner)? {
            match *tok.kind() {
                TokenKind::Punctuator(Punctuator::Comma) => {
                    if cursor.peek(1, interner).or_abrupt()?.kind()
                        == &TokenKind::Punctuator(Punctuator::CloseParen)
                    {
                        return Ok(lhs);
                    }

                    if cursor.peek(1, interner).or_abrupt()?.kind()
                        == &TokenKind::Punctuator(Punctuator::Spread)
                    {
                        return Ok(lhs);
                    }

                    cursor.advance(interner);

                    lhs = Binary::new(
                        Punctuator::Comma
                            .as_binary_op()
                            .expect("Could not get binary operation."),
                        lhs,
                        AssignmentExpression::new(
                            self.allow_in,
                            self.allow_yield,
                            self.allow_await,
                        )
                        .parse(cursor, interner)?,
                    )
                    .into();
                }
                _ => break,
            }
        }

        Ok(lhs)
    }
}

/// Parses a logical expression expression.
///
/// More information:
///  - [MDN documentation][mdn]
///  - [ECMAScript specification][spec]
///
/// [mdn]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Operators/Logical_Operators
/// [spec]: https://tc39.es/ecma262/#prod-ShortCircuitExpression
#[derive(Debug, Clone, Copy)]
struct ShortCircuitExpression {
    allow_in: AllowIn,
    allow_yield: AllowYield,
    allow_await: AllowAwait,
    previous: PreviousExpr,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PreviousExpr {
    None,
    Logical,
    Coalesce,
}

impl ShortCircuitExpression {
    /// Creates a new `ShortCircuitExpression` parser.
    pub(super) fn new<I, Y, A>(allow_in: I, allow_yield: Y, allow_await: A) -> Self
    where
        I: Into<AllowIn>,
        Y: Into<AllowYield>,
        A: Into<AllowAwait>,
    {
        Self {
            allow_in: allow_in.into(),
            allow_yield: allow_yield.into(),
            allow_await: allow_await.into(),
            previous: PreviousExpr::None,
        }
    }

    fn with_previous<I, Y, A>(
        allow_in: I,
        allow_yield: Y,
        allow_await: A,
        previous: PreviousExpr,
    ) -> Self
    where
        I: Into<AllowIn>,
        Y: Into<AllowYield>,
        A: Into<AllowAwait>,
    {
        Self {
            allow_in: allow_in.into(),
            allow_yield: allow_yield.into(),
            allow_await: allow_await.into(),
            previous,
        }
    }
}

impl<R> TokenParser<R> for ShortCircuitExpression
where
    R: ReadChar,
{
    type Output = FormalParameterListOrExpression;

    fn parse(self, cursor: &mut Cursor<R>, interner: &mut Interner) -> ParseResult<Self::Output> {
        let current_node = BinaryExpression::new(self.allow_in, self.allow_yield, self.allow_await)
            .parse(cursor, interner)?;
        let FormalParameterListOrExpression::Expression(mut current_node) = current_node else {
            return Ok(current_node);
        };

        let mut previous = self.previous;

        while let Some(tok) = cursor.peek(0, interner)? {
            match tok.kind() {
                TokenKind::Punctuator(Punctuator::BoolAnd) => {
                    if previous == PreviousExpr::Coalesce {
                        return Err(Error::expected(
                            ["??".to_owned()],
                            tok.to_string(interner),
                            tok.span(),
                            "logical expression (cannot use '??' without parentheses within '||' or '&&')",
                        ));
                    }
                    cursor.advance(interner);
                    previous = PreviousExpr::Logical;
                    let rhs =
                        BinaryExpression::new(self.allow_in, self.allow_yield, self.allow_await)
                            .parse(cursor, interner)?
                            .try_into_expression()?;

                    current_node =
                        Binary::new(BinaryOp::Logical(LogicalOp::And), current_node, rhs).into();
                }
                TokenKind::Punctuator(Punctuator::BoolOr) => {
                    if previous == PreviousExpr::Coalesce {
                        return Err(Error::expected(
                            ["??".to_owned()],
                            tok.to_string(interner),
                            tok.span(),
                            "logical expression (cannot use '??' without parentheses within '||' or '&&')",
                        ));
                    }
                    cursor.advance(interner);
                    previous = PreviousExpr::Logical;
                    let rhs = Self::with_previous(
                        self.allow_in,
                        self.allow_yield,
                        self.allow_await,
                        PreviousExpr::Logical,
                    )
                    .parse(cursor, interner)?
                    .try_into_expression()?;
                    current_node =
                        Binary::new(BinaryOp::Logical(LogicalOp::Or), current_node, rhs).into();
                }
                TokenKind::Punctuator(Punctuator::Coalesce) => {
                    if previous == PreviousExpr::Logical {
                        return Err(Error::expected(
                            ["&&".to_owned(), "||".to_owned()],
                            tok.to_string(interner),
                            tok.span(),
                            "cannot use '??' unparenthesized within '||' or '&&'",
                        ));
                    }
                    cursor.advance(interner);
                    previous = PreviousExpr::Coalesce;
                    let rhs =
                        BinaryExpression::new(self.allow_in, self.allow_yield, self.allow_await)
                            .parse(cursor, interner)?
                            .try_into_expression()?;
                    current_node =
                        Binary::new(BinaryOp::Logical(LogicalOp::Coalesce), current_node, rhs)
                            .into();
                }
                _ => break,
            }
        }
        Ok(current_node.into())
    }
}

/// Parses the binary operators from `|` to `*`, `/` and `%`.
///
/// The grammar has one production per precedence level, from
/// [`BitwiseORExpression`][or] down to [`MultiplicativeExpression`][mul], each a
/// left-associative list of the next level. Parsing them with one function per
/// level costs eight nested calls for every operand; this parses them by
/// precedence climbing instead, which builds the same tree, consumes and looks
/// at the same tokens in the same lexer goals and reports the same errors.
///
/// More information:
///  - [MDN documentation][mdn]
///  - [ECMAScript specification][or]
///
/// [mdn]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Operators
/// [or]: https://tc39.es/ecma262/#prod-BitwiseORExpression
/// [mul]: https://tc39.es/ecma262/#prod-MultiplicativeExpression
#[derive(Debug, Clone, Copy)]
struct BinaryExpression {
    allow_in: AllowIn,
    allow_yield: AllowYield,
    allow_await: AllowAwait,
}

/// The precedence levels of [`BinaryExpression`], from the loosest.
mod precedence {
    /// `BitwiseORExpression`: `|`.
    pub(super) const BITWISE_OR: u8 = 1;
    /// `BitwiseXORExpression`: `^`.
    pub(super) const BITWISE_XOR: u8 = 2;
    /// `BitwiseANDExpression`: `&`.
    pub(super) const BITWISE_AND: u8 = 3;
    /// `EqualityExpression`: `==`, `!=`, `===`, `!==`.
    pub(super) const EQUALITY: u8 = 4;
    /// `RelationalExpression`: `<`, `>`, `<=`, `>=`, `instanceof`, `in`.
    pub(super) const RELATIONAL: u8 = 5;
    /// `ShiftExpression`: `<<`, `>>`, `>>>`.
    pub(super) const SHIFT: u8 = 6;
    /// `AdditiveExpression`: `+`, `-`.
    pub(super) const ADDITIVE: u8 = 7;
    /// `MultiplicativeExpression`: `*`, `/`, `%`.
    pub(super) const MULTIPLICATIVE: u8 = 8;
}

/// The precedence of a binary operator punctuator, if it is one that
/// [`BinaryExpression`] parses.
const fn punctuator_precedence(punctuator: Punctuator) -> Option<u8> {
    Some(match punctuator {
        Punctuator::Or => precedence::BITWISE_OR,
        Punctuator::Xor => precedence::BITWISE_XOR,
        Punctuator::And => precedence::BITWISE_AND,
        Punctuator::Eq | Punctuator::NotEq | Punctuator::StrictEq | Punctuator::StrictNotEq => {
            precedence::EQUALITY
        }
        Punctuator::LessThan
        | Punctuator::GreaterThan
        | Punctuator::LessThanOrEq
        | Punctuator::GreaterThanOrEq => precedence::RELATIONAL,
        Punctuator::LeftSh | Punctuator::RightSh | Punctuator::URightSh => precedence::SHIFT,
        Punctuator::Add | Punctuator::Sub => precedence::ADDITIVE,
        Punctuator::Mul | Punctuator::Div | Punctuator::Mod => precedence::MULTIPLICATIVE,
        _ => return None,
    })
}

impl BinaryExpression {
    /// Creates a new `BinaryExpression` parser.
    fn new<I, Y, A>(allow_in: I, allow_yield: Y, allow_await: A) -> Self
    where
        I: Into<AllowIn>,
        Y: Into<AllowYield>,
        A: Into<AllowAwait>,
    {
        Self {
            allow_in: allow_in.into(),
            allow_yield: allow_yield.into(),
            allow_await: allow_await.into(),
        }
    }

    /// Parses an expression of precedence level `min` (one of [`precedence`]): the
    /// production of that level, such as a `ShiftExpression` for
    /// [`precedence::SHIFT`].
    fn parse_level<R>(
        self,
        min: u8,
        cursor: &mut Cursor<R>,
        interner: &mut Interner,
    ) -> ParseResult<FormalParameterListOrExpression>
    where
        R: ReadChar,
    {
        // The tightest operator that may continue the expression.
        let mut max = precedence::MULTIPLICATIVE;

        let mut lhs: ast::Expression = 'lhs: {
            // A `RelationalExpression` can be `PrivateIdentifier in ShiftExpression`.
            // It is not continued by relational or tighter operators.
            if min <= precedence::RELATIONAL && self.allow_in.0 {
                let token = cursor.peek(0, interner).or_abrupt()?;
                if let TokenKind::PrivateIdentifier(identifier) = token.kind() {
                    let identifier = *identifier;
                    let identifier_span = token.span();
                    let token = cursor.peek(1, interner).or_abrupt()?;
                    match token.kind() {
                        TokenKind::Keyword((Keyword::In, true)) => {
                            return Err(Error::general(
                                "Keyword must not contain escaped characters",
                                token.span().start(),
                            ));
                        }
                        TokenKind::Keyword((Keyword::In, false)) => {
                            cursor.advance(interner);
                            cursor.advance(interner);

                            let rhs = self
                                .parse_level(precedence::SHIFT, cursor, interner)?
                                .try_into_expression()?;

                            max = precedence::EQUALITY;
                            break 'lhs BinaryInPrivate::new(
                                PrivateName::new(identifier, identifier_span),
                                rhs,
                            )
                            .into();
                        }
                        _ => {}
                    }
                }
            }

            // A `MultiplicativeExpression` starts in the `Div` goal.
            cursor.set_goal(InputElement::Div);
            let lhs = ExponentiationExpression::new(self.allow_yield, self.allow_await)
                .parse(cursor, interner)?;
            let FormalParameterListOrExpression::Expression(lhs) = lhs else {
                return Ok(lhs);
            };
            lhs
        };

        while let Some(tok) = cursor.peek(0, interner)? {
            let (op, op_precedence) = match *tok.kind() {
                TokenKind::Punctuator(punctuator) => match punctuator_precedence(punctuator) {
                    Some(op_precedence) => (
                        punctuator
                            .as_binary_op()
                            .expect("Could not get binary operation."),
                        op_precedence,
                    ),
                    None => break,
                },
                TokenKind::Keyword((Keyword::InstanceOf | Keyword::In, true))
                    if min <= precedence::RELATIONAL && max >= precedence::RELATIONAL =>
                {
                    return Err(Error::general(
                        "Keyword must not contain escaped characters",
                        tok.span().start(),
                    ));
                }
                TokenKind::Keyword((op, false))
                    if op == Keyword::InstanceOf
                        || (op == Keyword::In && self.allow_in == AllowIn(true)) =>
                {
                    (
                        op.as_binary_op().expect("Could not get binary operation."),
                        precedence::RELATIONAL,
                    )
                }
                _ => break,
            };
            if op_precedence < min || op_precedence > max {
                break;
            }
            cursor.advance(interner);

            let rhs = if op_precedence == precedence::MULTIPLICATIVE {
                // The right operand of a `MultiplicativeExpression`.
                ExponentiationExpression::new(self.allow_yield, self.allow_await)
                    .parse(cursor, interner)?
            } else {
                self.parse_level(op_precedence + 1, cursor, interner)?
            }
            .try_into_expression()?;
            lhs = Binary::new(op, lhs, rhs).into();
        }

        Ok(lhs.into())
    }
}

impl<R> TokenParser<R> for BinaryExpression
where
    R: ReadChar,
{
    type Output = FormalParameterListOrExpression;

    /// Parses a `BitwiseORExpression`.
    fn parse(self, cursor: &mut Cursor<R>, interner: &mut Interner) -> ParseResult<Self::Output> {
        self.parse_level(precedence::BITWISE_OR, cursor, interner)
    }
}

/// Returns an error if `arguments` or `eval` are used as identifier in strict mode.
fn check_strict_arguments_or_eval(ident: Identifier, position: Position) -> ParseResult<()> {
    match ident.sym() {
        Sym::ARGUMENTS => Err(Error::general(
            "unexpected identifier `arguments` in strict mode",
            position,
        )),
        Sym::EVAL => Err(Error::general(
            "unexpected identifier `eval` in strict mode",
            position,
        )),
        _ => Ok(()),
    }
}
