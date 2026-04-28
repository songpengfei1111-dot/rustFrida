use super::*;

impl<'a> DslParser<'a> {
    pub(super) fn parse_postfix_member_value(
        &mut self,
        receiver: DslValue,
        null_safe: bool,
    ) -> Result<DslValue, String> {
        if null_safe {
            self.expect_op("?.")?;
        } else {
            self.expect_char('.')?;
        }
        let member_name = self.parse_ident()?;
        self.skip_ws();

        if member_name == "length" && self.peek() != Some('(') && self.peek() != Some('.') {
            return Ok(DslValue::ArrayLength(Box::new(receiver)));
        }
        if member_name == "$new" {
            return Err(self.err("$new is only supported on class names"));
        }

        let call_kind = if self.peek() == Some('.') {
            let checkpoint = self.mark();
            self.expect_char('.')?;
            if self.peek_ident("interface") {
                self.expect_ident("interface")?;
                self.skip_ws();
                DslCallKind::Interface
            } else {
                self.restore(checkpoint);
                DslCallKind::Virtual
            }
        } else {
            DslCallKind::Virtual
        };

        if self.peek() == Some('.') {
            self.expect_char('.')?;
            self.expect_ident("overload")?;
            return self.parse_postfix_overload_call(receiver, member_name, call_kind, null_safe);
        }

        if self.peek() != Some('(') {
            return self.build_receiver_member_field(receiver, member_name, call_kind, String::new());
        }

        self.parse_receiver_member_call(receiver, null_safe, member_name, call_kind)
    }

    pub(super) fn parse_js_member_value(&mut self, first: String) -> Result<DslValue, String> {
        let mut parts = vec![first];
        while self.peek() == Some('.') {
            self.expect_char('.')?;
            parts.push(self.parse_ident()?);
            self.skip_ws();
            if parts.last().map(|part| part.as_str()) == Some("overload") {
                return self.parse_js_overload_member_value(parts);
            }
        }
        if parts.len() < 2 {
            return Err(self.err("expected member access"));
        }
        if parts.last().map(|part| part.as_str()) == Some("$new") {
            return self.parse_js_new_member_value(parts);
        }
        if parts.len() == 2 && parts[1] == "length" && self.peek() != Some('(') {
            let target = self
                .scoped_target_name(&parts[0])
                .unwrap_or_else(|| DslTarget::Local(parts[0].clone()));
            return Ok(DslValue::ArrayLength(Box::new(DslValue::Target(target))));
        }
        if self.peek() != Some('(') {
            if parts.len() == 2 && !looks_like_static_class_name(&parts[0]) {
                let target = self
                    .scoped_target_name(&parts[0])
                    .unwrap_or_else(|| DslTarget::Local(parts[0].clone()));
                return Ok(self.build_target_field_get(target, parts[1].clone(), None, String::new()));
            }
            return Err(
                self.err("direct field access currently supports only instance fields on this/arg/local values")
            );
        }
        self.expect_char('(')?;

        if parts.len() == 2 && self.scoped_target_name(&parts[0]).is_some() {
            let target = self.scoped_target_name(&parts[0]).unwrap();
            self.parse_target_member_call(target, parts[1].clone())
        } else {
            let member_name = parts.pop().unwrap();
            let class_name = parts.join(".");
            self.parse_static_member_call(class_name, member_name)
        }
    }
}
