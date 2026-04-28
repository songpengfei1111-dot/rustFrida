use super::*;

impl<'a> DslParser<'a> {
    pub(super) fn parse_js_if_statement(&mut self) -> Result<DslStmt, String> {
        self.expect_ident("if")?;
        self.skip_ws();
        self.expect_char('(')?;
        let condition = self.parse_js_if_condition()?;
        self.expect_char(')')?;
        let then_stmts = self.parse_statement_body()?;
        self.skip_ws();
        let else_stmts = if self.peek_ident("else") {
            self.expect_ident("else")?;
            self.skip_ws();
            if self.peek_ident("if") {
                vec![self.parse_js_if_statement()?]
            } else {
                self.parse_statement_body()?
            }
        } else {
            Vec::new()
        };
        Ok(condition.into_if_stmt(then_stmts, else_stmts))
    }

    pub(super) fn parse_js_switch_statement(&mut self) -> Result<DslStmt, String> {
        self.expect_ident("switch")?;
        self.skip_ws();
        self.expect_char('(')?;
        let value = self.parse_value_arg()?;
        self.expect_char(')')?;
        self.skip_ws();
        self.expect_char('{')?;

        let mut cases = Vec::<(i16, Vec<DslStmt>)>::new();
        let mut default_stmts = None::<Vec<DslStmt>>;
        loop {
            self.skip_ws();
            if self.peek() == Some('}') {
                self.expect_char('}')?;
                break;
            }
            if self.peek_ident("case") {
                self.expect_ident("case")?;
                let literal = self.parse_i16()?;
                self.expect_char(':')?;
                let stmts = self.parse_block()?;
                cases.push((literal, stmts));
            } else if self.peek_ident("default") {
                if default_stmts.is_some() {
                    return Err(self.err("switch supports only one default block"));
                }
                self.expect_ident("default")?;
                self.skip_ws();
                self.expect_char(':')?;
                default_stmts = Some(self.parse_block()?);
            } else {
                return Err(self.err("expected switch case/default block"));
            }
        }
        if cases.is_empty() {
            return Err(self.err("switch requires at least one case"));
        }

        Ok(DslStmt::Switch {
            value,
            cases,
            default_stmts,
        })
    }

    pub(super) fn parse_js_try_catch_statement(&mut self) -> Result<DslStmt, String> {
        self.expect_ident("try")?;
        let try_stmts = self.parse_block()?;
        let mut catches = Vec::new();
        loop {
            self.skip_ws();
            if !self.peek_ident("catch") {
                break;
            }
            self.expect_ident("catch")?;
            self.skip_ws();
            self.expect_char('(')?;
            let (catch_type, catch_name) = self.parse_catch_param()?;
            self.skip_ws();
            self.expect_char(')')?;
            let (catch_name, catch_stmts) = self.with_local_scope(|parser| {
                let catch_name = parser.declare_local(catch_name)?;
                let catch_stmts = parser.parse_block()?;
                Ok((catch_name, catch_stmts))
            })?;
            catches.push(DslCatch {
                catch_type,
                catch_name,
                catch_stmts,
            });
        }
        if catches.is_empty() {
            return Err(self.err("try requires at least one catch block"));
        }
        Ok(DslStmt::TryCatch { try_stmts, catches })
    }

    fn parse_catch_param(&mut self) -> Result<(String, String), String> {
        self.skip_ws();
        let checkpoint = self.mark();
        if let Ok(catch_name) = self.parse_ident() {
            self.skip_ws();
            if self.peek() == Some(')') {
                return Ok(("java.lang.Throwable".to_string(), catch_name));
            }
        }
        self.restore(checkpoint);
        let catch_type = self.parse_type_name()?;
        self.skip_ws();
        let catch_name = self.parse_ident()?;
        Ok((catch_type, catch_name))
    }
}
