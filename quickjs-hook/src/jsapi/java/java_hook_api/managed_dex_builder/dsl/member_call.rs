use super::*;

impl<'a> DslParser<'a> {
    pub(super) fn build_receiver_member_field(
        &self,
        receiver: DslValue,
        field_name: String,
        call_kind: DslCallKind,
        type_name: String,
    ) -> Result<DslValue, String> {
        if call_kind == DslCallKind::Interface {
            return Err(self.err("interface field access is not supported"));
        }
        Ok(DslValue::FieldGet {
            stmt: Box::new(DslFieldStmt {
                target: None,
                receiver: Some(Box::new(receiver)),
                class_name: None,
                field_name,
                type_name,
                value: None,
            }),
            is_static: false,
        })
    }

    pub(super) fn parse_receiver_member_call(
        &mut self,
        receiver: DslValue,
        null_safe: bool,
        method_name: String,
        call_kind: DslCallKind,
    ) -> Result<DslValue, String> {
        self.expect_char('(')?;
        match self.parse_member_call_args(true, false)? {
            ParsedCallArgs::Direct(args) => Ok(DslValue::Call(DslCallStmt {
                kind: call_kind,
                target: None,
                receiver: Some(Box::new(receiver)),
                null_safe,
                class_name: None,
                method_name,
                sig: String::new(),
                args,
            })),
            ParsedCallArgs::LegacyCall { class_name, sig, args } => Ok(DslValue::Call(DslCallStmt {
                kind: call_kind,
                target: None,
                receiver: Some(Box::new(receiver)),
                null_safe,
                class_name,
                method_name,
                sig,
                args,
            })),
            ParsedCallArgs::Field { type_name, .. } => {
                self.build_receiver_member_field(receiver, method_name, call_kind, type_name)
            }
        }
    }

    pub(super) fn parse_target_member_call(
        &mut self,
        target: DslTarget,
        method_name: String,
    ) -> Result<DslValue, String> {
        let allow_explicit_class = matches!(target, DslTarget::Last | DslTarget::Result);
        match self.parse_member_call_args(true, allow_explicit_class)? {
            ParsedCallArgs::Direct(args) => Ok(DslValue::Call(DslCallStmt {
                kind: DslCallKind::Virtual,
                target: Some(target),
                receiver: None,
                null_safe: false,
                class_name: None,
                method_name,
                sig: String::new(),
                args,
            })),
            ParsedCallArgs::LegacyCall { class_name, sig, args } => Ok(DslValue::Call(DslCallStmt {
                kind: DslCallKind::Virtual,
                target: Some(target),
                receiver: None,
                null_safe: false,
                class_name,
                method_name,
                sig,
                args,
            })),
            ParsedCallArgs::Field { class_name, type_name } => {
                Ok(self.build_target_field_get(target, method_name, class_name, type_name))
            }
        }
    }

    pub(super) fn parse_static_member_call(
        &mut self,
        class_name: String,
        member_name: String,
    ) -> Result<DslValue, String> {
        match self.parse_member_call_args(true, false)? {
            ParsedCallArgs::Direct(args) => Ok(DslValue::Call(DslCallStmt {
                kind: DslCallKind::Static,
                target: None,
                receiver: None,
                null_safe: false,
                class_name: Some(class_name),
                method_name: member_name,
                sig: String::new(),
                args,
            })),
            ParsedCallArgs::LegacyCall { sig, args, .. } => Ok(DslValue::Call(DslCallStmt {
                kind: DslCallKind::Static,
                target: None,
                receiver: None,
                null_safe: false,
                class_name: Some(class_name),
                method_name: member_name,
                sig,
                args,
            })),
            ParsedCallArgs::Field { type_name, .. } => Ok(DslValue::FieldGet {
                stmt: Box::new(DslFieldStmt {
                    target: None,
                    receiver: None,
                    class_name: Some(class_name),
                    field_name: member_name,
                    type_name,
                    value: None,
                }),
                is_static: true,
            }),
        }
    }
    pub(super) fn build_target_field_get(
        &self,
        target: DslTarget,
        field_name: String,
        class_name: Option<String>,
        type_name: String,
    ) -> DslValue {
        DslValue::FieldGet {
            stmt: Box::new(DslFieldStmt {
                target: Some(target),
                receiver: None,
                class_name,
                field_name,
                type_name,
                value: None,
            }),
            is_static: false,
        }
    }
}
