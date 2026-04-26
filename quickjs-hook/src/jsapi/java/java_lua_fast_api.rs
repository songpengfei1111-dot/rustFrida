//! Java.luaFastMethod() backend used by Lua high-frequency callbacks.
//!
//! This is intentionally fast-only: registration rejects methods that do not
//! currently have an independent quick-code entrypoint. Slow/reflection/JNI
//! calls stay in the JS callback path.

use crate::ffi;
use crate::jsapi::callback_util::{
    extract_string_arg, js_u64_to_js_number_or_bigint, set_js_u64_property, throw_internal_error, throw_type_error,
};
use crate::jsapi::console::output_verbose;
use crate::value::JSValue;
use std::cell::Cell;
use std::ffi::CString;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use super::art_method::*;
use super::callback::{get_return_type_from_sig, parse_jni_param_types};
use super::jni_core::*;
use super::reflect::{decode_field_id, decode_method_id, find_class_safe};
use super::safe_mem::{refresh_mem_regions, safe_read_u32};

#[derive(Clone, Debug)]
pub(crate) struct LuaFastMethod {
    pub(crate) art_method: u64,
    method_id: u64,
    class_global_ref: u64,
    class_mirror: u64,
    pub(crate) is_static: bool,
    pub(crate) return_type: u8,
    pub(crate) param_types: Vec<String>,
    shorty: CString,
}

#[derive(Clone, Debug)]
pub(crate) struct LuaFastConstructor {
    pub(crate) class_global_ref: u64,
    pub(crate) class_mirror: u64,
    pub(crate) art_method: u64,
    method_id: u64,
    pub(crate) param_types: Vec<String>,
    shorty: CString,
}

#[derive(Clone, Debug)]
pub(crate) struct LuaFastField {
    #[allow(dead_code)]
    pub(crate) art_field: u64,
    pub(crate) offset: u32,
    pub(crate) is_static: bool,
    pub(crate) value_type: u8,
    #[allow(dead_code)]
    pub(crate) jni_sig: String,
    #[allow(dead_code)]
    pub(crate) class_name: String,
    #[allow(dead_code)]
    pub(crate) field_name: String,
}

#[derive(Clone, Copy)]
pub(crate) enum LuaFastArg {
    Raw(u64),
    JniRef { env: JniEnv, object: *mut std::ffi::c_void },
}

static LUA_FAST_METHODS: OnceLock<Mutex<Vec<LuaFastMethod>>> = OnceLock::new();
static LUA_FAST_CONSTRUCTORS: OnceLock<Mutex<Vec<LuaFastConstructor>>> = OnceLock::new();
static LUA_FAST_FIELDS: OnceLock<Mutex<Vec<LuaFastField>>> = OnceLock::new();
static FAST_ART_EXCEPTION_SEEN: AtomicU64 = AtomicU64::new(0);
static FAST_ART_EXCEPTION_CLEARED: AtomicU64 = AtomicU64::new(0);
static FAST_ART_HANDLE_SCOPE_ENTER: AtomicU64 = AtomicU64::new(0);
static FAST_ART_HANDLE_SCOPE_UNAVAILABLE: AtomicU64 = AtomicU64::new(0);
static FAST_ART_HANDLE_SCOPE_LEAKED: AtomicU64 = AtomicU64::new(0);
static FAST_ART_HANDLE_SCOPE_MAX_ROOTS: AtomicU64 = AtomicU64::new(0);
static FAST_ART_HANDLE_SCOPE_ROOT_FAILED: AtomicU64 = AtomicU64::new(0);
static FAST_ART_HANDLE_SCOPE_CAPACITY_EXCEEDED: AtomicU64 = AtomicU64::new(0);
static QUICK_ENTRYPOINTS_OFFSET: AtomicUsize = AtomicUsize::new(0);
static FAST_TLAB_ALLOC_HIT: AtomicU64 = AtomicU64::new(0);
static FAST_TLAB_ALLOC_MISS: AtomicU64 = AtomicU64::new(0);
static FAST_QUICK_ALLOC_SLOW_PATH: AtomicU64 = AtomicU64::new(0);

const QUICK_ENTRYPOINTS_OFFSET_FAILED: usize = usize::MAX;
const QUICK_ENTRYPOINT_COUNT: usize = 174;
const QUICK_ALLOC_OBJECT_INITIALIZED_INDEX: usize = 6;
const QUICK_JNI_METHOD_START_INDEX: usize = 45;
const QUICK_JNI_METHOD_END_INDEX: usize = 46;
const QUICK_SCAN_LIMIT: usize = 16384;
const QUICK_MIN_LIBART_POINTERS: usize = 40;
const THREAD_CARD_TABLE_OFFSET: usize = 0x90;
const THREAD_EXCEPTION_OFFSET: usize = THREAD_CARD_TABLE_OFFSET + std::mem::size_of::<usize>();
const THREAD_LOCAL_POS_OFFSET: usize = THREAD_CARD_TABLE_OFFSET + 26 * std::mem::size_of::<usize>();
const THREAD_LOCAL_END_OFFSET: usize = THREAD_LOCAL_POS_OFFSET + std::mem::size_of::<usize>();
const MIRROR_OBJECT_CLASS_OFFSET: usize = 0;
const MIRROR_OBJECT_LOCK_WORD_OFFSET: usize = 4;
const MAX_TLAB_FAST_OBJECT_SIZE: u32 = 1 << 20;
const FAST_ART_HANDLE_SCOPE_CAPACITY: usize = 256;
const FAST_ART_STACK_INVOKE_WORDS: usize = 64;

#[repr(C)]
struct FastArtHandleScope {
    link: u64,
    capacity: i32,
    size: u32,
    refs: [u32; FAST_ART_HANDLE_SCOPE_CAPACITY],
}

impl FastArtHandleScope {
    fn new(link: u64) -> Self {
        Self {
            link,
            capacity: FAST_ART_HANDLE_SCOPE_CAPACITY as i32,
            size: 0,
            refs: [0; FAST_ART_HANDLE_SCOPE_CAPACITY],
        }
    }
}

#[inline]
fn update_fast_max(target: &AtomicU64, value: u64) {
    let mut observed = target.load(Ordering::Acquire);
    while value > observed {
        match target.compare_exchange(observed, value, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(v) => observed = v,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct LuaFastArtRoot {
    slot: u32,
}

thread_local! {
    static CURRENT_FAST_ART_HANDLE_SCOPE: Cell<*mut FastArtHandleScope> = const { Cell::new(std::ptr::null_mut()) };
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RequestedCompileKind {
    Auto,
    Fast,
    Baseline,
    Optimized,
}

impl RequestedCompileKind {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "fast" => Some(Self::Fast),
            "baseline" => Some(Self::Baseline),
            "optimized" | "opt" => Some(Self::Optimized),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fast => "fast",
            Self::Baseline => "baseline",
            Self::Optimized => "optimized",
        }
    }

    fn sequence(self) -> &'static [u32] {
        match self {
            // Mirrors ART's JitAtFirstUse behavior: fast first, then baseline.
            Self::Auto => &[1, 2, 3],
            Self::Fast => &[1],
            Self::Baseline => &[2],
            Self::Optimized => &[3],
        }
    }
}

pub(crate) struct CompileResult {
    pub(crate) before: u64,
    pub(crate) after: u64,
    pub(crate) success: bool,
    pub(crate) compiled: bool,
    pub(crate) kind: &'static str,
    pub(crate) message: String,
}

fn lua_fast_methods() -> &'static Mutex<Vec<LuaFastMethod>> {
    LUA_FAST_METHODS.get_or_init(|| Mutex::new(Vec::new()))
}

fn lua_fast_constructors() -> &'static Mutex<Vec<LuaFastConstructor>> {
    LUA_FAST_CONSTRUCTORS.get_or_init(|| Mutex::new(Vec::new()))
}

fn lua_fast_fields() -> &'static Mutex<Vec<LuaFastField>> {
    LUA_FAST_FIELDS.get_or_init(|| Mutex::new(Vec::new()))
}

fn make_shorty(sig: &str) -> CString {
    let return_sig = sig
        .rsplit_once(')')
        .map(|(_, ret)| ret)
        .filter(|ret| !ret.is_empty())
        .unwrap_or("V");
    let mut shorty = Vec::with_capacity(sig.len() + 1);
    shorty.push(shorty_char(return_sig));
    for param in parse_jni_param_types(sig) {
        shorty.push(shorty_char(param.as_str()));
    }
    CString::new(shorty).unwrap_or_else(|_| CString::new("V").unwrap())
}

fn shorty_char(type_sig: &str) -> u8 {
    match type_sig.as_bytes().first().copied().unwrap_or(b'V') {
        b'L' | b'[' => b'L',
        ch => ch,
    }
}

