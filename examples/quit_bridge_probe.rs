//! Run `cargo run --example quit_bridge_probe` on macOS. No Waku data or daemon.
#[cfg(target_os = "macos")]
#[path = "../src/quit.rs"]
mod quit;

#[cfg(target_os = "macos")]
static SAVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(target_os = "macos")]
mod app {
    use gpui::{Context, IntoElement, Render, Window, div};

    pub struct Waku(pub usize);

    impl Waku {
        pub fn request_quit(&mut self, cx: &mut Context<Self>) {
            self.0 += 1;
            eprintln!("REQUEST {}", self.0);
            match self.0 {
                1 => {
                    eprintln!("SAVE_FAILED");
                    crate::quit::complete(false, cx);
                    cx.spawn(async move |_, _| {
                        smol::Timer::after(std::time::Duration::from_millis(100)).await;
                        crate::terminate();
                    })
                    .detach();
                }
                2 => {
                    eprintln!("SAVE_SUCCEEDED");
                    crate::SAVED.store(true, std::sync::atomic::Ordering::SeqCst);
                    crate::quit::complete(true, cx);
                }
                _ => panic!("unexpected extra quit request"),
            }
        }
    }

    impl Render for Waku {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }
}

#[cfg(target_os = "macos")]
fn terminate() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;

    eprintln!("TERMINATE_ENTER");
    let mtm = MainThreadMarker::new().expect("foreground quit runs on the main thread");
    NSApplication::sharedApplication(mtm).terminate(None);
    eprintln!("TERMINATE_RETURNED");
}

#[cfg(target_os = "macos")]
fn main() {
    use gpui::{App, AppContext, WindowOptions};

    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(5));
        eprintln!("WATCHDOG: quit did not complete");
        std::process::exit(1);
    });
    gpui_platform::application().run(|cx: &mut App| {
        let window = cx
            .open_window(
                WindowOptions {
                    show: false,
                    focus: false,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| app::Waku(0)),
            )
            .unwrap();
        quit::install(window, cx);
        cx.on_app_quit(|_| {
            assert!(SAVED.load(std::sync::atomic::Ordering::SeqCst));
            eprintln!("QUIT_CONFIRMED_AFTER_RETRY");
            async {}
        })
        .detach();
        eprintln!("INSTALLED");
        cx.spawn(async move |_| {
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
            terminate();
        })
        .detach();
    });
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("quit_bridge_probe requires macOS");
    std::process::exit(1);
}
