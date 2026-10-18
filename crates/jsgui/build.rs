//! Embeds the Windows icon and version metadata into the executable.
//!
//! Without this the binary carries no icon, so a shortcut to it shows the
//! generic one Windows uses for anything it knows nothing about.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "joulehound");
        res.set("FileDescription", "Live GUI for the Joulescope JS220");
        if let Err(e) = res.compile() {
            // Not fatal: the program is identical without it, just plainer,
            // and a cross-build has no resource compiler to call.
            println!("cargo:warning=could not embed the icon: {e}");
        }
    }
}
