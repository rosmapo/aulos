//! Desktop and web entry; everything lives in the library (`lib.rs`).

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    aulos_app::run()
}

#[cfg(target_arch = "wasm32")]
fn main() {
    aulos_app::run_web();
}
