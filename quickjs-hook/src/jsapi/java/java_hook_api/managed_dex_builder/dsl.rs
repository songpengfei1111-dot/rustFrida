use std::collections::BTreeMap;

use super::{build_method_sig, build_params_sig, java_class_to_descriptor_or_primitive, IfCmpOp};

mod ast;
pub(super) use ast::*;
mod lexer;
use lexer::{lex as dsl_lex, Token as DslToken, TokenKind as DslTokenKind};

mod expression;
mod statement;

pub(super) fn parse_managed_dsl(dsl: &str) -> Result<DslProgram, String> {
    let mut parser = DslParser::new(dsl)?;
    let stmts = parser.parse_statements(false)?;
    parser.skip_ws();
    parser.expect_eof()?;
    Ok(DslProgram { stmts })
}

struct DslParser<'a> {
    input: &'a str,
    tokens: Vec<DslToken>,
    pos: usize,
    local_scopes: Vec<BTreeMap<String, String>>,
    next_local_id: usize,
}

impl<'a> DslParser<'a> {
    fn new(input: &'a str) -> Result<Self, String> {
        Ok(Self {
            input,
            tokens: dsl_lex(input)?,
            pos: 0,
            local_scopes: vec![BTreeMap::new()],
            next_local_id: 0,
        })
    }

    fn with_local_scope<F, R>(&mut self, f: F) -> Result<R, String>
    where
        F: FnOnce(&mut Self) -> Result<R, String>,
    {
        self.local_scopes.push(BTreeMap::new());
        let result = f(self);
        self.local_scopes.pop();
        result
    }

    fn declare_local(&mut self, source_name: String) -> Result<String, String> {
        let Some(scope) = self.local_scopes.last_mut() else {
            return Err(self.err("internal parser scope error"));
        };
        if scope.contains_key(&source_name) {
            return Err(self.err(&format!("local '{}' is already declared in this scope", source_name)));
        }
        let internal_name = format!("__rf_l{}_{}", self.next_local_id, source_name);
        self.next_local_id += 1;
        scope.insert(source_name, internal_name.clone());
        Ok(internal_name)
    }

