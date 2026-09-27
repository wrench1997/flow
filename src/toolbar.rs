use eframe::egui::{self, Stroke, vec2};
#[derive(Clone, Copy)]
pub enum Icon {
    Add,
    Player,
    Exit,
    Resume,
    Pause,
    Check,
    Remove,
    Settings,
    Subscriptions,
    Update,
    Theme,
}
pub fn button(ui: &mut egui::Ui, enabled: bool, icon: Icon, hint: &str) -> egui::Response {
    let hint = crate::i18n::t(hint);
    let response = ui
        .add_enabled(enabled, egui::Button::new("").min_size(vec2(32.0, 30.0)))
        .on_hover_text(&hint)
        .on_disabled_hover_text(&hint);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, &hint));
    let p = ui.painter();
    let center = response.rect.center();
    let stroke = Stroke::new(1.7_f32, ui.style().interact(&response).fg_stroke.color);
    let point = |x: f32, y: f32| center + vec2(x, y);
    let line = |a: (f32, f32), b: (f32, f32)| {
        p.line_segment([point(a.0, a.1), point(b.0, b.1)], stroke);
    };
    let triangle = || {
        p.add(egui::Shape::closed_line(
            vec![point(-4., -7.), point(7., 0.), point(-4., 7.)],
            stroke,
        ));
    };
    match icon {
        Icon::Add => {
            line((-7., 0.), (7., 0.));
            line((0., -7.), (0., 7.));
        }
        Icon::Resume => triangle(),
        Icon::Pause => {
            line((-4., -7.), (-4., 7.));
            line((4., -7.), (4., 7.));
        }
        Icon::Player => {
            p.rect_stroke(
                egui::Rect::from_center_size(center, vec2(22., 18.)),
                3.,
                stroke,
                egui::StrokeKind::Inside,
            );
            triangle();
        }
        Icon::Exit => {
            let points = (0..25)
                .map(|i| {
                    let a = 0.65 + i as f32 * (std::f32::consts::TAU - 1.3) / 24.;
                    point(a.sin() * 8., -a.cos() * 8.)
                })
                .collect();
            p.add(egui::Shape::line(points, stroke));
            line((0., -10.), (0., 0.));
        }
        Icon::Check => {
            p.circle_stroke(center, 9., stroke);
            line((-5., 0.), (-1., 4.));
            line((-1., 4.), (5., -4.));
        }
        Icon::Remove => {
            line((-8., -6.), (8., -6.));
            line((-3., -9.), (3., -9.));
            p.add(egui::Shape::line(
                vec![
                    point(-6., -6.),
                    point(-5., 8.),
                    point(5., 8.),
                    point(6., -6.),
                ],
                stroke,
            ));
            line((-2., -2.), (-2., 5.));
            line((2., -2.), (2., 5.));
        }
        Icon::Settings => {
            p.circle_stroke(center, 6., stroke);
            p.circle_stroke(center, 2., stroke);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::TAU / 8.;
                line((a.cos() * 6., a.sin() * 6.), (a.cos() * 10., a.sin() * 10.));
            }
        }
        Icon::Subscriptions => {
            let origin = point(-7., 7.);
            p.circle_filled(origin, 1.8, stroke.color);
            for radius in [8., 15.] {
                let pts = (0..17)
                    .map(|i| {
                        let a = i as f32 * std::f32::consts::FRAC_PI_2 / 16.;
                        origin + vec2(a.sin() * radius, -a.cos() * radius)
                    })
                    .collect();
                p.add(egui::Shape::line(pts, stroke));
            }
        }
        Icon::Update => {
            line((0., -8.), (0., 4.));
            line((-4., 0.), (0., 4.));
            line((0., 4.), (4., 0.));
            p.add(egui::Shape::line(
                vec![point(-8., 4.), point(-8., 9.), point(8., 9.), point(8., 4.)],
                stroke,
            ));
        }
        Icon::Theme => {
            p.circle_stroke(center, 8., stroke);
            p.add(egui::Shape::convex_polygon(
                (0..17)
                    .map(|i| {
                        let a = std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 16.;
                        point(a.cos() * 8., a.sin() * 8.)
                    })
                    .collect(),
                stroke.color,
                Stroke::NONE,
            ));
        }
    }
    response
}
