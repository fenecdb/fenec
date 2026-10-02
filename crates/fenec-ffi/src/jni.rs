//! The JNI functions the Kotlin library calls (`integrations/kotlin`,
//! `com.fenecdb.FenecNative`), over the same calls as the C ABI.
//!
//! Written by hand over the JNI function table rather than through the
//! `jni` crate: four of its functions are used -- an array's length, a new
//! byte array, and copying bytes out of one and into one -- each found at
//! its fixed place in the table (`jni.h`'s `JNINativeInterface_`).
//!
//! Everything crosses as a `byte[]`. Text goes in as its UTF-8 bytes, which
//! Kotlin's `encodeToByteArray` makes: through a `jstring`, JNI's own
//! "modified UTF-8" writes a character outside the BMP -- an emoji in a
//! title -- as two three-byte halves, which is not UTF-8 and the parser
//! refuses. An answer comes back as one `byte[]`, its first byte the call's
//! code and the rest the answer's JSON (or, for an open, the handle as
//! decimal digits): no exception crosses, and a call costs one array.

use std::ffi::{c_char, c_void, CStr};

type Env = *mut *const *const c_void;
type JArray = *mut c_void;

const GET_ARRAY_LENGTH: usize = 171;
const NEW_BYTE_ARRAY: usize = 176;
const GET_BYTE_ARRAY_REGION: usize = 200;
const SET_BYTE_ARRAY_REGION: usize = 208;

/// The table's function at `at`.
unsafe fn function(env: Env, at: usize) -> *const c_void {
    *(*env).add(at)
}

/// A `byte[]`'s bytes, copied out; empty for a null.
unsafe fn read(env: Env, array: JArray) -> Vec<u8> {
    if array.is_null() {
        return Vec::new();
    }
    let len: unsafe extern "system" fn(Env, JArray) -> i32 =
        std::mem::transmute(function(env, GET_ARRAY_LENGTH));
    let get: unsafe extern "system" fn(Env, JArray, i32, i32, *mut i8) =
        std::mem::transmute(function(env, GET_BYTE_ARRAY_REGION));
    let n = len(env, array).max(0);
    let mut out = vec![0u8; n as usize];
    get(env, array, 0, n, out.as_mut_ptr() as *mut i8);
    out
}

/// A new `byte[]`: the code, then `payload`.
unsafe fn answer(env: Env, code: i32, payload: &[u8]) -> JArray {
    let new: unsafe extern "system" fn(Env, i32) -> JArray =
        std::mem::transmute(function(env, NEW_BYTE_ARRAY));
    let set: unsafe extern "system" fn(Env, JArray, i32, i32, *const i8) =
        std::mem::transmute(function(env, SET_BYTE_ARRAY_REGION));
    let mut bytes = Vec::with_capacity(1 + payload.len());
    bytes.push(code as u8);
    bytes.extend_from_slice(payload);
    let array = new(env, bytes.len() as i32);
    // An allocation the VM refused leaves an OutOfMemoryError pending,
    // which Kotlin throws as the call returns.
    if !array.is_null() {
        set(
            env,
            array,
            0,
            bytes.len() as i32,
            bytes.as_ptr() as *const i8,
        );
    }
    array
}

