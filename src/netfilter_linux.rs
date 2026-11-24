//! Network-level blocking on Linux via WebKitGTK's content filters
//! (`WebKitUserContentFilterStore`, Safari content-blocker JSON).
//!
//! Compiled filters are cached on disk keyed by the filter-list fingerprint,
//! so the (slow) WebKit compile only happens when lists change.
//! The webkit2gtk crate does not wrap these APIs, so this uses the -sys FFI.

use std::ffi::{c_void, CStr, CString};
use std::path::Path;
use std::ptr;

use gio_sys::GAsyncResult;
use glib::translate::ToGlibPtr;
use gobject_sys::GObject;
use webkit2gtk::WebViewExt;
use webkit2gtk_sys as wk;

struct Pending {
    store: *mut wk::WebKitUserContentFilterStore,
    manager: *mut wk::WebKitUserContentManager,
    identifier: CString,
    store_dir: std::path::PathBuf,
    make_rules: Option<Box<dyn FnOnce() -> Option<String>>>,
    done: Option<Box<dyn FnOnce()>>,
}

impl Pending {
    fn finish(mut self: Box<Self>, filter: *mut wk::WebKitUserContentFilter) {
        unsafe {
            if !filter.is_null() {
                wk::webkit_user_content_manager_add_filter(self.manager, filter);
                wk::webkit_user_content_filter_unref(filter);
                log::info!("network filter active ({})", self.identifier.to_string_lossy());
            }
            gobject_sys::g_object_unref(self.store as *mut GObject);
            gobject_sys::g_object_unref(self.manager as *mut GObject);
        }
        if let Some(done) = self.done.take() {
            done();
        }
    }
}

unsafe fn take_error(err: *mut glib_sys::GError) -> String {
    if err.is_null() {
        return "unknown error".into();
    }
    let msg = CStr::from_ptr((*err).message).to_string_lossy().into_owned();
    glib_sys::g_error_free(err);
    msg
}

unsafe extern "C" fn on_saved(_src: *mut GObject, res: *mut GAsyncResult, data: *mut c_void) {
    let p = Box::from_raw(data as *mut Pending);
    let mut err = ptr::null_mut();
    let filter = wk::webkit_user_content_filter_store_save_finish(p.store, res, &mut err);
    if filter.is_null() {
        log::warn!("WebKit could not compile network rules: {}", take_error(err));
    }
    p.finish(filter);
}

unsafe extern "C" fn on_loaded(_src: *mut GObject, res: *mut GAsyncResult, data: *mut c_void) {
    let mut p = Box::from_raw(data as *mut Pending);
    let mut err = ptr::null_mut();
    let filter = wk::webkit_user_content_filter_store_load_finish(p.store, res, &mut err);
    if !filter.is_null() {
        p.finish(filter);
        return;
    }
    if !err.is_null() {
        glib_sys::g_error_free(err);
    }

    // Not compiled yet for this fingerprint: drop stale compiled lists and compile.
    if let Ok(entries) = std::fs::read_dir(&p.store_dir) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let json = p.make_rules.take().and_then(|f| f());
    let Some(json) = json else {
        p.finish(ptr::null_mut());
        return;
    };
    log::info!("compiling network rules for WebKit (first run after a list update)…");
    let bytes = glib_sys::g_bytes_new(json.as_ptr() as *const c_void, json.len());
    let (store, ident) = (p.store, p.identifier.as_ptr());
    wk::webkit_user_content_filter_store_save(
        store,
        ident,
        bytes,
        ptr::null_mut(),
        Some(on_saved),
        Box::into_raw(p) as *mut c_void,
    );
    glib_sys::g_bytes_unref(bytes);
}

/// Attaches the content filter identified by `fingerprint` to `webview`,
/// compiling it with `make_rules` if it is not cached. `done` runs on the GTK
/// main thread once the filter is active (or failed). Must be called on the
/// GTK main thread.
pub fn install(
    webview: &webkit2gtk::WebView,
    store_dir: &Path,
    fingerprint: &str,
    make_rules: impl FnOnce() -> Option<String> + 'static,
    done: impl FnOnce() + 'static,
) {
    let Some(manager) = webview.user_content_manager() else {
        log::warn!("webview has no user content manager; network filtering disabled");
        done();
        return;
    };
    let _ = std::fs::create_dir_all(store_dir);
    let path = CString::new(store_dir.to_string_lossy().as_bytes()).unwrap_or_default();
    unsafe {
        let store = wk::webkit_user_content_filter_store_new(path.as_ptr());
        let manager_ptr: *mut wk::WebKitUserContentManager = manager.to_glib_full();
        let pending = Box::new(Pending {
            store,
            manager: manager_ptr,
            identifier: CString::new(format!("tubedesk-{fingerprint}")).unwrap(),
            store_dir: store_dir.to_path_buf(),
            make_rules: Some(Box::new(make_rules)),
            done: Some(Box::new(done)),
        });
        let ident = pending.identifier.as_ptr();
        wk::webkit_user_content_filter_store_load(
            store,
            ident,
            ptr::null_mut(),
            Some(on_loaded),
            Box::into_raw(pending) as *mut c_void,
        );
    }
}
