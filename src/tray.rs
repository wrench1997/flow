use raw_window_handle::HasWindowHandle;
use std::sync::mpsc;
pub enum Action {
    Show,
    Exit,
}
pub struct Tray {
    _icon: tray_icon::TrayIcon,
    pub events: mpsc::Receiver<Action>,
}
impl Tray {
    pub fn new(cc: &eframe::CreationContext<'_>) -> anyhow::Result<Self> {
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
        let show = MenuItem::new("显示 Flow", true, None);
        let exit = MenuItem::new("退出并停止下载", true, None);
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
                wake_window(native);
                let _ = sender.send(Action::Exit);
            }
            context.request_repaint();
        }));
        let context = ctx.clone();
        tray_icon::TrayIconEvent::set_event_handler(Some(move |event| {
            if matches!(event, tray_icon::TrayIconEvent::DoubleClick { .. }) {
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
            .with_tooltip("Flow · 双击显示窗口 / 右键退出")
            .build()?;
        Ok(Self {
            _icon: icon,
            events,
        })
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