unsafe fn resolve_lua_fast_method(
    env: JniEnv,
    class_name: &str,
    method_name: &str,
    signature: &str,
    force_static: bool,
) -> Result<(u64, u64, u64, bool), String> {
    let c_method = CString::new(method_name).map_err(|_| "invalid method name")?;
    let c_sig = CString::new(signature).map_err(|_| "invalid signature")?;
    let cls = find_class_safe(env, class_name);
    if cls.is_null() {
        jni_check_exc(env);
        return Err(format!("FindClass('{}') failed", class_name));
    }

    let delete_local_ref: DeleteLocalRefFn = jni_fn!(env, DeleteLocalRefFn, JNI_DELETE_LOCAL_REF);
    let delete_global_ref: DeleteGlobalRefFn = jni_fn!(env, DeleteGlobalRefFn, JNI_DELETE_GLOBAL_REF);
    let new_global_ref: NewGlobalRefFn = jni_fn!(env, NewGlobalRefFn, JNI_NEW_GLOBAL_REF);
    let class_global = new_global_ref(env, cls);
    if class_global.is_null() || jni_check_exc(env) {
        delete_local_ref(env, cls);
        return Err(format!("NewGlobalRef failed for {}", class_name));
    }

    if !force_static {
        let get_method_id: GetMethodIdFn = jni_fn!(env, GetMethodIdFn, JNI_GET_METHOD_ID);
        let method_id = get_method_id(env, cls, c_method.as_ptr(), c_sig.as_ptr());
        if !method_id.is_null() && !jni_check_exc(env) {
            let art_method = decode_method_id(env, cls, method_id as u64, false);
            delete_local_ref(env, cls);
            return Ok((art_method, method_id as u64, class_global as u64, false));
        }
        jni_check_exc(env);
    }

    let get_static_method_id: GetStaticMethodIdFn = jni_fn!(env, GetStaticMethodIdFn, JNI_GET_STATIC_METHOD_ID);
    let method_id = get_static_method_id(env, cls, c_method.as_ptr(), c_sig.as_ptr());
    if !method_id.is_null() && !jni_check_exc(env) {
        let art_method = decode_method_id(env, cls, method_id as u64, true);
        delete_local_ref(env, cls);
        return Ok((art_method, method_id as u64, class_global as u64, true));
    }
    jni_check_exc(env);
    delete_local_ref(env, cls);
    delete_global_ref(env, class_global);

    Err(format!("method not found: {}.{}{}", class_name, method_name, signature))
}

pub(crate) fn get_lua_fast_method(handle: u64) -> Option<LuaFastMethod> {
    if handle == 0 {
        return None;
    }
    let methods = lua_fast_methods().lock().unwrap_or_else(|e| e.into_inner());
    methods.get((handle - 1) as usize).cloned()
}

pub(crate) fn get_lua_fast_constructor(handle: u64) -> Option<LuaFastConstructor> {
    if handle == 0 {
        return None;
    }
    let constructors = lua_fast_constructors().lock().unwrap_or_else(|e| e.into_inner());
    constructors.get((handle - 1) as usize).cloned()
}

pub(crate) fn get_lua_fast_field(handle: u64) -> Option<LuaFastField> {
    if handle == 0 {
        return None;
    }
    let fields = lua_fast_fields().lock().unwrap_or_else(|e| e.into_inner());
    fields.get((handle - 1) as usize).cloned()
}