    fn resolve_local(&self, source_name: &str) -> Option<String> {
        self.local_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(source_name).cloned())
    }

    fn resolve_local_name_or_source(&self, source_name: String) -> String {
        self.resolve_local(&source_name).unwrap_or(source_name)
    }

    fn scoped_target_name(&self, name: &str) -> Option<DslTarget> {
        match parse_target_name(name) {
            Some(DslTarget::Local(local)) => Some(DslTarget::Local(self.resolve_local(&local).unwrap_or(local))),
            other => other,
        }
    }

    fn skip_ws(&mut self) {}

    fn expect_ident(&mut self, expected: &str) -> Result<(), String> {
        match self.tokens.get(self.pos).map(|token| &token.kind) {
            Some(DslTokenKind::Ident(value)) if value == expected => {
                self.pos += 1;
                Ok(())
            }
            _ => Err(self.err(&format!("expected identifier {}", expected))),
        }
    }

    fn peek_ident(&self, expected: &str) -> bool {
        matches!(self.tokens.get(self.pos).map(|token| &token.kind), Some(DslTokenKind::Ident(value)) if value == expected)
    }

    fn parse_ident(&mut self) -> Result<String, String> {
        match self.tokens.get(self.pos).map(|token| &token.kind) {
            Some(DslTokenKind::Ident(value)) => {
                self.pos += 1;
                Ok(value.clone())
            }
            _ => Err(self.err("expected identifier")),
        }
    }

    fn expect_char(&mut self, expected: char) -> Result<(), String> {
        match self.peek() {
            Some(ch) if ch == expected => {
                self.pos += 1;
                Ok(())
            }
            _ => Err(self.err(&format!("expected '{}'", expected))),
        }
    }

    fn parse_string_arg(&mut self) -> Result<String, String> {
        self.skip_ws();
        let value = self.parse_string()?;
        self.skip_ws();
        Ok(value)
    }

    fn parse_string(&mut self) -> Result<String, String> {
        match self.tokens.get(self.pos).map(|token| &token.kind) {
            Some(DslTokenKind::String(value)) => {
                self.pos += 1;
                Ok(value.clone())
            }
            _ => Err(self.err("expected string")),
        }
    }

    fn parse_type_name(&mut self) -> Result<String, String> {
        self.skip_ws();
        if self.peek_string() {
            return self.parse_string_arg();
        }
        let mut name = self.parse_ident()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('.') => {
                    self.expect_char('.')?;
                    let part = self.parse_ident()?;
                    name.push('.');
                    name.push_str(&part);
                }
                Some('[') => {
                    self.expect_char('[')?;
                    self.expect_char(']')?;
                    name.push_str("[]");
                }
                _ => break,
            }
        }
        self.skip_ws();
        Ok(name)
    }

    fn parse_i16(&mut self) -> Result<i16, String> {
        self.skip_ws();
        let negative = if self.peek() == Some('-') {
            self.pos += 1;
            true
        } else {
            false
        };
        let value_text = match self.tokens.get(self.pos).map(|token| &token.kind) {
            Some(DslTokenKind::Number(value)) => {
                self.pos += 1;
                value.clone()
            }
            _ => return Err(self.err("expected integer")),
        };
        let value: i32 = value_text.parse().map_err(|_| self.err("invalid integer"))?;
        let signed = if negative { -value } else { value };
        if signed < i16::MIN as i32 || signed > i16::MAX as i32 {
            return Err(self.err("integer must fit int16"));
        }
        self.skip_ws();
        Ok(signed as i16)
    }

    fn peek_compound_assign_op(&self) -> Option<DslIntBinOp> {
        if self.peek_op(">>>=") {
            return Some(DslIntBinOp::Ushr);
        }
        if self.peek_op("<<=") {
            return Some(DslIntBinOp::Shl);
        }
        if self.peek_op(">>=") {
            return Some(DslIntBinOp::Shr);
        }
        if self.peek_op("+=") {
            return Some(DslIntBinOp::Add);
        }
        if self.peek_op("-=") {
            return Some(DslIntBinOp::Sub);
        }
        if self.peek_op("*=") {
            return Some(DslIntBinOp::Mul);
        }
        if self.peek_op("/=") {
            return Some(DslIntBinOp::Div);
        }
        if self.peek_op("%=") {
            return Some(DslIntBinOp::Rem);
        }
        if self.peek_op("&=") {
            return Some(DslIntBinOp::And);
        }
        if self.peek_op("|=") {
            return Some(DslIntBinOp::Or);
        }
        if self.peek_op("^=") {
            return Some(DslIntBinOp::Xor);
        }
        None
    }

    fn consume_compound_assign_op(&mut self, op: DslIntBinOp) -> Result<(), String> {
        match op {
            DslIntBinOp::Ushr => self.expect_op(">>>="),
            DslIntBinOp::Shl => self.expect_op("<<="),
            DslIntBinOp::Shr => self.expect_op(">>="),
            DslIntBinOp::Add => self.expect_op("+="),
            DslIntBinOp::Sub => self.expect_op("-="),
            DslIntBinOp::Mul => self.expect_op("*="),
            DslIntBinOp::Div => self.expect_op("/="),
            DslIntBinOp::Rem => self.expect_op("%="),
            DslIntBinOp::And => self.expect_op("&="),
            DslIntBinOp::Or => self.expect_op("|="),
            DslIntBinOp::Xor => self.expect_op("^="),
        }
    }

    fn local_increment_stmt(&self, name: String, delta: i16) -> DslStmt {
        let op = if delta >= 0 { DslIntBinOp::Add } else { DslIntBinOp::Sub };
        self.local_compound_assign_stmt(name, op, DslValue::Int(delta.abs()))
    }

    fn local_compound_assign_stmt(&self, name: String, op: DslIntBinOp, rhs: DslValue) -> DslStmt {
        let left = DslValue::Target(DslTarget::Local(name.clone()));
        DslStmt::Assign {
            name,
            value: fold_int_binop(op, left, rhs),
        }
    }

    fn increment_value_stmt(&self, value: DslValue, delta: i16) -> Result<DslStmt, String> {
        let op = if delta >= 0 { DslIntBinOp::Add } else { DslIntBinOp::Sub };
        self.compound_assign_value_stmt(value, op, DslValue::Int(delta.abs()))
    }

    fn compound_assign_value_stmt(&self, value: DslValue, op: DslIntBinOp, rhs: DslValue) -> Result<DslStmt, String> {
        match value {
            DslValue::FieldGet { stmt, is_static } => Ok(DslStmt::FieldUpdate {
                stmt: *stmt,
                is_static,
                op,
                value: rhs,
            }),
            DslValue::ArrayGet {
                array,
                index,
                type_name,
            } => Ok(DslStmt::ArrayUpdate {
                array: *array,
                index: *index,
                type_name,
                op,
                value: rhs,
            }),
            DslValue::Target(DslTarget::Local(name)) => Ok(self.local_compound_assign_stmt(name, op, rhs)),
            _ => Err(self.err("compound assignment supports locals, fields, and array elements")),
        }
    }

    fn expect_eof(&self) -> Result<(), String> {
        if self.pos == self.tokens.len() {
            Ok(())
        } else {
            Err(self.err("unexpected trailing input"))
        }
    }

    fn peek(&self) -> Option<char> {
        match self.tokens.get(self.pos).map(|token| &token.kind) {
            Some(DslTokenKind::Symbol(ch)) => Some(*ch),
            _ => None,
        }
    }

    fn peek_string(&self) -> bool {
        matches!(
            self.tokens.get(self.pos).map(|token| &token.kind),
            Some(DslTokenKind::String(_))
        )
    }

    fn peek_number(&self) -> bool {
        matches!(
            self.tokens.get(self.pos).map(|token| &token.kind),
            Some(DslTokenKind::Number(_))
        )
    }

    fn peek_op(&self, expected: &str) -> bool {
        matches!(self.tokens.get(self.pos).map(|token| &token.kind), Some(DslTokenKind::Op(value)) if *value == expected)
    }

    fn expect_op(&mut self, expected: &str) -> Result<(), String> {
        if self.peek_op(expected) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected operator {}", expected)))
        }
    }

    fn is_eof(&self) -> bool {
        self.pos == self.tokens.len()
    }

    fn err(&self, msg: &str) -> String {
        let byte = self
            .tokens
            .get(self.pos)
            .map(|token| token.byte)
            .unwrap_or_else(|| self.input.len());
        format!("managed dex DSL parse error at byte {}: {}", byte, msg)
    }
}

