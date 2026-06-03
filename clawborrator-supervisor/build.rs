// Embeds the Windows application icon into relay.exe.
//
// Runs on every build, but only does work when the HOST is Windows
// (CI builds the Windows artifact natively on a windows runner). On
// macOS/Linux the `#[cfg(windows)]` block is compiled out, so this is a
// no-op and `winresource` isn't even pulled in (it's a Windows-only
// build-dependency). macOS gets its icon from the .app bundle's
// AppIcon.icns instead (see packaging/macos/), and Linux is headless.
fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app-icon.ico");
        res.set("ProductName", "Relay");
        res.set("FileDescription", "Relay");
        // Non-fatal: a missing resource compiler shouldn't break the
        // build outright, but surface it so CI logs show why the exe
        // has no icon.
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed Windows icon: {e}");
        }
    }
}