/// A call's code and the text it wrote, freed here.
unsafe fn taken(env: Env, code: i32, out: *mut c_char) -> JArray {
    if out.is_null() {
        return answer(env, code, &[]);
    }
    let r = answer(env, code, CStr::from_ptr(out).to_bytes());
    crate::fenec_free_string(out);
    r
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_fenecdb_FenecNative_version(
    env: Env,
    _class: *mut c_void,
) -> JArray {
    answer(env, 0, CStr::from_ptr(crate::fenec_version()).to_bytes())
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_fenecdb_FenecNative_open(
    env: Env,
    _class: *mut c_void,
    path: JArray,
    flags: i32,
) -> JArray {
    let path = read(env, path);
    let (mut handle, mut out) = (0u64, std::ptr::null_mut());
    let code = crate::fenec_open(
        path.as_ptr(),
        path.len(),
        flags as u32,
        &mut handle,
        &mut out,
        std::ptr::null_mut(),
    );
    match code {
        0 => answer(env, 0, handle.to_string().as_bytes()),
        _ => taken(env, code, out),
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_fenecdb_FenecNative_openMemory(
    env: Env,
    _class: *mut c_void,
) -> JArray {
    let (mut handle, mut out) = (0u64, std::ptr::null_mut());
    let code = crate::fenec_open_memory(&mut handle, &mut out, std::ptr::null_mut());
    match code {
        0 => answer(env, 0, handle.to_string().as_bytes()),
        _ => taken(env, code, out),
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_fenecdb_FenecNative_query(
    env: Env,
    _class: *mut c_void,
    handle: i64,
    text: JArray,
    params: JArray,
    vectors: JArray,
) -> JArray {
    let (text, params, vectors) = (read(env, text), read(env, params), read(env, vectors));
    let mut out = std::ptr::null_mut();
    let code = crate::fenec_query(
        handle as u64,
        text.as_ptr(),
        text.len(),
        params.as_ptr(),
        params.len(),
        vectors.as_ptr(),
        vectors.len(),
        &mut out,
        std::ptr::null_mut(),
    );
    taken(env, code, out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_fenecdb_FenecNative_changes(
    env: Env,
    _class: *mut c_void,
    handle: i64,
    since: i64,
) -> JArray {
    let mut out = std::ptr::null_mut();
    let code = crate::fenec_changes(
        handle as u64,
        since.max(0) as u64,
        &mut out,
        std::ptr::null_mut(),
    );
    taken(env, code, out)
}

/// `fenec_schema`: a schema declared as FenecQL, planned or applied.
#[no_mangle]
pub unsafe extern "system" fn Java_com_fenecdb_FenecNative_schema(
    env: Env,
    _class: *mut c_void,
    handle: i64,
    request: JArray,
    mode: i32,
) -> JArray {
    let request = read(env, request);
    let mut out = std::ptr::null_mut();
    let code = crate::fenec_schema(
        handle as u64,
        request.as_ptr(),
        request.len(),
        mode as u32,
        &mut out,
        std::ptr::null_mut(),
    );
    taken(env, code, out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_fenecdb_FenecNative_syncStart(
    env: Env,
    _class: *mut c_void,
    handle: i64,
    config: JArray,
) -> JArray {
    let config = read(env, config);
    let mut out = std::ptr::null_mut();
    let code = crate::fenec_sync_start(
        handle as u64,
        config.as_ptr(),
        config.len(),
        &mut out,
        std::ptr::null_mut(),
    );
    taken(env, code, out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_fenecdb_FenecNative_syncFeed(
    env: Env,
    _class: *mut c_void,
    handle: i64,
    kind: i32,
    id: i64,
    status: i32,
    seq: i64,
    bytes: JArray,
) -> JArray {
    let bytes = read(env, bytes);
    let mut out = std::ptr::null_mut();
    let code = crate::fenec_sync_feed(
        handle as u64,
        kind as u32,
        id as u64,
        status,
        seq.max(0) as u64,
        bytes.as_ptr(),
        bytes.len(),
        &mut out,
        std::ptr::null_mut(),
    );
    taken(env, code, out)
}

/// The calls that take a handle alone.
macro_rules! by_handle {
    ($($java:ident => $call:ident),* $(,)?) => {$(
        #[no_mangle]
        pub unsafe extern "system" fn $java(env: Env, _class: *mut c_void, handle: i64) -> JArray {
            let mut out = std::ptr::null_mut();
            let code = crate::$call(handle as u64, &mut out, std::ptr::null_mut());
            taken(env, code, out)
        }
    )*};
}

by_handle! {
    Java_com_fenecdb_FenecNative_close => fenec_close,
    Java_com_fenecdb_FenecNative_sync => fenec_sync,
    Java_com_fenecdb_FenecNative_flush => fenec_flush,
    Java_com_fenecdb_FenecNative_checkpoint => fenec_checkpoint,
    Java_com_fenecdb_FenecNative_syncStatus => fenec_sync_status,
}
