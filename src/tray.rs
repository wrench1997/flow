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
                shutdown.request();
                let _ = sender.send(Action::Exit);
                context.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
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
