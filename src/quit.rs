//! Pre-termination barrier. GPUI quit hooks run after windows close and have a
//! 200 ms budget, so durable shutdown must complete before AppKit accepts quit.
use crate::app::Waku;
use gpui::{App, WindowHandle};

pub fn install(window: WindowHandle<Waku>, cx: &mut App) {
    #[cfg(target_os = "macos")]
    {
        let (sender, requests) = smol::channel::bounded(1);
        native::install(sender);
        cx.spawn(async move |cx| {
            while requests.recv().await.is_ok() {
                if window
                    .update(cx, |waku, _, cx| waku.request_quit(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window.update(cx, |_, native_window, cx| {
            let entity = cx.entity().downgrade();
            native_window.on_window_should_close(cx, move |_, cx| {
                let _ = entity.update(cx, |waku, cx| waku.request_quit(cx));
                false
            });
        });
        cx.on_action(move |_: &crate::Quit, cx| {
            let _ = window.update(cx, |waku, _, cx| waku.request_quit(cx));
        });
    }
}

pub fn complete(success: bool, cx: &mut App) {
    if !success && !cfg!(all(debug_assertions, target_os = "macos")) {
        // Dock/system quit may start while the main window is hidden. Bring
        // the failure and unconfirmed-save boundary back into view.
        for window in cx.windows() {
            let _ = window.update(cx, |_, window, cx| {
                window.activate_window();
                cx.activate(true);
            });
        }
    }
    #[cfg(target_os = "macos")]
    native::complete(success);
    #[cfg(not(target_os = "macos"))]
    if success {
        cx.quit();
    }
    #[cfg(target_os = "macos")]
    let _ = cx;
}

#[cfg(target_os = "macos")]
mod native {
    use objc2::runtime::{AnyClass, AnyObject, Sel};
    use objc2::{MainThreadMarker, msg_send, sel};
    use objc2_app_kit::NSApplication;
    use std::cell::RefCell;
    use std::ffi::{c_char, c_void};
    thread_local! {
        static REQUESTS: RefCell<Option<smol::channel::Sender<()>>> = const { RefCell::new(None) };
    }
    unsafe extern "C" {
        fn class_addMethod(
            class: *const AnyClass,
            selector: Sel,
            implementation: *const c_void,
            types: *const c_char,
        ) -> bool;
    }
    extern "C" fn should_terminate(_: *mut AnyObject, _: Sel, _: *mut AnyObject) -> usize {
        REQUESTS.with(|requests| {
            if let Some(sender) = requests.borrow().as_ref() {
                let _ = sender.try_send(());
            }
        });
        2 // NSTerminateLater: reply only after the owned daemon exits safely.
    }
    pub fn install(sender: smol::channel::Sender<()>) {
        let mtm =
            MainThreadMarker::new().expect("quit bridge must be installed on the main thread");
        REQUESTS.with(|requests| *requests.borrow_mut() = Some(sender));
        let application = NSApplication::sharedApplication(mtm);
        unsafe {
            let delegate: *mut AnyObject = msg_send![&*application, delegate];
            assert!(!delegate.is_null(), "GPUI application delegate is missing");
            assert!(
                class_addMethod(
                    (*delegate).class(),
                    sel!(applicationShouldTerminate:),
                    should_terminate as *const c_void,
                    c"Q@:@".as_ptr()
                ),
                "GPUI already defines a termination barrier"
            );
        }
    }
    pub fn complete(success: bool) {
        let mtm = MainThreadMarker::new().expect("quit reply must run on the main thread");
        NSApplication::sharedApplication(mtm).replyToApplicationShouldTerminate(success);
    }
}
