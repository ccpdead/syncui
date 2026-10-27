//! Linux Secret Service via the desktop's libsecret. No plaintext fallback.
use std::ffi::{c_char, c_int, c_void, CStr, CString};
type Ptr = *mut c_void;
pub fn access(key: &str, password: Option<&str>, remove: bool) -> Result<Option<String>, String> {
    unsafe {
        let lib = libc::dlopen(c"libsecret-1.so.0".as_ptr(), libc::RTLD_NOW);
        if lib.is_null() {
            return Err("系统密钥环不可用；密码仅用于本次会话".into());
        }
        // Keep libsecret loaded: its DBus callbacks may outlive this call.
        unsafe fn symbol<T: Copy>(lib: Ptr, name: &CStr) -> Result<T, String> {
            let p = libc::dlsym(lib, name.as_ptr());
            if p.is_null() {
                Err("系统密钥环接口不可用".into())
            } else {
                Ok(std::mem::transmute_copy(&p))
            }
        }
        let new: unsafe extern "C" fn(*const c_char, c_int, ...) -> Ptr =
            symbol(lib, c"secret_schema_new")?;
        let unref: unsafe extern "C" fn(Ptr) = symbol(lib, c"secret_schema_unref")?;
        let schema = new(
            c"com.syncui.SSH".as_ptr(),
            0,
            c"profile".as_ptr(),
            0,
            std::ptr::null::<c_char>(),
        );
        if schema.is_null() {
            return Err("无法初始化系统密钥环".into());
        }
        let key = CString::new(key).map_err(|_| "无效凭据 ID")?;
        let mut error: Ptr = std::ptr::null_mut();
        let result = if remove {
            let clear: unsafe extern "C" fn(Ptr, Ptr, *mut Ptr, ...) -> c_int =
                symbol(lib, c"secret_password_clear_sync")?;
            clear(
                schema,
                std::ptr::null_mut(),
                &mut error,
                c"profile".as_ptr(),
                key.as_ptr(),
                std::ptr::null::<c_char>(),
            );
            Ok(None)
        } else if let Some(password) = password {
            let store: unsafe extern "C" fn(
                Ptr,
                *const c_char,
                *const c_char,
                *const c_char,
                Ptr,
                *mut Ptr,
                ...
            ) -> c_int = symbol(lib, c"secret_password_store_sync")?;
            let mut bytes = password.as_bytes().to_vec();
            bytes.push(0);
            if password.contains('\0') {
                unref(schema);
                return Err("密码含无效字符".into());
            }
            let ok = store(
                schema,
                c"default".as_ptr(),
                c"SyncUI SSH".as_ptr(),
                bytes.as_ptr().cast(),
                std::ptr::null_mut(),
                &mut error,
                c"profile".as_ptr(),
                key.as_ptr(),
                std::ptr::null::<c_char>(),
            );
            for b in &mut bytes {
                std::ptr::write_volatile(b, 0)
            }
            if ok == 0 {
                Err("密码未保存：密钥环不可用或已取消解锁".into())
            } else {
                Ok(None)
            }
        } else {
            let lookup: unsafe extern "C" fn(Ptr, Ptr, *mut Ptr, ...) -> *mut c_char =
                symbol(lib, c"secret_password_lookup_sync")?;
            let free: unsafe extern "C" fn(*mut c_char) = symbol(lib, c"secret_password_free")?;
            let p = lookup(
                schema,
                std::ptr::null_mut(),
                &mut error,
                c"profile".as_ptr(),
                key.as_ptr(),
                std::ptr::null::<c_char>(),
            );
            if p.is_null() {
                Ok(None)
            } else {
                let s = CStr::from_ptr(p).to_string_lossy().into_owned();
                free(p);
                Ok(Some(s))
            }
        };
        unref(schema);
        if !error.is_null() {
            let free: unsafe extern "C" fn(Ptr) = symbol(lib, c"g_error_free")?;
            free(error);
            return Err("系统密钥环不可用或已取消解锁；可使用本次会话密码".into());
        }
        result
    }
}
