// generate C header when capi feature is enabled

fn main() {
    #[cfg(feature = "capi")]
    {
        generate_c_header();
    }
}

#[cfg(feature = "capi")]
fn generate_c_header() {
    use std::env;
    use std::path::PathBuf;

    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let output_file = PathBuf::from(&crate_dir).join("pravaha.h");

    println!("cargo:rerun-if-changed=src/ffi.rs");
    println!("cargo:rerun-if-changed=src/core.rs");

    cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_language(cbindgen::Language::C)
        .with_include_guard("PRAVAHA_H")
        .with_pragma_once(true)
        .with_documentation(true)
        .with_namespace("pravaha")
        .with_parse_deps(true)
        .with_parse_include(&["libc"])
        .rename_item("PravahaErrorCode", "pravaha_error_code_t")
        .rename_item("PravahaFilesystem", "pravaha_filesystem_t")
        .rename_item("PravahaFile", "pravaha_file_t")
        .with_header(
            "/**\n\
             * Pravaha C API\n\
             * \n\
             * A library for reading HTTP(S) files with chunking, caching, and prefetching.\n\
             * \n\
             * Basic usage:\n\
             * \n\
             *     pravaha_file_t* file = pravaha_open_url(\"https://example.com/data.bin\", \"r\");\n\
             *     if (!file) {\n\
             *         fprintf(stderr, \"Error: %s\\n\", pravaha_last_error());\n\
             *         return 1;\n\
             *     }\n\
             *     \n\
             *     char buffer[1024];\n\
             *     ssize_t n = pravaha_read(file, buffer, sizeof(buffer));\n\
             *     \n\
             *     pravaha_file_close(file);\n\
             * \n\
             * All functions are thread-safe for their error reporting (thread-local storage).\n\
             * Filesystem handles can be shared between threads.\n\
             * File handles should not be used from multiple threads simultaneously.\n\
             */",
        )
        .with_after_include(
            "#include <stdint.h>\n\
             #include <stddef.h>\n\
             \n\
             #ifdef _WIN32\n\
             typedef intptr_t ssize_t;\n\
             #else\n\
             #include <sys/types.h>\n\
             #endif\n\
             \n\
             #ifdef __cplusplus\n\
             extern \"C\" {\n\
             #endif",
        )
        .with_trailer(
            "#ifdef __cplusplus\n\
             }\n\
             #endif",
        )
        .generate()
        .expect("Unable to generate C bindings")
        .write_to_file(&output_file);

    println!("cargo:warning=Generated C header: {}", output_file.display());
}
