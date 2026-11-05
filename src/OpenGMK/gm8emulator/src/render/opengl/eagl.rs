//! OpenGL ES context for iOS, through EAGL.
//!
//! iOS has no EGL. The context, the layer and the framebuffer built on it are created by the app
//! (`ios/host/main.m`), because they are Objective-C objects; this module only calls into it. It
//! offers the same functions as `egl.rs`, under the same module name, so the renderer and the game
//! loop do not need to know which phone they run on.
//!
//! Two differences from Android matter:
//! - There is no framebuffer 0. The screen is a framebuffer the app made on top of the layer, and
//!   [screen_framebuffer] is its name.
//! - A backgrounded app that touches OpenGL ES is killed by the system. [suspend] waits for the
//!   frame in flight and then reports no surface, so the game loop stops drawing before iOS takes
//!   the screen away.

use std::{
    ffi::{c_void, CString},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use ramen::{connection::Connection, window::Window};

extern "C" {
    /// Creates the ES 3 context on the calling thread, makes it current and builds the screen
    /// framebuffer on `layer`. Returns false if any of that failed.
    fn rl_host_gl_create(layer: *mut c_void) -> bool;
    /// Rebuilds the screen framebuffer's storage at the layer's current size.
    fn rl_host_gl_update_drawable() -> bool;
    /// The name of the screen framebuffer.
    fn rl_host_gl_framebuffer() -> u32;
    /// Shows the frame drawn into the screen framebuffer.
    fn rl_host_gl_present() -> bool;
    /// Releases the context and everything built on it.
    fn rl_host_gl_destroy();
}

/// Whether the context exists.
static CREATED: AtomicBool = AtomicBool::new(false);
/// Whether the app is in the foreground. iOS forbids GL work otherwise.
static ACTIVE: AtomicBool = AtomicBool::new(true);
/// Set when the layer changed size; the render thread rebuilds the storage on its next frame.
static RESIZE_PENDING: AtomicBool = AtomicBool::new(false);
/// Cached name of the screen framebuffer.
static SCREEN_FBO: AtomicU32 = AtomicU32::new(0);
static SWAP_INTERVAL: AtomicU32 = AtomicU32::new(1);

extern "C" fn no_op_fn() {}

/// Entry points stubbed out because GLES doesn't have them.
static MISSING_FNS: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());

thread_local! {
    /// Reused so the loader's C strings outlive the call that made them.
    static NAME_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The stubbed names, for diagnostics.
pub fn stubbed_functions() -> Vec<String> {
    MISSING_FNS.lock().unwrap().iter().map(|s| s.to_string()).collect()
}

/// Prints the stubbed entry points once, so they show up in the log without flooding it.
pub fn log_stubbed_once() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let list = stubbed_functions();
        if !list.is_empty() {
            eprintln!("[gl] ausentes, sustituidas por no-ops: {}", list.join(", "));
        }
    });
}

/// EAGL does not lose contexts the way EGL can; kept for the game loop, which asks.
pub fn context_lost() -> bool {
    false
}

/// Held for the duration of every frame, so [suspend] can wait for the frame in flight.
static FRAME: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Shared guard to hold while a frame is being drawn.
pub fn frame_guard() -> Option<std::sync::RwLockReadGuard<'static, ()>> {
    FRAME.read().ok()
}

/// Whether there is a surface to draw to right now.
pub fn has_surface() -> bool {
    CREATED.load(Ordering::SeqCst) && ACTIVE.load(Ordering::SeqCst)
}

/// The framebuffer that reaches the screen.
pub fn screen_framebuffer() -> u32 {
    SCREEN_FBO.load(Ordering::Relaxed)
}

