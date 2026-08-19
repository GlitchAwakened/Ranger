fn main() {
    // The Slint compiler (parsing + lowering of the `.slint` file) is
    // **recursive** over the file's tree. `main_window.slint` is large and
    // deeply nested → on **Windows**, where the main thread only gets a ~1 MB
    // stack (vs ~8 MB on Linux), `slint_build::compile` was overflowing the
    // stack (`STATUS_STACK_OVERFLOW`). We therefore run the compilation on a
    // dedicated large-stack thread (a functional no-op elsewhere: Linux never
    // overflowed).
    let build = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024) // 64 MB: ample margin for the nesting depth
        .spawn(|| {
            slint_build::compile("src/ui/main_window.slint")
                .expect("main_window.slint compilation failed");
        })
        .expect("failed to spawn the Slint compilation thread");
    build.join().expect("the Slint compilation thread panicked");

    // Windows: embeds the app icon into ranger.exe (Explorer, taskbar, alt-tab).
    // Non-blocking — a tooling failure (rc.exe missing) must not break the
    // build; the exe then ships without an embedded icon.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/ranger.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/ranger.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed the icon (ranger.ico): {e}");
        }
    }
}
