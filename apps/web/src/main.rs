#[cfg(feature = "ssr")]
fn main() {
    dioxus::launch(openpool_web::App);
}

#[cfg(not(feature = "ssr"))]
fn main() {
    dioxus::launch(openpool_web::App);
}