unsafe fn is_lua_fast_field_type(sig: &str) -> bool {
    matches!(
        sig.as_bytes().first().copied(),
        Some(b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' | b'L' | b'[')
    )
}

unsafe fn parse_lua_fast_options(
    ctx: *mut ffi::JSContext,
    argc: i32,
    argv: *mut ffi::JSValue,
    opt_index: i32,
) -> Result<(bool, RequestedCompileKind), ffi::JSValue> {
    if argc <= opt_index {
        return Ok((false, RequestedCompileKind::Auto));
    }
    let opt = JSValue(*argv.add(opt_index as usize));
    if opt.is_bool() {
        return Ok((opt.to_bool().unwrap_or(false), RequestedCompileKind::Auto));
    }
    if opt.is_string() {
        let Some(kind_s) = opt.to_string(ctx) else {
            return Ok((false, RequestedCompileKind::Auto));
        };
        let Some(kind) = RequestedCompileKind::from_str(kind_s.as_str()) else {
            return Err(throw_type_error(ctx, b"invalid compile kind\0"));
        };
        return Ok((true, kind));
    }
    if opt.is_object() {
        let compile_val = opt.get_property(ctx, "compile");
        let should_compile = compile_val.to_bool().unwrap_or(false);
        compile_val.free(ctx);

        let kind_val = opt.get_property(ctx, "kind");
        let kind = if kind_val.is_string() {
            let kind_s = kind_val.to_string(ctx).unwrap_or_else(|| "auto".to_string());
            let Some(kind) = RequestedCompileKind::from_str(kind_s.as_str()) else {
                kind_val.free(ctx);
                return Err(throw_type_error(ctx, b"invalid compile kind\0"));
            };
            kind
        } else {
            RequestedCompileKind::Auto
        };
        kind_val.free(ctx);
        return Ok((should_compile, kind));
    }
    Ok((false, RequestedCompileKind::Auto))
}

pub(crate) unsafe extern "C" fn js_java_lua_fast_method(
    ctx: *mut ffi::JSContext,
    _this: ffi::JSValue,
    argc: i32,
    argv: *mut ffi::JSValue,
) -> ffi::JSValue {
    if argc < 3 {
        return throw_type_error(
            ctx,
            b"luaFastMethod(class, method, sig[, options]) requires at least 3 arguments\0",
        );
    }

    let class_name = match extract_string_arg(ctx, JSValue(*argv), b"arg 0 must be string\0") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let method_name = match extract_string_arg(ctx, JSValue(*argv.add(1)), b"arg 1 must be string\0") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let sig_str = match extract_string_arg(ctx, JSValue(*argv.add(2)), b"arg 2 must be string\0") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let (actual_sig, force_static) = if let Some(stripped) = sig_str.strip_prefix("static:") {
        (stripped.to_string(), true)
    } else {
        (sig_str, false)
    };

    let env = match ensure_jni_initialized() {
        Ok(e) => e,
        Err(msg) => return throw_internal_error(ctx, msg),
    };

    let (art_method, method_id, class_global_ref, is_static) =
        match resolve_lua_fast_method(env, &class_name, &method_name, &actual_sig, force_static) {
            Ok(v) => v,
            Err(msg) => return throw_internal_error(ctx, msg),
        };

    let (should_compile, compile_kind) = match parse_lua_fast_options(ctx, argc, argv, 3) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let spec = get_art_method_spec(env, art_method);
    let bridge = find_art_bridge_functions(env, spec.entry_point_offset);
    let mut entry_point = read_entry_point(art_method, spec.entry_point_offset);
    if is_art_quick_entrypoint(entry_point, &bridge) && should_compile {
        let compile = compile_art_method_to_quick(env, art_method, spec.entry_point_offset, bridge, compile_kind);
        entry_point = compile.after;
        crate::jsapi::console::output_verbose(&format!(
            "[luaFastMethod] compile {}.{}{} kind={} success={} before={:#x} after={:#x} msg={}",
            class_name,
            method_name,
            actual_sig,
            compile.kind,
            compile.success,
            compile.before,
            compile.after,
            compile.message
        ));
    }
    if is_art_quick_entrypoint(entry_point, &bridge) {
        return throw_internal_error(
            ctx,
            format!(
                "luaFastMethod rejected {}.{}{}: no independent quick entrypoint (entry={:#x})",
                class_name, method_name, actual_sig, entry_point
            ),
        );
    }

    let method = LuaFastMethod {
        art_method,
        method_id,
        class_global_ref,
        class_mirror: super::decode_global_jobject_raw(env, class_global_ref as *mut std::ffi::c_void).unwrap_or(0),
        is_static,
        return_type: get_return_type_from_sig(&actual_sig),
        param_types: parse_jni_param_types(&actual_sig),
        shorty: make_shorty(&actual_sig),
    };
    let mut methods = lua_fast_methods().lock().unwrap_or_else(|e| e.into_inner());
    methods.push(method);
    js_u64_to_js_number_or_bigint(ctx, methods.len() as u64)
}

pub(crate) unsafe extern "C" fn js_java_lua_fast_constructor(
    ctx: *mut ffi::JSContext,
    _this: ffi::JSValue,
    argc: i32,
    argv: *mut ffi::JSValue,
) -> ffi::JSValue {
    if argc < 2 {
        return throw_type_error(
            ctx,
            b"luaFastConstructor(class, sig[, options]) requires at least 2 arguments\0",
        );
    }

    let class_name = match extract_string_arg(ctx, JSValue(*argv), b"arg 0 must be string\0") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let sig_str = match extract_string_arg(ctx, JSValue(*argv.add(1)), b"arg 1 must be string\0") {
        Ok(s) => s,
        Err(e) => return e,
    };
    if get_return_type_from_sig(&sig_str) != b'V' {
        return throw_type_error(ctx, b"constructor signature must return void\0");
    }

    let env = match ensure_jni_initialized() {
        Ok(e) => e,
        Err(msg) => return throw_internal_error(ctx, msg),
    };

    let (art_method, method_id, class_global_ref, is_static) =
        match resolve_lua_fast_method(env, &class_name, "<init>", &sig_str, false) {
            Ok(v) => v,
            Err(msg) => return throw_internal_error(ctx, msg),
        };
    if is_static {
        return throw_internal_error(
            ctx,
            format!("constructor resolved as static: {}{}", class_name, sig_str),
        );
    }

    let (should_compile, compile_kind) = match parse_lua_fast_options(ctx, argc, argv, 2) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let spec = get_art_method_spec(env, art_method);
    let bridge = find_art_bridge_functions(env, spec.entry_point_offset);
    let mut entry_point = read_entry_point(art_method, spec.entry_point_offset);
    if is_art_quick_entrypoint(entry_point, &bridge) && should_compile {
        let compile = compile_art_method_to_quick(env, art_method, spec.entry_point_offset, bridge, compile_kind);
        entry_point = compile.after;
        crate::jsapi::console::output_verbose(&format!(
            "[luaFastConstructor] compile {}.<init>{} kind={} success={} before={:#x} after={:#x} msg={}",
            class_name, sig_str, compile.kind, compile.success, compile.before, compile.after, compile.message
        ));
    }
    if is_art_quick_entrypoint(entry_point, &bridge) {
        return throw_internal_error(
            ctx,
            format!(
                "luaFastConstructor rejected {}.<init>{}: no independent quick entrypoint (entry={:#x})",
                class_name, sig_str, entry_point
            ),
        );
    }

    let class_mirror = super::decode_global_jobject_raw(env, class_global_ref as *mut std::ffi::c_void).unwrap_or(0);
    output_verbose(&format!(
        "[lua fast ctor] {}.<init>{} class_global={:#x} class_mirror={:#x}",
        class_name, sig_str, class_global_ref as usize, class_mirror
    ));
    let constructor = LuaFastConstructor {
        class_global_ref: class_global_ref as u64,
        class_mirror,
        art_method,
        method_id,
        param_types: parse_jni_param_types(&sig_str),
        shorty: make_shorty(&sig_str),
    };
    let mut constructors = lua_fast_constructors().lock().unwrap_or_else(|e| e.into_inner());
    constructors.push(constructor);
    js_u64_to_js_number_or_bigint(ctx, constructors.len() as u64)
}

pub(crate) unsafe extern "C" fn js_java_lua_fast_field(
    ctx: *mut ffi::JSContext,
    _this: ffi::JSValue,
    argc: i32,
    argv: *mut ffi::JSValue,
) -> ffi::JSValue {
    if argc < 2 {
        return throw_type_error(
            ctx,
            b"luaFastField(class, field[, sig]) requires at least 2 arguments\0",
        );
    }

    let class_name = match extract_string_arg(ctx, JSValue(*argv), b"arg 0 must be string\0") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let field_name = match extract_string_arg(ctx, JSValue(*argv.add(1)), b"arg 1 must be string\0") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let requested_sig = if argc >= 3 {
        let sig_arg = JSValue(*argv.add(2));
        if !sig_arg.is_undefined() && !sig_arg.is_null() {
            match extract_string_arg(ctx, sig_arg, b"arg 2 must be string\0") {
                Ok(s) => Some(s),
                Err(e) => return e,
            }
        } else {
            None
        }
    } else {
        None
    };

    let env = match ensure_jni_initialized() {
        Ok(e) => e,
        Err(msg) => return throw_internal_error(ctx, msg),
    };
    let Some(spec) = get_art_field_spec() else {
        return throw_internal_error(ctx, "unsupported ArtField layout".to_string());
    };

    cache_fields_for_class(env, &class_name);
    let (jni_sig, field_id, is_static) = {
        let guard = FIELD_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(cache) = guard.as_ref() else {
            return throw_internal_error(ctx, format!("field cache unavailable for {}", class_name));
        };
        let Some(fields) = cache.get(&class_name) else {
            return throw_internal_error(ctx, format!("fields unavailable for {}", class_name));
        };
        let Some(info) = fields.get(&field_name) else {
            return throw_internal_error(ctx, format!("field not found: {}.{}", class_name, field_name));
        };
        (info.jni_sig.clone(), info.field_id, info.is_static)
    };

    if let Some(sig) = requested_sig.as_ref() {
        if sig != &jni_sig {
            return throw_type_error(ctx, b"field signature mismatch\0");
        }
    }
    if is_static {
        return throw_type_error(ctx, b"luaFastField only supports instance fields\0");
    }
    if !is_lua_fast_field_type(&jni_sig) {
        return throw_type_error(ctx, b"luaFastField only supports primitive/object instance fields\0");
    }

    let cls = find_class_safe(env, &class_name);
    if cls.is_null() {
        return throw_internal_error(ctx, format!("class not found: {}", class_name));
    }
    let art_field = decode_field_id(env, cls, field_id as u64, is_static);
    jni_check_exc(env);
    if art_field == 0 {
        return throw_internal_error(ctx, format!("failed to decode field id: {}.{}", class_name, field_name));
    }
    refresh_mem_regions();
    let offset = safe_read_u32(art_field + spec.offset_offset as u64);
    if offset == 0 {
        return throw_internal_error(ctx, format!("invalid field offset: {}.{}", class_name, field_name));
    }

    let field = LuaFastField {
        art_field,
        offset,
        is_static,
        value_type: jni_sig.as_bytes()[0],
        jni_sig,
        class_name,
        field_name,
    };
    let mut fields = lua_fast_fields().lock().unwrap_or_else(|e| e.into_inner());
    fields.push(field);
    js_u64_to_js_number_or_bigint(ctx, fields.len() as u64)
}

pub(crate) unsafe extern "C" fn js_java_compile_method(
    ctx: *mut ffi::JSContext,
    _this: ffi::JSValue,
    argc: i32,
    argv: *mut ffi::JSValue,
) -> ffi::JSValue {
    if argc < 3 {
        return throw_type_error(
            ctx,
            b"compileMethod(class, method, sig[, kind]) requires at least 3 arguments\0",
        );
    }

    let class_name = match extract_string_arg(ctx, JSValue(*argv), b"arg 0 must be string\0") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let method_name = match extract_string_arg(ctx, JSValue(*argv.add(1)), b"arg 1 must be string\0") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let sig_str = match extract_string_arg(ctx, JSValue(*argv.add(2)), b"arg 2 must be string\0") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let (actual_sig, force_static) = if let Some(stripped) = sig_str.strip_prefix("static:") {
        (stripped.to_string(), true)
    } else {
        (sig_str, false)
    };
    let kind = if argc >= 4 {
        if let Some(s) = JSValue(*argv.add(3)).to_string(ctx) {
            match RequestedCompileKind::from_str(s.as_str()) {
                Some(k) => k,
                None => return throw_type_error(ctx, b"invalid compile kind\0"),
            }
        } else {
            RequestedCompileKind::Auto
        }
    } else {
        RequestedCompileKind::Auto
    };

    let env = match ensure_jni_initialized() {
        Ok(e) => e,
        Err(msg) => return throw_internal_error(ctx, msg),
    };
    let (art_method, _is_static) = match resolve_art_method(env, &class_name, &method_name, &actual_sig, force_static) {
        Ok(v) => v,
        Err(msg) => return throw_internal_error(ctx, msg),
    };
    let spec = get_art_method_spec(env, art_method);
    let bridge = find_art_bridge_functions(env, spec.entry_point_offset);
    let result = compile_art_method_to_quick(env, art_method, spec.entry_point_offset, bridge, kind);

    let obj = ffi::JS_NewObject(ctx);
    let obj_v = JSValue(obj);
    obj_v.set_property(ctx, "success", JSValue::bool(result.success));
    obj_v.set_property(ctx, "compiled", JSValue::bool(result.compiled));
    obj_v.set_property(ctx, "kind", JSValue::string(ctx, result.kind));
    obj_v.set_property(ctx, "message", JSValue::string(ctx, &result.message));
    set_js_u64_property(ctx, obj, "artMethod", art_method);
    set_js_u64_property(ctx, obj, "before", result.before);
    set_js_u64_property(ctx, obj, "after", result.after);
    obj
}

pub(crate) unsafe extern "C" fn js_java_jit_info(
    ctx: *mut ffi::JSContext,
    _this: ffi::JSValue,
    _argc: i32,
    _argv: *mut ffi::JSValue,
) -> ffi::JSValue {
    let _env = match ensure_jni_initialized() {
        Ok(e) => e,
        Err(msg) => return throw_internal_error(ctx, msg),
    };
    let Some(info) = probe_jit_runtime_info() else {
        return throw_internal_error(ctx, "JIT runtime info unavailable".to_string());
    };

    let obj = ffi::JS_NewObject(ctx);
    let obj_v = JSValue(obj);
    set_js_u64_property(ctx, obj, "runtime", info.runtime);
    set_js_u64_property(ctx, obj, "javaVmOffset", info.java_vm_offset as u64);
    set_js_u64_property(ctx, obj, "jitOffset", info.jit_offset as u64);
    set_js_u64_property(ctx, obj, "jitCodeCacheOffset", info.jit_code_cache_offset as u64);
    set_js_u64_property(ctx, obj, "directJit", info.direct_jit);
    set_js_u64_property(ctx, obj, "runtimeJitCodeCache", info.runtime_jit_code_cache);
    set_js_u64_property(ctx, obj, "directGetCodeCache", info.direct_get_code_cache);
    set_js_u64_property(ctx, obj, "foundJit", info.found_jit);
    obj_v.set_property(ctx, "message", JSValue::string(ctx, &info.message));
    obj
}

pub(crate) unsafe fn compile_art_method_to_quick(
    env: JniEnv,
    art_method: u64,
    entry_point_offset: usize,
    bridge: &ArtBridgeFunctions,
    kind: RequestedCompileKind,
) -> CompileResult {
    let before = read_entry_point(art_method, entry_point_offset);
    if !is_art_quick_entrypoint(before, bridge) {
        return CompileResult {
            before,
            after: before,
            success: true,
            compiled: false,
            kind: "already-quick",
            message: "method already has independent quick code".to_string(),
        };
    }

    let Some(jit) = find_jit_instance() else {
        return CompileResult {
            before,
            after: before,
            success: false,
            compiled: false,
            kind: kind.label(),
            message: "Jit* not found".to_string(),
        };
    };
    let Some(thread) = current_art_thread(env) else {
        return CompileResult {
            before,
            after: before,
            success: false,
            compiled: false,
            kind: kind.label(),
            message: "Thread::Current() unavailable".to_string(),
        };
    };
    let compile_sym = crate::jsapi::module::libart_dlsym(
        "_ZN3art3jit3Jit13CompileMethodEPNS_9ArtMethodEPNS_6ThreadENS_15CompilationKindEb",
    );
    if compile_sym.is_null() {
        return CompileResult {
            before,
            after: before,
            success: false,
            compiled: false,
            kind: kind.label(),
            message: "Jit::CompileMethod symbol not found".to_string(),
        };
    }

    type CompileMethodFn =
        unsafe extern "C" fn(this: u64, method: u64, thread: u64, compilation_kind: u32, prejit: u8) -> u8;
    let compile_method: CompileMethodFn = std::mem::transmute(compile_sym);

    let mut last_kind = kind.label();
    let mut saw_compile_success = false;
    for k in kind.sequence() {
        last_kind = match *k {
            1 => "fast",
            2 => "baseline",
            3 => "optimized",
            _ => "unknown",
        };
        let ok = compile_method(jit, art_method, thread, *k, 0) != 0;
        let after = read_entry_point(art_method, entry_point_offset);
        if ok {
            saw_compile_success = true;
        }
        if !is_art_quick_entrypoint(after, bridge) {
            return CompileResult {
                before,
                after,
                success: true,
                compiled: true,
                kind: last_kind,
                message: format!("Jit::CompileMethod({}) succeeded", last_kind),
            };
        }
    }

    let after = read_entry_point(art_method, entry_point_offset);
    CompileResult {
        before,
        after,
        success: false,
        compiled: saw_compile_success,
        kind: last_kind,
        message: if saw_compile_success {
            "JIT reported success but entrypoint is still a shared ART bridge".to_string()
        } else {
            "Jit::CompileMethod returned false".to_string()
        },
    }
}

unsafe fn current_art_thread(env: JniEnv) -> Option<u64> {
    let sym = crate::jsapi::module::libart_dlsym("_ZN3art6Thread7CurrentEv");
    if !sym.is_null() {
        type ThreadCurrentFn = unsafe extern "C" fn() -> u64;
        let thread_current: ThreadCurrentFn = std::mem::transmute(sym);
        let thread = thread_current() & super::PAC_STRIP_MASK;
        if thread != 0 {
            return Some(thread);
        }
    }
    if !env.is_null() {
        let thread = *((env as usize + 8) as *const u64) & super::PAC_STRIP_MASK;
        if thread != 0 {
            return Some(thread);
        }
    }
    None
}

type ArtMethodInvokeFn = unsafe extern "C" fn(
    method: *mut std::ffi::c_void,
    thread: *mut std::ffi::c_void,
    args: *mut u32,
    args_size: u32,
    result: *mut u64,
    shorty: *const std::os::raw::c_char,
);

static ART_METHOD_INVOKE: OnceLock<Option<ArtMethodInvokeFn>> = OnceLock::new();

pub(crate) unsafe fn invoke_lua_fast_method(
    method: &LuaFastMethod,
    receiver: u64,
    args: &[LuaFastArg],
) -> Result<u64, String> {
    if !method.is_static && receiver == 0 {
        return Err("jcall instance receiver is null".to_string());
    }
    if args.len() != method.param_types.len() {
        return Err(format!(
            "jcall argument count mismatch: expected {}, got {}",
            method.param_types.len(),
            args.len()
        ));
    }

    let env = get_thread_env().unwrap_or(std::ptr::null_mut());
    if env.is_null() {
        return Err("current JNIEnv is null".to_string());
    }
    let mut local_refs = Vec::new();
    let receiver_obj = if method.is_static {
        std::ptr::null_mut()
    } else {
        let local = raw_mirror_to_local_ref(env, receiver as *mut std::ffi::c_void);
        if local.is_null() {
            return Err("failed to create receiver local ref".to_string());
        }
        local_refs.push(local);
        local
    };

    let mut jni_args = Vec::with_capacity(method.param_types.len());
    for (i, type_sig) in method.param_types.iter().enumerate() {
        jni_args.push(lua_fast_arg_to_jni_value(
            env,
            args[i],
            type_sig.as_str(),
            &mut local_refs,
        )?);
    }

    let ret = call_lua_fast_jni_method(env, method, receiver_obj, &jni_args);
    delete_local_refs(env, local_refs);
    ret
}

#[allow(dead_code)]
unsafe fn invoke_lua_fast_method_art(
    method: &LuaFastMethod,
    receiver: u64,
    args: &[LuaFastArg],
) -> Result<u64, String> {
    let mut invoke_args = Vec::with_capacity(1 + method.param_types.len() * 2);
    if !method.is_static {
        push_art_invoke_arg(&mut invoke_args, "L", receiver);
    }

    for (i, type_sig) in method.param_types.iter().enumerate() {
        let raw = resolve_lua_fast_arg(args[i], type_sig.as_str())?;
        push_art_invoke_arg(&mut invoke_args, type_sig.as_str(), raw);
    }

    let Some(ret) = crate::lua::callback::with_current_quick_runnable(|thread| {
        invoke_lua_fast_method_art_ready(method, thread as u64, &mut invoke_args)
    }) else {
        return Err("jcall is only available inside quick Lua callbacks".to_string());
    };
    ret
}

pub(crate) unsafe fn invoke_lua_fast_method_art_on_thread(
    method: &LuaFastMethod,
    thread: u64,
    receiver: u64,
    args: &[LuaFastArg],
) -> Result<u64, String> {
    if thread == 0 {
        return Err("current ART Thread is null".to_string());
    }
    if !method.is_static && receiver == 0 {
        return Err("jcall instance receiver is null".to_string());
    }
    if args.len() != method.param_types.len() {
        return Err(format!(
            "jcall argument count mismatch: expected {}, got {}",
            method.param_types.len(),
            args.len()
        ));
    }

    let mut invoke_args = Vec::with_capacity(1 + method.param_types.len() * 2);
    if !method.is_static {
        push_art_invoke_arg(&mut invoke_args, "L", receiver);
    }
    for (i, type_sig) in method.param_types.iter().enumerate() {
        let raw = resolve_lua_fast_arg(args[i], type_sig.as_str())?;
        push_art_invoke_arg(&mut invoke_args, type_sig.as_str(), raw);
    }
    let before_exception = thread_exception(thread);
    let ret = invoke_lua_fast_method_art_ready(method, thread, &mut invoke_args)?;
    if clear_new_thread_exception(thread, before_exception) {
        return Err("ArtMethod::Invoke method raised exception".to_string());
    }
    Ok(ret)
}

pub(crate) unsafe fn invoke_lua_fast_method_raw_on_thread(
    method: &LuaFastMethod,
    thread: u64,
    receiver: u64,
    args: &[u64],
) -> Result<u64, String> {
    if thread == 0 {
        return Err("current ART Thread is null".to_string());
    }
    if !method.is_static && receiver == 0 {
        return Err("jcall instance receiver is null".to_string());
    }
    if args.len() != method.param_types.len() {
        return Err(format!(
            "jcall argument count mismatch: expected {}, got {}",
            method.param_types.len(),
            args.len()
        ));
    }

    let mut invoke_args = StackArtInvokeArgs::new();
    if !method.is_static {
        invoke_args.push("L", receiver)?;
    }
    for (i, type_sig) in method.param_types.iter().enumerate() {
        invoke_args.push(type_sig.as_str(), args[i])?;
    }
    let before_exception = thread_exception(thread);
    let ret = invoke_lua_fast_method_art_ready_raw(method, thread, invoke_args.as_mut_ptr(), invoke_args.size_bytes())?;
    if clear_new_thread_exception(thread, before_exception) {
        return Err("ArtMethod::Invoke method raised exception".to_string());
    }
    Ok(ret)
}

pub(crate) unsafe fn lua_fast_method_receiver_is_exact(method: &LuaFastMethod, receiver: u64) -> bool {
    method.is_static || object_class_matches(receiver, method.class_mirror)
}

unsafe fn object_class_matches(obj: u64, class_mirror: u64) -> bool {
    if obj == 0 || class_mirror == 0 {
        return false;
    }
    let compressed_class = std::ptr::read_volatile(obj as *const u32) as u64;
    compressed_class == (class_mirror & 0xffff_ffff)
}

unsafe fn invoke_lua_fast_method_art_ready(
    method: &LuaFastMethod,
    thread: u64,
    invoke_args: &mut Vec<u32>,
) -> Result<u64, String> {
    invoke_lua_fast_method_art_ready_raw(
        method,
        thread,
        invoke_args.as_mut_ptr(),
        (invoke_args.len() * std::mem::size_of::<u32>()) as u32,
    )
}

unsafe fn invoke_lua_fast_method_art_ready_raw(
    method: &LuaFastMethod,
    thread: u64,
    args: *mut u32,
    args_size: u32,
) -> Result<u64, String> {
    let Some(invoke) = art_method_invoke() else {
        return Err("ArtMethod::Invoke symbol not found".to_string());
    };
    let mut result = 0u64;
    invoke(
        method.art_method as *mut std::ffi::c_void,
        thread as *mut std::ffi::c_void,
        args,
        args_size,
        &mut result as *mut u64,
        method.shorty.as_ptr(),
    );
    Ok(result)
}

pub(crate) unsafe fn invoke_lua_fast_constructor(
    ctor: &LuaFastConstructor,
    receiver: u64,
    args: &[LuaFastArg],
) -> Result<(), String> {
    if receiver == 0 {
        return Err("jnew receiver allocation returned null".to_string());
    }
    if args.len() != ctor.param_types.len() {
        return Err(format!(
            "jnew argument count mismatch: expected {}, got {}",
            ctor.param_types.len(),
            args.len()
        ));
    }

    if let Ok(()) = invoke_lua_fast_constructor_art(ctor, receiver, args) {
        return Ok(());
    }

    let env = get_thread_env().unwrap_or(std::ptr::null_mut());
    if env.is_null() {
        return Err("current JNIEnv is null".to_string());
    }
    let mut local_refs = Vec::new();
    let receiver_obj = raw_mirror_to_local_ref(env, receiver as *mut std::ffi::c_void);
    if receiver_obj.is_null() {
        return Err("failed to create constructor receiver local ref".to_string());
    }
    local_refs.push(receiver_obj);
    let mut jni_args = Vec::with_capacity(ctor.param_types.len());
    for (i, type_sig) in ctor.param_types.iter().enumerate() {
        jni_args.push(lua_fast_arg_to_jni_value(
            env,
            args[i],
            type_sig.as_str(),
            &mut local_refs,
        )?);
    }

    let ret = call_lua_fast_jni_constructor(env, ctor, receiver_obj, &jni_args);
    delete_local_refs(env, local_refs);
    ret
}

#[allow(dead_code)]
unsafe fn invoke_lua_fast_constructor_art(
    ctor: &LuaFastConstructor,
    receiver: u64,
    args: &[LuaFastArg],
) -> Result<(), String> {
    let mut invoke_args = Vec::with_capacity(1 + ctor.param_types.len() * 2);
    push_art_invoke_arg(&mut invoke_args, "L", receiver);

    for (i, type_sig) in ctor.param_types.iter().enumerate() {
        let raw = resolve_lua_fast_arg(args[i], type_sig.as_str())?;
        push_art_invoke_arg(&mut invoke_args, type_sig.as_str(), raw);
    }

    let Some(ret) = crate::lua::callback::with_current_quick_runnable(|thread| {
        invoke_lua_fast_constructor_art_ready(ctor, thread as u64, &mut invoke_args)
    }) else {
        return Err("jnew is only available inside quick Lua callbacks".to_string());
    };
    ret?;

    let env = get_thread_env().unwrap_or(std::ptr::null_mut());
    if !env.is_null() && jni_check_exc(env) {
        Err("ArtMethod::Invoke constructor raised exception".to_string())
    } else {
        Ok(())
    }
}

pub(crate) unsafe fn invoke_lua_fast_constructor_art_on_thread(
    ctor: &LuaFastConstructor,
    thread: u64,
    receiver: u64,
    args: &[LuaFastArg],
) -> Result<(), String> {
    if thread == 0 {
        return Err("current ART Thread is null".to_string());
    }
    if receiver == 0 {
        return Err("jnew receiver allocation returned null".to_string());
    }
    if args.len() != ctor.param_types.len() {
        return Err(format!(
            "jnew argument count mismatch: expected {}, got {}",
            ctor.param_types.len(),
            args.len()
        ));
    }

    let mut invoke_args = Vec::with_capacity(1 + ctor.param_types.len() * 2);
    push_art_invoke_arg(&mut invoke_args, "L", receiver);
    for (i, type_sig) in ctor.param_types.iter().enumerate() {
        let raw = resolve_lua_fast_arg(args[i], type_sig.as_str())?;
        push_art_invoke_arg(&mut invoke_args, type_sig.as_str(), raw);
    }
    let before_exception = thread_exception(thread);
    invoke_lua_fast_constructor_art_ready(ctor, thread, &mut invoke_args)?;
    if clear_new_thread_exception(thread, before_exception) {
        return Err("ArtMethod::Invoke constructor raised exception".to_string());
    }
    Ok(())
}

pub(crate) unsafe fn invoke_lua_fast_constructor_raw_on_thread(
    ctor: &LuaFastConstructor,
    thread: u64,
    receiver: u64,
    args: &[u64],
) -> Result<(), String> {
    if thread == 0 {
        return Err("current ART Thread is null".to_string());
    }
    if receiver == 0 {
        return Err("jnew receiver allocation returned null".to_string());
    }
    if args.len() != ctor.param_types.len() {
        return Err(format!(
            "jnew argument count mismatch: expected {}, got {}",
            ctor.param_types.len(),
            args.len()
        ));
    }

    let mut invoke_args = StackArtInvokeArgs::new();
    invoke_args.push("L", receiver)?;
    for (i, type_sig) in ctor.param_types.iter().enumerate() {
        invoke_args.push(type_sig.as_str(), args[i])?;
    }
    let before_exception = thread_exception(thread);
    invoke_lua_fast_constructor_art_ready_raw(ctor, thread, invoke_args.as_mut_ptr(), invoke_args.size_bytes())?;
    if clear_new_thread_exception(thread, before_exception) {
        return Err("ArtMethod::Invoke constructor raised exception".to_string());
    }
    Ok(())
}

unsafe fn invoke_lua_fast_constructor_art_ready(
    ctor: &LuaFastConstructor,
    thread: u64,
    invoke_args: &mut Vec<u32>,
) -> Result<(), String> {
    invoke_lua_fast_constructor_art_ready_raw(
        ctor,
        thread,
        invoke_args.as_mut_ptr(),
        (invoke_args.len() * std::mem::size_of::<u32>()) as u32,
    )
}

unsafe fn invoke_lua_fast_constructor_art_ready_raw(
    ctor: &LuaFastConstructor,
    thread: u64,
    args: *mut u32,
    args_size: u32,
) -> Result<(), String> {
    let Some(invoke) = art_method_invoke() else {
        return Err("ArtMethod::Invoke symbol not found".to_string());
    };
    let mut result = 0u64;
    invoke(
        ctor.art_method as *mut std::ffi::c_void,
        thread as *mut std::ffi::c_void,
        args,
        args_size,
        &mut result as *mut u64,
        ctor.shorty.as_ptr(),
    );
    Ok(())
}

unsafe fn lua_fast_arg_to_jni_value(
    env: JniEnv,
    arg: LuaFastArg,
    type_sig: &str,
    local_refs: &mut Vec<*mut std::ffi::c_void>,
) -> Result<u64, String> {
    if !matches!(type_sig.as_bytes().first().copied(), Some(b'L' | b'[')) {
        return Ok(match arg {
            LuaFastArg::Raw(raw) => raw,
            LuaFastArg::JniRef { object, .. } => object as u64,
        });
    }

    match arg {
        LuaFastArg::Raw(0) => Ok(0),
        LuaFastArg::Raw(raw) => {
            let local = raw_mirror_to_local_ref(env, raw as *mut std::ffi::c_void);
            if local.is_null() {
                Err("failed to create object argument local ref".to_string())
            } else {
                local_refs.push(local);
                Ok(local as u64)
            }
        }
        LuaFastArg::JniRef { object, .. } => Ok(object as u64),
    }
}

unsafe fn call_lua_fast_jni_method(
    env: JniEnv,
    method: &LuaFastMethod,
    receiver: *mut std::ffi::c_void,
    args: &[u64],
) -> Result<u64, String> {
    let cls = method.class_global_ref as *mut std::ffi::c_void;
    let mid = method.method_id as *mut std::ffi::c_void;
    let argv = args.as_ptr() as *const std::ffi::c_void;
    let vtable = *(env as *const *const usize);

    let result = match (method.is_static, method.return_type) {
        (true, b'V') => {
            type FnTy =
                unsafe extern "C" fn(JniEnv, *mut std::ffi::c_void, *mut std::ffi::c_void, *const std::ffi::c_void);
            let f: FnTy = std::mem::transmute(*vtable.add(JNI_CALL_STATIC_VOID_METHOD_A));
            f(env, cls, mid, argv);
            0
        }
        (false, b'V') => {
            type FnTy =
                unsafe extern "C" fn(JniEnv, *mut std::ffi::c_void, *mut std::ffi::c_void, *const std::ffi::c_void);
            let f: FnTy = std::mem::transmute(*vtable.add(JNI_CALL_VOID_METHOD_A));
            f(env, receiver, mid, argv);
            0
        }
        (true, b'Z') => call_static_primitive_u8(env, cls, mid, argv, JNI_CALL_STATIC_BOOLEAN_METHOD_A) as u64,
        (false, b'Z') => call_primitive_u8(env, receiver, mid, argv, JNI_CALL_BOOLEAN_METHOD_A) as u64,
        (true, b'B') => call_static_primitive_i8(env, cls, mid, argv, JNI_CALL_STATIC_BYTE_METHOD_A) as u64,
        (false, b'B') => call_primitive_i8(env, receiver, mid, argv, JNI_CALL_BYTE_METHOD_A) as u64,
        (true, b'C') => call_static_primitive_u16(env, cls, mid, argv, JNI_CALL_STATIC_CHAR_METHOD_A) as u64,
        (false, b'C') => call_primitive_u16(env, receiver, mid, argv, JNI_CALL_CHAR_METHOD_A) as u64,
        (true, b'S') => call_static_primitive_i16(env, cls, mid, argv, JNI_CALL_STATIC_SHORT_METHOD_A) as u64,
        (false, b'S') => call_primitive_i16(env, receiver, mid, argv, JNI_CALL_SHORT_METHOD_A) as u64,
        (true, b'I') => call_static_primitive_i32(env, cls, mid, argv, JNI_CALL_STATIC_INT_METHOD_A) as u64,
        (false, b'I') => call_primitive_i32(env, receiver, mid, argv, JNI_CALL_INT_METHOD_A) as u64,
        (true, b'J') => call_static_primitive_i64(env, cls, mid, argv, JNI_CALL_STATIC_LONG_METHOD_A) as u64,
        (false, b'J') => call_primitive_i64(env, receiver, mid, argv, JNI_CALL_LONG_METHOD_A) as u64,
        (true, b'F') => call_static_primitive_f32(env, cls, mid, argv, JNI_CALL_STATIC_FLOAT_METHOD_A).to_bits() as u64,
        (false, b'F') => call_primitive_f32(env, receiver, mid, argv, JNI_CALL_FLOAT_METHOD_A).to_bits() as u64,
        (true, b'D') => call_static_primitive_f64(env, cls, mid, argv, JNI_CALL_STATIC_DOUBLE_METHOD_A).to_bits(),
        (false, b'D') => call_primitive_f64(env, receiver, mid, argv, JNI_CALL_DOUBLE_METHOD_A).to_bits(),
        (true, _) => call_static_object_method(env, cls, mid, argv)?,
        (false, _) => call_object_method(env, receiver, mid, argv)?,
    };
    if jni_check_exc(env) {
        Err("JNI method call raised exception".to_string())
    } else {
        Ok(result)
    }
}

unsafe fn call_lua_fast_jni_constructor(
    env: JniEnv,
    ctor: &LuaFastConstructor,
    receiver: *mut std::ffi::c_void,
    args: &[u64],
) -> Result<(), String> {
    let mid = ctor.method_id as *mut std::ffi::c_void;
    let argv = args.as_ptr() as *const std::ffi::c_void;
    let vtable = *(env as *const *const usize);
    type FnTy = unsafe extern "C" fn(JniEnv, *mut std::ffi::c_void, *mut std::ffi::c_void, *const std::ffi::c_void);
    let f: FnTy = std::mem::transmute(*vtable.add(JNI_CALL_VOID_METHOD_A));
    f(env, receiver, mid, argv);
    if jni_check_exc(env) {
        Err("JNI constructor call raised exception".to_string())
    } else {
        Ok(())
    }
}

unsafe fn call_object_method(
    env: JniEnv,
    obj: *mut std::ffi::c_void,
    mid: *mut std::ffi::c_void,
    args: *const std::ffi::c_void,
) -> Result<u64, String> {
    let f: CallObjectMethodAFn = jni_fn!(env, CallObjectMethodAFn, JNI_CALL_OBJECT_METHOD_A);
    local_object_return_to_raw(env, f(env, obj, mid, args))
}

unsafe fn call_static_object_method(
    env: JniEnv,
    cls: *mut std::ffi::c_void,
    mid: *mut std::ffi::c_void,
    args: *const std::ffi::c_void,
) -> Result<u64, String> {
    let f: CallStaticObjectMethodAFn = jni_fn!(env, CallStaticObjectMethodAFn, JNI_CALL_STATIC_OBJECT_METHOD_A);
    local_object_return_to_raw(env, f(env, cls, mid, args))
}

unsafe fn local_object_return_to_raw(env: JniEnv, obj: *mut std::ffi::c_void) -> Result<u64, String> {
    if obj.is_null() {
        return Ok(0);
    }
    let raw = super::decode_jobject_raw(env, obj)
        .or_else(|| crate::lua::api::decode_jni_local_ref_via_irt(env as *const std::ffi::c_void, obj))
        .ok_or_else(|| "failed to decode JNI object return".to_string())?;
    if !crate::lua::api::push_current_local_ref(obj) {
        delete_local_ref(env, obj);
        return Err("callback local-ref scope unavailable".to_string());
    }
    Ok(raw)
}

pub(crate) unsafe fn with_lua_fast_art_handle_scope<R>(thread: u64, f: impl FnOnce() -> R) -> R {
    FAST_ART_HANDLE_SCOPE_ENTER.fetch_add(1, Ordering::Relaxed);
    let env = get_thread_env().unwrap_or(std::ptr::null_mut());
    if env.is_null() {
        FAST_ART_HANDLE_SCOPE_UNAVAILABLE.fetch_add(1, Ordering::Relaxed);
        return f();
    }
    let Some(spec) = super::art_thread::get_art_thread_spec(env) else {
        FAST_ART_HANDLE_SCOPE_UNAVAILABLE.fetch_add(1, Ordering::Relaxed);
        return f();
    };
    if thread == 0 {
        FAST_ART_HANDLE_SCOPE_UNAVAILABLE.fetch_add(1, Ordering::Relaxed);
        return f();
    }

    let top_addr = (thread as usize + spec.top_handle_scope_offset) as *mut u64;
    let previous_top = std::ptr::read_volatile(top_addr);
    let mut scope = FastArtHandleScope::new(previous_top);
    let scope_ptr = &mut scope as *mut FastArtHandleScope;
    std::ptr::write_volatile(top_addr, scope_ptr as u64);
    let previous_tls = CURRENT_FAST_ART_HANDLE_SCOPE.with(|current| {
        let previous = current.get();
        current.set(scope_ptr);
        previous
    });

    let result = f();

    let used_roots = (*scope_ptr).size as u64;
    update_fast_max(&FAST_ART_HANDLE_SCOPE_MAX_ROOTS, used_roots);
    CURRENT_FAST_ART_HANDLE_SCOPE.with(|current| current.set(previous_tls));
    let current_top = std::ptr::read_volatile(top_addr);
    if current_top == scope_ptr as u64 {
        std::ptr::write_volatile(top_addr, previous_top);
    } else {
        FAST_ART_HANDLE_SCOPE_LEAKED.fetch_add(1, Ordering::Relaxed);
        std::ptr::write_volatile(top_addr, previous_top);
        return result;
    }
    result
}

pub(crate) unsafe fn root_lua_fast_raw_object_for_callback(raw: u64) -> Result<LuaFastArtRoot, String> {
    if raw == 0 {
        return Err("cannot root null raw object".to_string());
    }
    if raw > u32::MAX as u64 {
        return Err(format!("raw object is not a compressed ART reference: {:#x}", raw));
    }
    CURRENT_FAST_ART_HANDLE_SCOPE.with(|current| {
        let scope = current.get();
        if scope.is_null() {
            FAST_ART_HANDLE_SCOPE_ROOT_FAILED.fetch_add(1, Ordering::Relaxed);
            return Err("fast ART handle scope unavailable".to_string());
        }
        let scope = &mut *scope;
        let slot = scope.size as usize;
        if slot >= FAST_ART_HANDLE_SCOPE_CAPACITY {
            FAST_ART_HANDLE_SCOPE_ROOT_FAILED.fetch_add(1, Ordering::Relaxed);
            FAST_ART_HANDLE_SCOPE_CAPACITY_EXCEEDED.fetch_add(1, Ordering::Relaxed);
            return Err("fast ART handle scope capacity exceeded".to_string());
        }
        scope.refs[slot] = raw as u32;
        scope.size += 1;
        Ok(LuaFastArtRoot { slot: slot as u32 })
    })
}

pub(crate) unsafe fn read_lua_fast_art_root(root: LuaFastArtRoot) -> Option<u64> {
    CURRENT_FAST_ART_HANDLE_SCOPE.with(|current| {
        let scope = current.get();
        if scope.is_null() {
            return None;
        }
        let scope = &*scope;
        let slot = root.slot as usize;
        if slot >= scope.size as usize || slot >= FAST_ART_HANDLE_SCOPE_CAPACITY {
            None
        } else {
            Some(scope.refs[slot] as u64)
        }
    })
}

unsafe fn raw_mirror_to_local_ref(env: JniEnv, raw: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    if env.is_null() || raw.is_null() {
        return std::ptr::null_mut();
    }
    type ArtNewLocalRefFn = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    static ART_NEW_LOCAL_REF: OnceLock<Option<ArtNewLocalRefFn>> = OnceLock::new();
    let local = if let Some(add_ref) = *ART_NEW_LOCAL_REF.get_or_init(|| {
        let sym = crate::jsapi::module::libart_dlsym("_ZN3art9JNIEnvExt11NewLocalRefEPNS_6mirror6ObjectE");
        if sym.is_null() {
            None
        } else {
            Some(std::mem::transmute(sym))
        }
    }) {
        add_ref(env as *mut std::ffi::c_void, raw)
    } else {
        std::ptr::null_mut()
    };
    if jni_check_exc(env) {
        std::ptr::null_mut()
    } else {
        local
    }
}

unsafe fn delete_local_ref(env: JniEnv, obj: *mut std::ffi::c_void) {
    if obj.is_null() {
        return;
    }
    let delete_local_ref: DeleteLocalRefFn = jni_fn!(env, DeleteLocalRefFn, JNI_DELETE_LOCAL_REF);
    delete_local_ref(env, obj);
}

unsafe fn delete_local_refs(env: JniEnv, refs: Vec<*mut std::ffi::c_void>) {
    for obj in refs {
        delete_local_ref(env, obj);
    }
}

macro_rules! primitive_call {
    ($name:ident, $ret:ty) => {
        unsafe fn $name(
            env: JniEnv,
            obj: *mut std::ffi::c_void,
            mid: *mut std::ffi::c_void,
            args: *const std::ffi::c_void,
            index: usize,
        ) -> $ret {
            type FnTy = unsafe extern "C" fn(
                JniEnv,
                *mut std::ffi::c_void,
                *mut std::ffi::c_void,
                *const std::ffi::c_void,
            ) -> $ret;
            let vtable = *(env as *const *const usize);
            let f: FnTy = std::mem::transmute(*vtable.add(index));
            f(env, obj, mid, args)
        }
    };
}

macro_rules! primitive_static_call {
    ($name:ident, $ret:ty) => {
        unsafe fn $name(
            env: JniEnv,
            cls: *mut std::ffi::c_void,
            mid: *mut std::ffi::c_void,
            args: *const std::ffi::c_void,
            index: usize,
        ) -> $ret {
            type FnTy = unsafe extern "C" fn(
                JniEnv,
                *mut std::ffi::c_void,
                *mut std::ffi::c_void,
                *const std::ffi::c_void,
            ) -> $ret;
            let vtable = *(env as *const *const usize);
            let f: FnTy = std::mem::transmute(*vtable.add(index));
            f(env, cls, mid, args)
        }
    };
}

primitive_call!(call_primitive_u8, u8);
primitive_call!(call_primitive_i8, i8);
primitive_call!(call_primitive_u16, u16);
primitive_call!(call_primitive_i16, i16);
primitive_call!(call_primitive_i32, i32);
primitive_call!(call_primitive_i64, i64);
primitive_call!(call_primitive_f32, f32);
primitive_call!(call_primitive_f64, f64);
primitive_static_call!(call_static_primitive_u8, u8);
primitive_static_call!(call_static_primitive_i8, i8);
primitive_static_call!(call_static_primitive_u16, u16);
primitive_static_call!(call_static_primitive_i16, i16);
primitive_static_call!(call_static_primitive_i32, i32);
primitive_static_call!(call_static_primitive_i64, i64);
primitive_static_call!(call_static_primitive_f32, f32);
primitive_static_call!(call_static_primitive_f64, f64);

unsafe fn art_method_invoke() -> Option<ArtMethodInvokeFn> {
    *ART_METHOD_INVOKE.get_or_init(|| {
        let sym = crate::jsapi::module::libart_dlsym("_ZN3art9ArtMethod6InvokeEPNS_6ThreadEPjjPNS_6JValueEPKc");
        if sym.is_null() {
            None
        } else {
            Some(std::mem::transmute(sym))
        }
    })
}

fn push_art_invoke_arg(out: &mut Vec<u32>, type_sig: &str, raw: u64) {
    match type_sig.as_bytes().first().copied() {
        Some(b'J' | b'D') => {
            out.push(raw as u32);
            out.push((raw >> 32) as u32);
        }
        Some(b'F') => out.push(raw as u32),
        Some(b'L' | b'[') => out.push(raw as u32),
        _ => out.push(raw as u32),
    }
}

struct StackArtInvokeArgs {
    words: [u32; FAST_ART_STACK_INVOKE_WORDS],
    len: usize,
}

impl StackArtInvokeArgs {
    fn new() -> Self {
        Self {
            words: [0; FAST_ART_STACK_INVOKE_WORDS],
            len: 0,
        }
    }

    fn push(&mut self, type_sig: &str, raw: u64) -> Result<(), String> {
        match type_sig.as_bytes().first().copied() {
            Some(b'J' | b'D') => {
                self.push_word(raw as u32)?;
                self.push_word((raw >> 32) as u32)
            }
            Some(b'F') => self.push_word(raw as u32),
            Some(b'L' | b'[') => self.push_word(raw as u32),
            _ => self.push_word(raw as u32),
        }
    }

    fn push_word(&mut self, word: u32) -> Result<(), String> {
        if self.len >= self.words.len() {
            return Err("ArtMethod::Invoke argument buffer exceeded fast stack capacity".to_string());
        }
        self.words[self.len] = word;
        self.len += 1;
        Ok(())
    }

    fn as_mut_ptr(&mut self) -> *mut u32 {
        self.words.as_mut_ptr()
    }

    fn size_bytes(&self) -> u32 {
        (self.len * std::mem::size_of::<u32>()) as u32
    }
}

unsafe fn resolve_lua_fast_arg(arg: LuaFastArg, type_sig: &str) -> Result<u64, String> {
    match arg {
        LuaFastArg::Raw(raw) => Ok(raw),
        LuaFastArg::JniRef { env, object } => {
            if !matches!(type_sig.as_bytes().first().copied(), Some(b'L' | b'[')) {
                return Ok(object as u64);
            }
            super::decode_jobject_raw(env, object)
                .or_else(|| crate::lua::api::decode_jni_local_ref_via_irt(env as *const std::ffi::c_void, object))
                .or_else(|| super::decode_global_jobject_raw(env, object))
                .ok_or_else(|| "failed to decode JNI ref for quick call".to_string())
        }
    }
}

pub(crate) unsafe fn alloc_lua_fast_object_quick_on_thread(thread: u64, class_mirror: u64) -> Option<u64> {
    if thread == 0 || class_mirror == 0 {
        FAST_TLAB_ALLOC_MISS.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let size_offset = super::heap_scan::resolve_class_object_size_offset();
    let object_size = std::ptr::read_volatile((class_mirror as usize + size_offset) as *const u32);
    if object_size == 0 || object_size > MAX_TLAB_FAST_OBJECT_SIZE || object_size % 8 != 0 {
        FAST_TLAB_ALLOC_MISS.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let pos_addr = (thread as usize + THREAD_LOCAL_POS_OFFSET) as *mut u64;
    let end_addr = (thread as usize + THREAD_LOCAL_END_OFFSET) as *const u64;
    let pos = std::ptr::read_volatile(pos_addr);
    let end = std::ptr::read_volatile(end_addr);
    let Some(next) = pos.checked_add(object_size as u64) else {
        FAST_TLAB_ALLOC_MISS.fetch_add(1, Ordering::Relaxed);
        return alloc_lua_fast_object_quick_slow_on_thread(thread, class_mirror);
    };
    if pos == 0 || next > end {
        FAST_TLAB_ALLOC_MISS.fetch_add(1, Ordering::Relaxed);
        return alloc_lua_fast_object_quick_slow_on_thread(thread, class_mirror);
    }
    std::ptr::write_volatile(pos_addr, next);
    std::ptr::write_bytes(pos as *mut u8, 0, object_size as usize);
    std::ptr::write_volatile(
        (pos as usize + MIRROR_OBJECT_CLASS_OFFSET) as *mut u32,
        class_mirror as u32,
    );
    std::ptr::write_volatile((pos as usize + MIRROR_OBJECT_LOCK_WORD_OFFSET) as *mut u32, 0);
    std::sync::atomic::fence(Ordering::Release);
    FAST_TLAB_ALLOC_HIT.fetch_add(1, Ordering::Relaxed);
    Some(pos)
}

unsafe fn alloc_lua_fast_object_quick_slow_on_thread(thread: u64, class_mirror: u64) -> Option<u64> {
    if thread == 0 || class_mirror == 0 {
        return None;
    }
    let entry = quick_entrypoint(thread as usize, QUICK_ALLOC_OBJECT_INITIALIZED_INDEX)?;
    FAST_QUICK_ALLOC_SLOW_PATH.fetch_add(1, Ordering::Relaxed);
    let before_exception = thread_exception(thread);
    let raw = call_quick_alloc_object(entry as usize, thread as usize, class_mirror as usize) as u64;
    if clear_new_thread_exception(thread, before_exception) {
        return None;
    }
    (raw != 0).then_some(raw)
}

#[inline]
pub(crate) unsafe fn fast_art_exception_stats() -> (u64, u64) {
    (
        FAST_ART_EXCEPTION_SEEN.load(Ordering::Acquire),
        FAST_ART_EXCEPTION_CLEARED.load(Ordering::Acquire),
    )
}

#[inline]
pub(crate) unsafe fn fast_art_handle_scope_stats() -> (u64, u64, u64, u64, u64, u64) {
    (
        FAST_ART_HANDLE_SCOPE_ENTER.load(Ordering::Acquire),
        FAST_ART_HANDLE_SCOPE_UNAVAILABLE.load(Ordering::Acquire),
        FAST_ART_HANDLE_SCOPE_LEAKED.load(Ordering::Acquire),
        FAST_ART_HANDLE_SCOPE_MAX_ROOTS.load(Ordering::Acquire),
        FAST_ART_HANDLE_SCOPE_ROOT_FAILED.load(Ordering::Acquire),
        FAST_ART_HANDLE_SCOPE_CAPACITY_EXCEEDED.load(Ordering::Acquire),
    )
}

#[inline]
pub(crate) unsafe fn fast_tlab_alloc_stats() -> (u64, u64, u64) {
    (
        FAST_TLAB_ALLOC_HIT.load(Ordering::Acquire),
        FAST_TLAB_ALLOC_MISS.load(Ordering::Acquire),
        FAST_QUICK_ALLOC_SLOW_PATH.load(Ordering::Acquire),
    )
}

#[inline]
unsafe fn thread_exception(thread: u64) -> u64 {
    if thread == 0 {
        return 0;
    }
    std::ptr::read_volatile((thread as usize + THREAD_EXCEPTION_OFFSET) as *const u64)
}

#[inline]
unsafe fn clear_new_thread_exception(thread: u64, before_exception: u64) -> bool {
    if thread == 0 {
        return false;
    }
    let exception_addr = (thread as usize + THREAD_EXCEPTION_OFFSET) as *mut u64;
    let after_exception = std::ptr::read_volatile(exception_addr);
    if after_exception == 0 || after_exception == before_exception {
        return false;
    }
    FAST_ART_EXCEPTION_SEEN.fetch_add(1, Ordering::Relaxed);
    if before_exception == 0 {
        std::ptr::write_volatile(exception_addr, 0);
        FAST_ART_EXCEPTION_CLEARED.fetch_add(1, Ordering::Relaxed);
        return true;
    }
    false
}

unsafe fn quick_entrypoint(thread: usize, index: usize) -> Option<u64> {
    if thread == 0 || index >= QUICK_ENTRYPOINT_COUNT {
        return None;
    }
    let cached = QUICK_ENTRYPOINTS_OFFSET.load(Ordering::Acquire);
    if cached == QUICK_ENTRYPOINTS_OFFSET_FAILED {
        return None;
    }
    if cached != 0 {
        let off = cached - 1;
        let entry = std::ptr::read_volatile((thread + off + index * 8) as *const u64);
        return crate::jsapi::module::is_in_libart(entry).then_some(entry);
    }

    let max_off = QUICK_SCAN_LIMIT.saturating_sub(QUICK_ENTRYPOINT_COUNT * 8);
    for off in (0..=max_off).step_by(8) {
        let base = (thread + off) as *const u64;
        let start = std::ptr::read_volatile(base.add(QUICK_JNI_METHOD_START_INDEX));
        let end = std::ptr::read_volatile(base.add(QUICK_JNI_METHOD_END_INDEX));
        if !crate::jsapi::module::is_in_libart(start) || !crate::jsapi::module::is_in_libart(end) {
            continue;
        }
        if off < 16 {
            continue;
        }
        let prev0 = std::ptr::read_volatile((thread + off - 16) as *const u64);
        let prev1 = std::ptr::read_volatile((thread + off - 8) as *const u64);
        if !crate::jsapi::module::is_in_libart(prev0) || !crate::jsapi::module::is_in_libart(prev1) {
            continue;
        }

        let mut libart_ptrs = 0usize;
        for i in 0..QUICK_ENTRYPOINT_COUNT {
            if crate::jsapi::module::is_in_libart(std::ptr::read_volatile(base.add(i))) {
                libart_ptrs += 1;
            }
        }
        if libart_ptrs < QUICK_MIN_LIBART_POINTERS {
            continue;
        }

        QUICK_ENTRYPOINTS_OFFSET.store(off + 1, Ordering::Release);
        let entry = std::ptr::read_volatile(base.add(index));
        return crate::jsapi::module::is_in_libart(entry).then_some(entry);
    }

    QUICK_ENTRYPOINTS_OFFSET.store(QUICK_ENTRYPOINTS_OFFSET_FAILED, Ordering::Release);
    None
}

#[cfg(target_arch = "aarch64")]
unsafe fn call_quick_alloc_object(entry: usize, thread: usize, klass: usize) -> usize {
    let mut ret = klass;
    core::arch::asm!(
        "str x19, [sp, #-16]!",
        "mov x19, x10",
        "blr x11",
        "ldr x19, [sp], #16",
        in("x10") thread,
        in("x11") entry,
        inlateout("x0") ret,
        clobber_abi("C"),
    );
    ret
}

#[cfg(not(target_arch = "aarch64"))]
unsafe fn call_quick_alloc_object(entry: usize, _thread: usize, klass: usize) -> usize {
    let f: unsafe extern "C" fn(usize) -> usize = std::mem::transmute(entry);
    f(klass)
}
