// Generate bindings to the libva *backend* ABI. No link directive: the
// symbols we need are defined in our cdylib and resolved by libva's dlopen.
use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg("-I/usr/include")
        // Allow everything reachable from va*.h to avoid forward-decl opaquing.
        .allowlist_file(".*/va/.*\\.h")
        .derive_default(false)
        .derive_debug(false)
        .derive_copy(false)
        .prepend_enum_name(false)
        .layout_tests(false)
        .generate_comments(false)
        .generate()
        .expect("failed to generate libva backend bindings");

    bindings
        .write_to_file(out_dir.join("va_bindings.rs"))
        .expect("failed to write va_bindings.rs");
}
