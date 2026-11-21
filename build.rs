fn main() {
    // Этот код сработает только при сборке под Windows
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("app_icon.ico"); // путь к твоему .ico файлу в корне
        
        // По желанию можно вшить метаданные, которые видны в свойствах .exe
        res.set_language(0x0019); // Русский язык
        res.set("FileDescription", "RustArch — Быстрый архиватор");
        res.set("LegalCopyright", "© 2026 RustArch");
        
        if let Err(e) = res.compile() {
            eprintln!("Ошибка при вшивании иконки: {}", e);
            std::process::exit(1);
        }
    }
}