fn parse_target_name(name: &str) -> Option<DslTarget> {
    match name {
        "this" | "$this" => Some(DslTarget::This),
        "last" | "$last" => Some(DslTarget::Last),
        "result" | "$result" => Some(DslTarget::Result),
        value if value.starts_with("arg") => value[3..].parse::<usize>().ok().map(DslTarget::Arg),
        value if value.starts_with('$') => value[1..].parse::<usize>().ok().map(DslTarget::Arg),
        value if value.starts_with('p') => value[1..].parse::<usize>().ok().map(DslTarget::Arg),
        value if is_local_ident(value) => Some(DslTarget::Local(value.to_string())),
        _ => None,
    }
}

fn looks_like_type_name(value: &str) -> bool {
    matches!(
        value,
        "boolean" | "byte" | "char" | "short" | "int" | "long" | "float" | "double" | "void"
    ) || matches!(value, "Z" | "B" | "C" | "S" | "I" | "J" | "F" | "D" | "V")
        || value.starts_with('[')
        || (value.starts_with('L') && value.ends_with(';'))
        || value.ends_with("[]")
        || value.contains('.')
        || value.contains('/')
}

fn looks_like_static_class_name(value: &str) -> bool {
    value.chars().next().map(|ch| ch.is_ascii_uppercase()).unwrap_or(false)
}

