#[cfg(windows)]
fn main() {
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/cursorlink.ico");
    res.compile().expect("Windows 리소스 컴파일 실패");
}

#[cfg(not(windows))]
fn main() {}
