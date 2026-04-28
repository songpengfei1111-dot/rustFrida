use super::*;

impl<'a> DslParser<'a> {
    pub(super) fn parse_js_while_statement(&mut self) -> Result<DslStmt, String> {
        self.expect_ident("while")?;
        self.skip_ws();
        self.expect_char('(')?;
        let condition = self.parse_js_if_condition()?;
        self.expect_char(')')?;
        let body_stmts = self.parse_statement_body()?;
        Ok(DslStmt::While { condition, body_stmts })
    }

    pub(super) fn parse_js_do_while_statement(&mut self) -> Result<DslStmt, String> {
        self.expect_ident("do")?;
        let body_stmts = self.parse_statement_body()?;
        self.skip_ws();
        self.expect_ident("while")?;
        self.skip_ws();
        self.expect_char('(')?;
        let condition = self.parse_js_if_condition()?;
        self.expect_char(')')?;
        self.skip_ws();
        self.expect_char(';')?;
        Ok(DslStmt::DoWhile { body_stmts, condition })
    }

    pub(super) fn parse_js_for_statement(&mut self) -> Result<DslStmt, String> {
        self.expect_ident("for")?;
        self.with_local_scope(|parser| parser.parse_js_for_statement_scoped())
    }

    fn parse_js_for_statement_scoped(&mut self) -> Result<DslStmt, String> {
        self.skip_ws();
        self.expect_char('(')?;
        let init_stmts = if self.peek() == Some(';') {
            self.expect_char(';')?;
            Vec::new()
        } else if self.peek_ident("let") {
            self.expect_ident("let")?;
            self.parse_js_let_declarations_until(';')?
        } else {
            self.parse_for_header_statement_list(';', false)?
        };
        self.skip_ws();
        let condition = if self.peek() == Some(';') {
            None
        } else {
            Some(self.parse_js_if_condition()?)
        };
        self.expect_char(';')?;
        self.skip_ws();
        let update_stmts = if self.peek() == Some(')') {
            self.expect_char(')')?;
            Vec::new()
        } else {
            self.parse_for_header_statement_list(')', false)?
        };
        let body_stmts = self.parse_statement_body()?;
        Ok(DslStmt::For {
            init_stmts,
            condition,
            update_stmts,
            body_stmts,
        })
    }

    fn parse_for_header_statement_list(&mut self, terminator: char, allow_let: bool) -> Result<Vec<DslStmt>, String> {
        let mut stmts = Vec::new();
        loop {
            if self.peek_ident("let") {
                if !allow_let {
                    return Err(self.err("let declarations are only supported in for init"));
                }
                self.expect_ident("let")?;
                stmts.extend(self.parse_js_let_declarations_until(terminator)?);
                break;
            }
            stmts.push(self.parse_for_header_statement()?);
            self.skip_ws();
            if self.peek() == Some(',') {
                self.expect_char(',')?;
                continue;
            }
            self.expect_char(terminator)?;
            break;
        }
        Ok(stmts)
    }

    fn parse_for_header_statement(&mut self) -> Result<DslStmt, String> {
        self.skip_ws();
        if self.peek_op("++") || self.peek_op("--") {
            let delta = if self.peek_op("++") {
                self.expect_op("++")?;
                1
            } else {
                self.expect_op("--")?;
                -1
            };
            let name = self.parse_ident()?;
            let name = self.resolve_local_name_or_source(name);
            self.skip_ws();
            return Ok(self.local_increment_stmt(name, delta));
        }
        let name = self.parse_ident()?;
        self.skip_ws();
        let stmt = if self.peek() == Some('=') {
            self.expect_char('=')?;
            let value = self.parse_value_arg()?;
            let name = self.resolve_local_name_or_source(name);
            DslStmt::Assign { name, value }
        } else if let Some(op) = self.peek_compound_assign_op() {
            self.consume_compound_assign_op(op)?;
            let rhs = self.parse_value_arg()?;
            let name = self.resolve_local_name_or_source(name);
            self.local_compound_assign_stmt(name, op, rhs)
        } else if self.peek_op("++") || self.peek_op("--") {
            let delta = if self.peek_op("++") {
                self.expect_op("++")?;
                1
            } else {
                self.expect_op("--")?;
                -1
            };
            let name = self.resolve_local_name_or_source(name);
            self.local_increment_stmt(name, delta)
        } else if self.peek() == Some('.') || self.peek() == Some('[') || self.peek_ident("as") {
            let value = self.parse_value_from_ident(name)?;
            self.skip_ws();
            if self.peek() == Some('=') {
                self.expect_char('=')?;
                let rhs = self.parse_value_arg()?;
                match value {
                    DslValue::FieldGet { stmt, is_static } => {
                        let mut stmt = *stmt;
                        stmt.value = Some(rhs);
                        DslStmt::FieldWrite { stmt, is_static }
                    }
                    DslValue::ArrayGet {
                        array,
                        index,
                        type_name,
                    } => DslStmt::ArrayPut {
                        array: *array,
                        index: *index,
                        type_name,
                        value: rhs,
                    },
                    _ => return Err(self.err("only fields and array elements can be assigned")),
                }
            } else if let Some(op) = self.peek_compound_assign_op() {
                self.consume_compound_assign_op(op)?;
                let rhs = self.parse_value_arg()?;
                self.compound_assign_value_stmt(value, op, rhs)?
            } else if self.peek_op("++") || self.peek_op("--") {
                let delta = if self.peek_op("++") {
                    self.expect_op("++")?;
                    1
                } else {
                    self.expect_op("--")?;
                    -1
                };
                self.increment_value_stmt(value, delta)?
            } else {
                value
                    .into_statement()
                    .ok_or_else(|| self.err("only method calls and field reads can be used in for update"))?
            }
        } else {
            return Err(self.err(&format!("unsupported for header statement '{}'", name)));
        };
        self.skip_ws();
        Ok(stmt)
    }
}