fn fold_unary_op(op: DslUnaryOp, value: DslValue) -> DslValue {
    match (op, value) {
        (DslUnaryOp::Neg, DslValue::Int(value)) => {
            value
                .checked_neg()
                .map(DslValue::Int)
                .unwrap_or_else(|| DslValue::UnaryOp {
                    op,
                    value: Box::new(DslValue::Int(value)),
                })
        }
        (DslUnaryOp::BitNot, DslValue::Int(value)) => DslValue::Int(!value),
        (DslUnaryOp::BoolNot, DslValue::Bool(value)) => DslValue::Bool(!value),
        (op, value) => DslValue::UnaryOp {
            op,
            value: Box::new(value),
        },
    }
}

fn fold_int_binop(op: DslIntBinOp, left: DslValue, right: DslValue) -> DslValue {
    let (DslValue::Int(left_value), DslValue::Int(right_value)) = (&left, &right) else {
        return simplify_int_binop(op, left, right);
    };
    let Some(folded) = eval_const_int_binop(op, *left_value as i32, *right_value as i32) else {
        return simplify_int_binop(op, left, right);
    };
    if folded < i16::MIN as i32 || folded > i16::MAX as i32 {
        return simplify_int_binop(op, left, right);
    }
    DslValue::Int(folded as i16)
}

fn simplify_int_binop(op: DslIntBinOp, left: DslValue, right: DslValue) -> DslValue {
    let left_int = value_int_literal(&left);
    let right_int = value_int_literal(&right);
    match op {
        DslIntBinOp::Add => {
            if right_int == Some(0) {
                return left;
            }
            if left_int == Some(0) {
                return right;
            }
        }
        DslIntBinOp::Sub => {
            if right_int == Some(0) {
                return left;
            }
            if left_int == Some(0) {
                return fold_unary_op(DslUnaryOp::Neg, right);
            }
        }
        DslIntBinOp::Mul => {
            if right_int == Some(1) {
                return left;
            }
            if left_int == Some(1) {
                return right;
            }
        }
        DslIntBinOp::Div => {
            if right_int == Some(1) {
                return left;
            }
        }
        DslIntBinOp::And => {
            if right_int == Some(-1) {
                return left;
            }
            if left_int == Some(-1) {
                return right;
            }
        }
        DslIntBinOp::Or | DslIntBinOp::Xor => {
            if right_int == Some(0) {
                return left;
            }
            if left_int == Some(0) {
                return right;
            }
        }
        DslIntBinOp::Shl | DslIntBinOp::Shr | DslIntBinOp::Ushr => {
            if right_int == Some(0) {
                return left;
            }
        }
        DslIntBinOp::Rem => {}
    }
    DslValue::IntBinOp {
        op,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn value_int_literal(value: &DslValue) -> Option<i16> {
    let DslValue::Int(value) = value else {
        return None;
    };
    Some(*value)
}

fn eval_const_int_binop(op: DslIntBinOp, left: i32, right: i32) -> Option<i32> {
    let value = match op {
        DslIntBinOp::Add => left.wrapping_add(right),
        DslIntBinOp::Sub => left.wrapping_sub(right),
        DslIntBinOp::Mul => left.wrapping_mul(right),
        DslIntBinOp::Div => {
            if right == 0 {
                return None;
            }
            left.wrapping_div(right)
        }
        DslIntBinOp::Rem => {
            if right == 0 {
                return None;
            }
            left.wrapping_rem(right)
        }
        DslIntBinOp::And => left & right,
        DslIntBinOp::Or => left | right,
        DslIntBinOp::Xor => left ^ right,
        DslIntBinOp::Shl => left.wrapping_shl((right & 0x1f) as u32),
        DslIntBinOp::Shr => left.wrapping_shr((right & 0x1f) as u32),
        DslIntBinOp::Ushr => ((left as u32).wrapping_shr((right & 0x1f) as u32)) as i32,
    };
    Some(value)
}

fn is_local_ident(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first == '$' {
        return false;
    }
    first == '_' || first.is_ascii_alphabetic()
}