/// Called from the main thread when the app leaves the foreground. Waits (bounded) for the frame in
/// flight, so that no GL call is made once this returns. iOS kills apps that keep drawing after
/// that point.
pub fn suspend() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    loop {
        if let Ok(_no_frame) = FRAME.try_write() {
            ACTIVE.store(false, Ordering::SeqCst);
            return
        }
        if std::time::Instant::now() >= deadline {
            // A frame stuck for that long is stuck in the driver; stop the next ones anyway.
            ACTIVE.store(false, Ordering::SeqCst);
            eprintln!("[gl] un frame no termino a tiempo al pasar a segundo plano");
            return
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Called from the main thread when the app is back in the foreground.
pub fn resume() {
    ACTIVE.store(true, Ordering::SeqCst);
}

/// Called from the main thread when the layer changes size.
pub fn request_resize() {
    RESIZE_PENDING.store(true, Ordering::SeqCst);
}

/// Called from the render thread at the start of every frame. EAGL keeps the context current on
/// the thread that made it, so the only thing that can need doing is following a size change.
pub unsafe fn bind_surface() {
    if !has_surface() {
        return
    }
    if RESIZE_PENDING.swap(false, Ordering::SeqCst) {
        if !rl_host_gl_update_drawable() {
            eprintln!("[gl] no pude ajustar el framebuffer al nuevo tamano de pantalla");
        }
        SCREEN_FBO.store(rl_host_gl_framebuffer(), Ordering::Relaxed);
    }
}

/// Handle to the context the app owns. Carries nothing itself.
pub struct PlatformImpl;

impl PlatformImpl {
    pub unsafe fn new(connection: &Connection, window: &Window) -> Result<Self, String> {
        let _ = connection;
        let layer = window.xid();
        if layer.is_null() {
            return Err("iOS no ha entregado la capa de dibujo".into())
        }
        if !rl_host_gl_create(layer) {
            return Err("no se pudo crear el contexto OpenGL ES 3 (EAGL)".into())
        }
        SCREEN_FBO.store(rl_host_gl_framebuffer(), Ordering::Relaxed);
        RESIZE_PENDING.store(false, Ordering::SeqCst);
        CREATED.store(true, Ordering::SeqCst);
        Ok(Self)
    }

    pub unsafe fn get_function_loader() -> Result<Box<dyn FnMut(&'static str) -> *const c_void>, String> {
        // The framework is linked into the app, so its symbols are already loaded; opening it again
        // only takes a reference, and gives a handle that limits the lookup to GL itself.
        let path = CString::new("/System/Library/Frameworks/OpenGLES.framework/OpenGLES").unwrap();
        let gles = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);

        Ok(Box::new(move |name: &'static str| {
            NAME_BUF.with(|buf| {
                let mut buf = buf.borrow_mut();
                buf.clear();
                buf.extend_from_slice(name.as_bytes());
                buf.push(0);

                let mut ptr: *const c_void = std::ptr::null();
                if !gles.is_null() {
                    ptr = libc::dlsym(gles, buf.as_ptr().cast()) as *const c_void;
                }
                if ptr.is_null() {
                    ptr = libc::dlsym(libc::RTLD_DEFAULT, buf.as_ptr().cast()) as *const c_void;
                }
                if ptr.is_null() {
                    // Desktop-only entry points that GLES doesn't provide. Rather than let
                    // gl_generator's fallback abort the game, point at a no-op and remember
                    // which ones were stubbed so they can be reported.
                    MISSING_FNS.lock().unwrap().push(name);
                    ptr = no_op_fn as *const c_void;
                }
                ptr
            })
        }))
    }

    pub unsafe fn clean_function_loader() {}

    pub unsafe fn swap_buffers(&self) {
        if has_surface() && !rl_host_gl_present() {
            eprintln!("[gl] presentRenderbuffer fallo");
        }
    }

    /// EAGL has no swap interval: presenting already waits for the display when it has to.
    pub unsafe fn set_swap_interval(&self, n: u32) -> bool {
        SWAP_INTERVAL.store(n, Ordering::Relaxed);
        true
    }

    pub unsafe fn get_swap_interval(&self) -> u32 {
        SWAP_INTERVAL.load(Ordering::Relaxed)
    }

    pub unsafe fn wait_vsync(&self) {}
}

impl Drop for PlatformImpl {
    fn drop(&mut self) {
        CREATED.store(false, Ordering::SeqCst);
        unsafe { rl_host_gl_destroy() };
        SCREEN_FBO.store(0, Ordering::Relaxed);
    }
}
