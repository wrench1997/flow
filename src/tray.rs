use raw_window_handle::HasWindowHandle;
use std::sync::mpsc;
pub enum Action {
    Show,
    Exit,
}
pub struct Tray {
    _icon: tray_icon::TrayIcon,
    show: tray_icon::menu::MenuItem,
    exit: tray_icon::menu::MenuItem,
    english: bool,
    pub events: mpsc::Receiver<Action>,
}
impl Tray {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        shutdown: crate::backend::Shutdown,
    ) -> anyhow::Result<Self> {
        let ctx = &cc.egui_ctx;
        let native = match cc.window_handle()?.as_raw() {
            raw_window_handle::RawWindowHandle::Win32(h) => h.hwnd.get(),
            _ => 0,
        };
        use tray_icon::{
            TrayIconBuilder,
            menu::{Menu, MenuEvent, MenuItem},
        };
        let menu = Menu::new();
        let show = MenuItem::new(crate::i18n::t("显示主界面"), true, None);
        let exit = MenuItem::new(crate::i18n::t("停止下载并退出"), true, None);
        menu.append(&show)?;
        menu.append(&exit)?;
        let (tx, events) = mpsc::channel();
        let context = ctx.clone();
        let sender = tx.clone();
        let show_id = show.id().clone();
        let exit_id = exit.id().clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == show_id {
                wake_window(native);
                let _ = sender.send(Action::Show);
            }
            if event.id == exit_id {
                let _ = sender.send(Action::Exit);
                request_exit(&context, native, &shutdown);
            }
            context.request_repaint();
        }));
        let context = ctx.clone();
        tray_icon::TrayIconEvent::set_event_handler(Some(move |event| {
            if shows_window(&event) {
                wake_window(native);
                let _ = tx.send(Action::Show);
                context.request_repaint();
            }
        }));
        let pixels = eframe::icon_data::from_png_bytes(include_bytes!("../assets/flow-icon.png"))?;
        let icon = tray_icon::Icon::from_rgba(pixels.rgba, pixels.width, pixels.height)?;
        let icon = TrayIconBuilder::new()
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(true)
            .with_tooltip(crate::i18n::t("Flow · 双击显示窗口 / 右键退出"))
            .build()?;
        Ok(Self {
            _icon: icon,
            show,
            exit,
            english: crate::i18n::english(),
            events,
        })
    }
    pub fn refresh_language(&mut self) {
        let english = crate::i18n::english();
        if english != self.english {
            self.show.set_text(crate::i18n::t("显示主界面"));
            self.exit.set_text(crate::i18n::t("停止下载并退出"));
            let _ = self
                ._icon
                .set_tooltip(Some(crate::i18n::t("Flow · 双击显示窗口 / 右键退出")));
            self.english = english;
        }
    }
}
fn request_exit(
    context: &eframe::egui::Context,
    native: isize,
    shutdown: &crate::backend::Shutdown,
) {
    shutdown.request();
    // A hidden/minimized window may never receive another redraw. Wake its
    // native message loop as well as queuing egui's close command.
    #[cfg(windows)]
    if native != 0 {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn ShowWindow(window: isize, command: i32) -> i32;
            fn PostMessageW(window: isize, message: u32, wparam: usize, lparam: isize) -> i32;
        }
        unsafe {
            ShowWindow(native, 4); // SW_SHOWNOACTIVATE: restore without stealing focus.
            PostMessageW(native, 0x0010, 0, 0); // WM_CLOSE
        }
    }
    context.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
    context.request_repaint();
}
fn shows_window(event: &tray_icon::TrayIconEvent) -> bool {
    matches!(
        event,
        tray_icon::TrayIconEvent::DoubleClick {
            button: tray_icon::MouseButton::Left,
            ..
        }
    )
}
#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    #[ignore = "opens isolated desktop windows; run with --ignored --exact"]
    fn shutdown_hidden_and_minimized_windows_without_ui_event_delivery() {
        use eframe::{App, egui};
        use raw_window_handle::HasWindowHandle;
        use std::{
            sync::mpsc,
            thread,
            time::{Duration, Instant},
        };
        use winit::platform::windows::EventLoopBuilderExtWindows;

        struct Smoke {
            app: crate::DownloadApp,
            native: isize,
            minimized: bool,
            started: Option<mpsc::Sender<Instant>>,
        }
        impl App for Smoke {
            fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
                self.app.update(ctx, frame);
                if self.app.connected
                    && let Some(started) = self.started.take()
                {
                    ctx.send_viewport_cmd(if self.minimized {
                        egui::ViewportCommand::Minimized(true)
                    } else {
                        egui::ViewportCommand::Visible(false)
                    });
                    let ctx = ctx.clone();
                    let native = self.native;
                    let shutdown = self.app.shutdown.clone();
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(300));
                        started.send(Instant::now()).unwrap();
                        // Use the tray callback's exit path without sending an
                        // Action::Exit to the app. The out-of-band flag must win
                        // over background-on-close even if UI events stop.
                        super::request_exit(&ctx, native, &shutdown);
                    });
                }
            }
        }

        for minimized in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut settings = crate::settings::Settings::defaults(root.path());
            settings.clipboard_watch = false;
            settings.listen_port = 0;
            std::fs::create_dir_all(root.path().join("data")).unwrap();
            std::fs::write(
                root.path().join("data/settings.json"),
                serde_json::to_vec(&settings).unwrap(),
            )
            .unwrap();
            let (started, requested) = mpsc::channel();
            let (finished, finish) = mpsc::channel();
            let mut watchdog = None;
            let options = eframe::NativeOptions {
                event_loop_builder: Some(Box::new(|builder| {
                    builder.with_any_thread(true);
                })),
                ..Default::default()
            };
            eframe::run_native(
                "Flow exit regression",
                options,
                Box::new(|cc| {
                    let native = match cc.window_handle().unwrap().as_raw() {
                        raw_window_handle::RawWindowHandle::Win32(handle) => handle.hwnd.get(),
                        _ => unreachable!(),
                    };
                    let app = crate::DownloadApp::new(cc, root.path().into());
                    assert!(app.tray.is_some());
                    let shutdown = app.shutdown.clone();
                    let ctx = cc.egui_ctx.clone();
                    watchdog = Some(thread::spawn(move || {
                        if finish.recv_timeout(Duration::from_secs(10)).is_err() {
                            super::request_exit(&ctx, native, &shutdown);
                        }
                    }));
                    Ok(Box::new(Smoke {
                        app,
                        native,
                        minimized,
                        started: Some(started),
                    }))
                }),
            )
            .unwrap();
            let _ = finished.send(());
            watchdog.unwrap().join().unwrap();
            let elapsed = requested
                .try_recv()
                .expect("app did not connect before watchdog exit")
                .elapsed();
            assert!(
                elapsed < Duration::from_secs(3),
                "exit took {elapsed:?}, minimized={minimized}"
            );
            assert_eq!(
                std::fs::read_to_string(root.path().join("data/startup-stage.txt")).unwrap(),
                "引擎已退出"
            );
            println!("minimized={minimized}: native exit and engine cleanup took {elapsed:?}");
        }
    }

    #[test]
    fn only_left_double_click_opens_main_window() {
        use tray_icon::{
            MouseButton, MouseButtonState, Rect, TrayIconEvent, TrayIconId,
            dpi::{PhysicalPosition, PhysicalSize},
        };
        let rect = Rect {
            position: PhysicalPosition::new(0., 0.),
            size: PhysicalSize::new(16, 16),
        };
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            assert_eq!(
                super::shows_window(&TrayIconEvent::DoubleClick {
                    id: TrayIconId::new("test"),
                    position: PhysicalPosition::new(0., 0.),
                    rect,
                    button
                }),
                button == MouseButton::Left
            );
            for button_state in [MouseButtonState::Down, MouseButtonState::Up] {
                assert!(!super::shows_window(&TrayIconEvent::Click {
                    id: TrayIconId::new("test"),
                    position: PhysicalPosition::new(0., 0.),
                    rect,
                    button,
                    button_state
                }));
            }
        }
    }
}
pub(crate) fn wake_window(native: isize) {
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn ShowWindow(window: isize, command: i32) -> i32;
            fn SetForegroundWindow(window: isize) -> i32;
        }
        if native != 0 {
            unsafe {
                ShowWindow(native, 9);
                SetForegroundWindow(native);
            }
        }
    }
}
