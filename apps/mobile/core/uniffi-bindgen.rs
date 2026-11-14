//! Generates the bindings: `cargo run --features bindgen --bin
//! uniffi-bindgen -- generate --library LIBRARY --language kotlin`.

fn main() {
    uniffi::uniffi_bindgen_main()
}
