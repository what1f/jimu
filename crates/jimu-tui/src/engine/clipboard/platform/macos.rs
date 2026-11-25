use std::ffi::{CStr, c_char, c_void};
use std::io;
use std::ptr::NonNull;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use super::{
    ImageData, NativeWriteOutcome, OSASCRIPT_WAIT, get_text_output, run_capture, run_pipe_in,
};

mod attachments;
use attachments::{
    ProbeTemps, RasterClass, image_osascript, osascript_attachments, read_probe_raster,
};

pub(in crate::engine::clipboard) fn get_text() -> io::Result<Option<String>> {
    let (status, data) = run_capture(&["pbpaste", "-Prefer", "txt"], Duration::from_secs(2))?;
    if !status.success() {
        return Err(io::Error::other(format!("pbpaste exited with {status}")));
    }
    Ok(get_text_output(data))
}

pub(in crate::engine::clipboard) fn set_text_with_outcome(text: &str) -> NativeWriteOutcome {
    let cli_ok = run_pipe_in(&["pbcopy"], text.as_bytes(), Duration::from_secs(2)).is_ok();
    NativeWriteOutcome {
        cli_ok,
        ..Default::default()
    }
}

fn run_osascript(script: &str) -> io::Result<Vec<u8>> {
    let (status, data) = run_capture(&["osascript", "-e", script], OSASCRIPT_WAIT)?;
    if status.success() {
        Ok(data)
    } else {
        Err(io::Error::other(format!("osascript exited with {status}")))
    }
}

pub(in crate::engine::clipboard) fn get_attachments()
-> io::Result<(Option<ImageData>, Option<String>)> {
    if let Some(image) = native_image_read() {
        return Ok((Some(image), None));
    }
    osascript_attachments(ProbeTemps::new(), run_osascript)
}

pub(in crate::engine::clipboard) fn get_image() -> io::Result<Option<ImageData>> {
    if let Some(image) = native_image_read() {
        return Ok(Some(image));
    }
    let temps = ProbeTemps::new()?;
    let stdout = run_osascript(&image_osascript(&temps))?;
    match String::from_utf8_lossy(&stdout)
        .trim()
        .parse::<RasterClass>()
    {
        Ok(class) => read_probe_raster(class, &temps),
        Err(()) => Ok(None),
    }
}

fn appkit_loaded() -> bool {
    static LOADED: OnceLock<bool> = OnceLock::new();
    *LOADED.get_or_init(|| {
        let path = c"/System/Library/Frameworks/AppKit.framework/AppKit";
        // SAFETY: the path is NUL-terminated. The framework stays loaded for this process.
        !unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY) }.is_null()
    })
}

pub(in crate::engine::clipboard) fn clipboard_prewarm() {
    let _ = appkit_loaded();
}

pub(in crate::engine::clipboard) fn clipboard_image_probe_supported() -> bool {
    appkit_loaded()
}

type Object = NonNull<c_void>;

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *const c_void;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

unsafe fn send<R>(receiver: Object, selector: &CStr) -> R {
    // SAFETY: the caller selects the result type for this zero-argument method.
    let message: unsafe extern "C" fn(Object, *const c_void) -> R =
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { message(receiver, sel_registerName(selector.as_ptr())) }
}

unsafe fn send_with<A, R>(receiver: Object, selector: &CStr, argument: A) -> R {
    // SAFETY: the caller selects the argument and result types for this one-argument method.
    let message: unsafe extern "C" fn(Object, *const c_void, A) -> R =
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { message(receiver, sel_registerName(selector.as_ptr()), argument) }
}

