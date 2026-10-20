use gpui_kit::component::button::*;
use gpui_kit::*;

struct Root;

impl Render for Root {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(Button::new("hello").primary().label("fastgrid"))
    }
}

fn main() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        if let Err(err) =
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Root))
        {
            eprintln!("failed to open window: {err}");
            cx.quit();
        }
    });
}