fn with_pasteboard<T>(read: impl FnOnce(Object) -> Option<T>) -> Option<T> {
    if !appkit_loaded() {
        return None;
    }
    // The bare AppKit load does not initialize NSApplication. Serialize the metadata and data reads.
    static PASTEBOARD_LOCK: Mutex<()> = Mutex::new(());
    let _guard = PASTEBOARD_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    struct Pool(*mut c_void);
    impl Drop for Pool {
        fn drop(&mut self) {
            // SAFETY: this token came from a push on this same thread.
            unsafe { objc_autoreleasePoolPop(self.0) }
        }
    }
    // SAFETY: the pool is popped by its guard before this scope returns.
    let _pool = Pool(unsafe { objc_autoreleasePoolPush() });
    // SAFETY: names a loaded Objective-C class and sends its documented class method.
    let class = NonNull::new(unsafe { objc_getClass(c"NSPasteboard".as_ptr()) })?;
    let pasteboard = NonNull::new(unsafe { send(class, c"generalPasteboard") })?;
    read(pasteboard)
}

fn change_count(pasteboard: Object) -> u64 {
    // SAFETY: NSPasteboard's changeCount returns an NSInteger.
    let count: isize = unsafe { send(pasteboard, c"changeCount") };
    count as u64
}

pub(in crate::engine::clipboard) fn clipboard_change_count() -> Option<u64> {
    with_pasteboard(|pasteboard| Some(change_count(pasteboard)))
}

pub(in crate::engine::clipboard) fn clipboard_image_snapshot() -> (Option<u64>, bool) {
    with_pasteboard(|pasteboard| {
        let count = change_count(pasteboard);
        let has_image =
            advertised_types(pasteboard).is_some_and(|types| native_image_type(&types).is_some());
        Some((Some(count), has_image))
    })
    .unwrap_or((None, false))
}

fn advertised_types(pasteboard: Object) -> Option<Vec<Vec<u8>>> {
    // SAFETY: these selectors read the NSArray of NSStrings while the autorelease pool is active.
    let types = NonNull::new(unsafe { send(pasteboard, c"types") })?;
    let count: usize = unsafe { send(types, c"count") };
    let mut advertised = Vec::with_capacity(count);
    for index in 0..count {
        let Some(ty) = NonNull::new(unsafe { send_with(types, c"objectAtIndex:", index) }) else {
            continue;
        };
        let utf8: *const c_char = unsafe { send(ty, c"UTF8String") };
        if !utf8.is_null() {
            advertised.push(unsafe { CStr::from_ptr(utf8) }.to_bytes().to_vec());
        }
    }
    Some(advertised)
}

const NATIVE_IMAGE_TYPES: &[(&CStr, &str)] = &[
    (c"public.png", "image/png"),
    (c"public.tiff", "image/tiff"),
    (c"public.jpeg", "image/jpeg"),
];

fn native_image_type(types: &[Vec<u8>]) -> Option<(&'static CStr, &'static str)> {
    if types
        .iter()
        .any(|ty| matches!(ty.as_slice(), b"public.file-url" | b"NSFilenamesPboardType"))
    {
        return None;
    }
    NATIVE_IMAGE_TYPES
        .iter()
        .copied()
        .find(|(uti, _)| types.iter().any(|ty| ty == uti.to_bytes()))
}

fn native_image_read() -> Option<ImageData> {
    if std::env::var_os("JIMU_TUI_CLIPBOARD_NO_NATIVE_READ").is_some() {
        return None;
    }
    with_pasteboard(|pasteboard| {
        let types = advertised_types(pasteboard)?;
        let (uti, mime) = native_image_type(&types)?;
        // SAFETY: all selectors use their declared Objective-C ABI. All borrowed objects and
        // NSData's buffer stay valid until the autorelease pool is popped after this closure.
        let data = unsafe {
            let class = NonNull::new(objc_getClass(c"NSString".as_ptr()))?;
            let ty: Object =
                NonNull::new(send_with(class, c"stringWithUTF8String:", uti.as_ptr()))?;
            let data = NonNull::new(send_with(pasteboard, c"dataForType:", ty))?;
            let len: usize = send(data, c"length");
            let bytes: *const u8 = send(data, c"bytes");
            if len == 0 || len > isize::MAX as usize || bytes.is_null() {
                return None;
            }
            std::slice::from_raw_parts(bytes, len).to_vec()
        };
        Some(ImageData {
            data,
            mime_type: mime.to_owned(),
        })
    })
}
