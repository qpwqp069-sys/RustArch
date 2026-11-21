#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use directories::{ProjectDirs, UserDirs};
use eframe::egui::{
    self, Color32, Frame, IconData, Margin, Pos2, Rect, RichText, Rounding, Stroke, Vec2,
};
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use rayon::prelude::*;
use rustfs_mimalloc::MiMalloc;
use serde::{Deserialize, Serialize};
use sevenz_rust2::encoder_options::Lzma2Options;
use sevenz_rust2::{ArchiveEntry, ArchiveWriter};
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const APP_ICON_BYTES: &[u8] = include_bytes!("../app_icon.ico");
const CANCELLED_MARKER: &str = "__RUSTARCH_CANCELLED__";
const ZIP64_THRESHOLD_BYTES: u64 = 3_800_000_000;
const ZIP_PARALLEL_MAX_FILE_SIZE: u64 = 32 * 1024 * 1024;
const ZIP_PARALLEL_BATCH_BYTES: u64 = 128 * 1024 * 1024;

fn main() -> eframe::Result<()> {
    let initial_paths: Vec<PathBuf> = std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("RustArch — Быстрый архиватор")
        .with_inner_size([750.0_f32, 695.0_f32])
        .with_min_inner_size([620.0_f32, 580.0_f32])
        .with_drag_and_drop(true);

    if let Some(icon) = load_embedded_icon() {
        viewport = viewport.with_icon(Arc::new(icon));
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "RustArch — Быстрый архиватор",
        options,
        Box::new(move |cc| Ok(Box::new(ArchiverApp::new(cc, initial_paths)))),
    )
}

fn load_embedded_icon() -> Option<IconData> {
    let icon_dir = ico::IconDir::read(Cursor::new(APP_ICON_BYTES)).ok()?;
    let entry = icon_dir
        .entries()
        .iter()
        .max_by_key(|e| e.width() * e.height())?;
    let image = entry.decode().ok()?;
    Some(IconData {
        rgba: image.rgba_data().to_vec(),
        width: image.width(),
        height: image.height(),
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ThemeMode {
    Dark,
    Light,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum OutputLocation {
    Downloads,
    NextToSource,
    Custom,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SplitPreset {
    None,
    Discord19,
    Mb50,
    Mb100,
    Custom,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ArchiveFormat {
    Zip,
    SevenZ,
    TarZst,
    TarXz,
    TarBr,
    TarGz,
    TarLz4,
    TarBz2, // Только распаковка
}

impl ArchiveFormat {
    const COMPRESSIBLE: [Self; 7] = [
        Self::Zip,
        Self::SevenZ,
        Self::TarZst,
        Self::TarXz,
        Self::TarBr,
        Self::TarGz,
        Self::TarLz4,
    ];

    fn can_compress(self) -> bool {
        !matches!(self, Self::TarBz2)
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Zip => ".zip",
            Self::SevenZ => ".7z",
            Self::TarZst => ".tar.zst",
            Self::TarXz => ".tar.xz",
            Self::TarBr => ".tar.br",
            Self::TarGz => ".tar.gz",
            Self::TarLz4 => ".tar.lz4",
            Self::TarBz2 => ".tar.bz2",
        }
    }

    fn short_label(self) -> &'static str {
        self.extension()
    }

    fn description(self) -> &'static str {
        match self {
            Self::Zip => "Универсальный — откроется на любом ПК и телефоне без программ. Многопоточный zlib-rs + авто-ZIP64 (без лимита 4 ГБ).",
            Self::SevenZ => "Мощное сжатие (многопоточный LZMA2 на sevenz-rust2) — компактный вес для проектов, игр и папок.",
            Self::TarZst => "Современный баланс (Zstandard) — жмёт многопоточно на всех ядрах, очень быстрый и компактный.",
            Self::TarXz => "Чемпион по минимальному весу (сплошной многопоточный LZMA2) — жмёт плотнее всех, открывается в Windows 11 и Linux.",
            Self::TarBr => "Алгоритм Brotli от Google — лучше всех сжимает исходный код, тексты, конфиги, JSON и веб-проекты.",
            Self::TarGz => "Классика для Linux, macOS и серверов — многопоточное сжатие на быстром движке zlib-rs.",
            Self::TarLz4 => "Рекордная скорость SSD — мгновенная упаковка огромных папок, когда время важнее веса.",
            Self::TarBz2 => "Устаревший формат Bzip2 — поддерживается для распаковки готовых архивов из сети.",
        }
    }

    fn from_name(file_name: &str) -> Option<Self> {
        let lower = file_name.to_lowercase();
        if lower.ends_with(".tar.zst") || lower.ends_with(".tzst") {
            Some(Self::TarZst)
        } else if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
            Some(Self::TarXz)
        } else if lower.ends_with(".tar.br") || lower.ends_with(".tbr") {
            Some(Self::TarBr)
        } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            Some(Self::TarGz)
        } else if lower.ends_with(".tar.lz4") || lower.ends_with(".tlz4") {
            Some(Self::TarLz4)
        } else if lower.ends_with(".tar.bz2") || lower.ends_with(".tbz2") || lower.ends_with(".tbz")
        {
            Some(Self::TarBz2)
        } else if lower.ends_with(".zip") {
            Some(Self::Zip)
        } else if lower.ends_with(".7z") {
            Some(Self::SevenZ)
        } else {
            None
        }
    }

    fn from_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_string_lossy();
        Self::from_name(&name)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum CompressionLevel {
    Fast,
    Balanced,
    Maximum,
}

impl CompressionLevel {
    fn label(self) -> &'static str {
        match self {
            Self::Fast => "Быстро",
            Self::Balanced => "Баланс",
            Self::Maximum => "Максимум",
        }
    }

    fn zip_level(self) -> i64 {
        match self {
            Self::Fast => 1,
            Self::Balanced => 6,
            Self::Maximum => 9,
        }
    }

    fn sevenz_level(self) -> u32 {
        match self {
            Self::Fast => 1,
            Self::Balanced => 6,
            Self::Maximum => 9,
        }
    }

    fn xz_level(self) -> u32 {
        match self {
            Self::Fast => 1,
            Self::Balanced => 6,
            Self::Maximum => 9,
        }
    }

    fn brotli_level(self) -> u32 {
        match self {
            Self::Fast => 2,
            Self::Balanced => 6,
            Self::Maximum => 9,
        }
    }

    fn zstd_level(self) -> i32 {
        match self {
            Self::Fast => 1,
            Self::Balanced => 6,
            Self::Maximum => 19,
        }
    }

    fn gzip_level(self) -> u32 {
        match self {
            Self::Fast => 1,
            Self::Balanced => 6,
            Self::Maximum => 9,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct AppConfig {
    theme: ThemeMode,
    output_location: OutputLocation,
    custom_output_dir: Option<PathBuf>,
    default_format: ArchiveFormat,
    default_level: CompressionLevel,
    custom_split_mb: u64,
    overwrite_existing: bool,
    auto_extract_without_confirm: bool,
    smart_skip_media_zip: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: ThemeMode::Dark,
            output_location: OutputLocation::Downloads,
            custom_output_dir: None,
            default_format: ArchiveFormat::TarZst,
            default_level: CompressionLevel::Balanced,
            custom_split_mb: 25,
            overwrite_existing: false,
            auto_extract_without_confirm: false,
            smart_skip_media_zip: true,
        }
    }
}

fn config_file_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "RustArch").map(|p| p.config_dir().join("config.json"))
}

fn load_config() -> AppConfig {
    let Some(path) = config_file_path() else {
        return AppConfig::default();
    };
    let mut cfg: AppConfig = fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    if !cfg.default_format.can_compress() {
        cfg.default_format = ArchiveFormat::TarZst;
    }
    if cfg.custom_split_mb == 0 {
        cfg.custom_split_mb = 25;
    }
    cfg
}

fn save_config(cfg: &AppConfig) {
    let Some(path) = config_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(path, json);
    }
}

fn downloads_dir() -> Option<PathBuf> {
    UserDirs::new().and_then(|u| u.download_dir().map(|p| p.to_path_buf()))
}

// ---------- Интеграция с контекстным меню Windows (HKCU, без прав админа) ----------

#[cfg(windows)]
mod win_shell {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
    use winreg::RegKey;

    const KEYS: [&str; 2] = [
        r"Software\Classes\*\shell\RustArch",
        r"Software\Classes\Directory\shell\RustArch",
    ];

    pub fn is_enabled() -> bool {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        hkcu.open_subkey_with_flags(KEYS[0], KEY_READ).is_ok()
    }

    pub fn needs_path_update() -> bool {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let expected = format!("\"{}\" \"%1\"", exe.display());
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let cmd_key = format!(r"{}\command", KEYS[0]);
        let Ok(key) = hkcu.open_subkey_with_flags(cmd_key, KEY_READ) else {
            return false;
        };
        let current: String = key.get_value("").unwrap_or_default();
        !current.eq_ignore_ascii_case(&expected)
    }

    pub fn set_enabled(enable: bool) -> Result<(), String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if enable {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let exe_str = exe.display().to_string();
            let icon_val = format!("\"{exe_str}\",0");
            let cmd_val = format!("\"{exe_str}\" \"%1\"");

            for base in KEYS {
                let (key, _) = hkcu.create_subkey(base).map_err(|e| e.to_string())?;
                key.set_value("", &"Открыть в RustArch")
                    .map_err(|e| e.to_string())?;
                key.set_value("Icon", &icon_val)
                    .map_err(|e| e.to_string())?;

                let (cmd_key, _) = key.create_subkey("command").map_err(|e| e.to_string())?;
                cmd_key
                    .set_value("", &cmd_val)
                    .map_err(|e| e.to_string())?;
            }
        } else {
            for base in KEYS {
                let _ = hkcu.delete_subkey_all(base);
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod win_shell {
    pub fn is_enabled() -> bool {
        false
    }
    pub fn needs_path_update() -> bool {
        false
    }
    pub fn set_enabled(_enable: bool) -> Result<(), String> {
        Err("Контекстное меню доступно только в Windows.".to_string())
    }
}

#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    card_bg: Color32,
    drop_bg: Color32,
    drop_hover_bg: Color32,
    border: Color32,
    accent: Color32,
    accent_hover: Color32,
    text_primary: Color32,
    text_muted: Color32,
    success: Color32,
    error: Color32,
    bar_track: Color32,
    btn_bg: Color32,
    btn_hover: Color32,
}

impl Palette {
    fn for_theme(theme: ThemeMode) -> Self {
        match theme {
            ThemeMode::Dark => Self {
                bg: Color32::from_rgb(20, 21, 23),
                card_bg: Color32::from_rgb(28, 30, 34),
                drop_bg: Color32::from_rgb(24, 26, 30),
                drop_hover_bg: Color32::from_rgb(41, 31, 25),
                border: Color32::from_rgb(48, 52, 60),
                accent: Color32::from_rgb(217, 107, 39),
                accent_hover: Color32::from_rgb(232, 122, 53),
                text_primary: Color32::from_rgb(236, 238, 242),
                text_muted: Color32::from_rgb(142, 149, 163),
                success: Color32::from_rgb(74, 222, 128),
                error: Color32::from_rgb(248, 113, 113),
                bar_track: Color32::from_rgb(40, 43, 50),
                btn_bg: Color32::from_rgb(38, 41, 48),
                btn_hover: Color32::from_rgb(52, 56, 66),
            },
            ThemeMode::Light => Self {
                bg: Color32::from_rgb(243, 244, 246),
                card_bg: Color32::from_rgb(255, 255, 255),
                drop_bg: Color32::from_rgb(250, 250, 251),
                drop_hover_bg: Color32::from_rgb(255, 244, 236),
                border: Color32::from_rgb(214, 218, 226),
                accent: Color32::from_rgb(200, 90, 23),
                accent_hover: Color32::from_rgb(217, 107, 39),
                text_primary: Color32::from_rgb(24, 26, 31),
                text_muted: Color32::from_rgb(95, 103, 117),
                success: Color32::from_rgb(21, 128, 61),
                error: Color32::from_rgb(220, 38, 38),
                bar_track: Color32::from_rgb(226, 229, 235),
                btn_bg: Color32::from_rgb(236, 238, 242),
                btn_hover: Color32::from_rgb(224, 227, 234),
            },
        }
    }
}

fn apply_theme(ctx: &egui::Context, theme: ThemeMode) {
    let p = Palette::for_theme(theme);
    let mut visuals = match theme {
        ThemeMode::Dark => egui::Visuals::dark(),
        ThemeMode::Light => egui::Visuals::light(),
    };

    visuals.panel_fill = p.bg;
    visuals.window_fill = p.card_bg;
    visuals.window_stroke = Stroke::new(1.0_f32, p.border);
    visuals.window_rounding = Rounding::same(8.0_f32);
    visuals.extreme_bg_color = match theme {
        ThemeMode::Dark => Color32::from_rgb(15, 16, 18),
        ThemeMode::Light => Color32::from_rgb(246, 247, 249),
    };
    visuals.override_text_color = Some(p.text_primary);

    visuals.widgets.inactive.bg_fill = p.btn_bg;
    visuals.widgets.inactive.weak_bg_fill = p.btn_bg;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.border);
    visuals.widgets.inactive.rounding = Rounding::same(6.0_f32);

    visuals.widgets.hovered.bg_fill = p.btn_hover;
    visuals.widgets.hovered.weak_bg_fill = p.btn_hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent);
    visuals.widgets.hovered.rounding = Rounding::same(6.0_f32);

    visuals.widgets.active.bg_fill = p.accent;
    visuals.widgets.active.weak_bg_fill = p.accent;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, p.accent_hover);
    visuals.widgets.active.rounding = Rounding::same(6.0_f32);

    visuals.selection.bg_fill = p.accent;
    visuals.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);

    ctx.set_visuals(visuals);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AppMode {
    Compress,
    Extract,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveView {
    Main,
    Settings,
}

enum WorkerMessage {
    Progress {
        fraction: Option<f32>,
        bytes_done: u64,
        total_bytes: u64,
        speed_mbps: f64,
        status: String,
    },
    Done {
        summary: String,
        result_path: PathBuf,
    },
    Cancelled,
    Error(String),
}

struct FileEntry {
    src_path: PathBuf,
    archive_path: String,
    is_dir: bool,
    size: u64,
}

struct PendingArchive {
    base_path: PathBuf,
    volumes: Vec<PathBuf>,
    format: ArchiveFormat,
}

struct ArchiverApp {
    config: AppConfig,
    view: ActiveView,

    inputs: Vec<PathBuf>,
    mode: AppMode,
    format: ArchiveFormat,
    level: CompressionLevel,
    split_preset: SplitPreset,
    custom_split_input: String,
    output_name: String,

    pending_archive: Option<PendingArchive>,

    context_menu_enabled: bool,
    context_menu_needs_update: bool,
    context_menu_notice: Option<(String, bool)>,

    is_busy: bool,
    cancel_flag: Option<Arc<AtomicBool>>,
    progress: Option<f32>,
    bytes_done: u64,
    total_bytes: u64,
    speed_mbps: f64,
    status_text: String,
    is_error: bool,
    last_result_path: Option<PathBuf>,

    rx: Option<Receiver<WorkerMessage>>,
}

impl ArchiverApp {
    fn new(cc: &eframe::CreationContext<'_>, initial_paths: Vec<PathBuf>) -> Self {
        let config = load_config();
        apply_theme(&cc.egui_ctx, config.theme);

        let context_menu_enabled = win_shell::is_enabled();
        let context_menu_needs_update = context_menu_enabled && win_shell::needs_path_update();

        let mut app = Self {
            format: config.default_format,
            level: config.default_level,
            custom_split_input: config.custom_split_mb.to_string(),
            config,
            view: ActiveView::Main,
            inputs: Vec::new(),
            mode: AppMode::Compress,
            split_preset: SplitPreset::None,
            output_name: String::new(),
            pending_archive: None,
            context_menu_enabled,
            context_menu_needs_update,
            context_menu_notice: None,
            is_busy: false,
            cancel_flag: None,
            progress: None,
            bytes_done: 0,
            total_bytes: 0,
            speed_mbps: 0.0,
            status_text: "Готов к работе. Перетащи файлы, папку или архив.".to_string(),
            is_error: false,
            last_result_path: None,
            rx: None,
        };

        if !initial_paths.is_empty() {
            app.add_paths(initial_paths, true, &cc.egui_ctx);
        }

        app
    }

    fn split_limit_bytes(&self) -> Option<u64> {
        const MB: u64 = 1024 * 1024;
        match self.split_preset {
            SplitPreset::None => None,
            SplitPreset::Discord19 => Some(19 * MB),
            SplitPreset::Mb50 => Some(50 * MB),
            SplitPreset::Mb100 => Some(100 * MB),
            SplitPreset::Custom => self
                .custom_split_input
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|&mb| mb > 0)
                .map(|mb| mb.saturating_mul(MB)),
        }
    }

    fn split_limit_label(&self) -> Option<String> {
        match self.split_preset {
            SplitPreset::None => None,
            SplitPreset::Discord19 => Some("19 МБ".to_string()),
            SplitPreset::Mb50 => Some("50 МБ".to_string()),
            SplitPreset::Mb100 => Some("100 МБ".to_string()),
            SplitPreset::Custom => self
                .custom_split_input
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|&mb| mb > 0)
                .map(|mb| format!("{mb} МБ")),
        }
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>, from_drop: bool, ctx: &egui::Context) {
        if paths.is_empty() || self.is_busy {
            return;
        }

        // Проверяем: либо бросили 1 архив (обычный или часть .001/.002),
        // либо выделили и бросили сразу все части одного многотомного архива.
        let single_archive_candidate = if paths.len() == 1 {
            Some(paths[0].clone())
        } else if all_belong_to_same_split_archive(&paths) {
            Some(paths[0].clone())
        } else {
            None
        };

        if let Some(candidate) = single_archive_candidate {
            match inspect_archive_candidate(&candidate) {
                Ok(Some((base_path, fmt, volumes))) => {
                    if from_drop
                        && self.config.auto_extract_without_confirm
                        && self.inputs.is_empty()
                    {
                        self.setup_extract_and_start(base_path, volumes, fmt, ctx);
                    } else {
                        self.pending_archive = Some(PendingArchive {
                            base_path,
                            volumes,
                            format: fmt,
                        });
                    }
                    return;
                }
                Err(missing_err) => {
                    self.is_error = true;
                    self.status_text = format!("Ошибка: {missing_err}");
                    return;
                }
                Ok(None) => {}
            }
        }

        self.append_for_compress(paths);
    }

    fn setup_extract_and_start(
        &mut self,
        base_path: PathBuf,
        volumes: Vec<PathBuf>,
        fmt: ArchiveFormat,
        ctx: &egui::Context,
    ) {
        self.inputs = volumes;
        self.mode = AppMode::Extract;
        self.format = fmt;
        self.output_name = strip_archive_ext(&base_path);
        self.is_error = false;
        self.last_result_path = None;
        self.view = ActiveView::Main;
        self.start_task(ctx);
    }

    fn append_for_compress(&mut self, paths: Vec<PathBuf>) {
        self.mode = AppMode::Compress;
        if !self.format.can_compress() {
            self.format = self.config.default_format;
        }
        for p in paths {
            if !self.inputs.contains(&p) {
                self.inputs.push(p);
            }
        }
        if self.output_name.trim().is_empty() {
            self.suggest_output_name();
        }
        self.is_error = false;
        self.last_result_path = None;
        self.status_text = format!("Выбрано объектов: {}", self.inputs.len());
    }

    fn suggest_output_name(&mut self) {
        if self.mode == AppMode::Extract && !self.inputs.is_empty() {
            let base = parse_volume_path(&self.inputs[0])
                .map(|v| v.base_path)
                .unwrap_or_else(|| self.inputs[0].clone());
            self.output_name = strip_archive_ext(&base);
        } else if self.inputs.len() == 1 {
            let p = &self.inputs[0];
            if p.is_dir() {
                self.output_name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "archive".to_string());
            } else {
                self.output_name = p
                    .file_stem()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "archive".to_string());
            }
        } else if !self.inputs.is_empty() {
            self.output_name = "archive".to_string();
        } else {
            self.output_name.clear();
        }
    }

    fn resolve_output_dir(&self, fallback_source: Option<&Path>) -> Option<PathBuf> {
        let source_parent = fallback_source
            .or_else(|| self.inputs.first().map(|p| p.as_path()))
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf());

        match self.config.output_location {
            OutputLocation::Downloads => downloads_dir().or(source_parent),
            OutputLocation::NextToSource => source_parent.or_else(downloads_dir),
            OutputLocation::Custom => self
                .config
                .custom_output_dir
                .clone()
                .filter(|p| !p.as_os_str().is_empty())
                .or_else(downloads_dir)
                .or(source_parent),
        }
    }

    fn target_path(&self) -> Option<PathBuf> {
        let dir = self.resolve_output_dir(None)?;
        let clean_name = sanitize_name(&self.output_name);
        if clean_name.is_empty() {
            return None;
        }
        let will_split = self.mode == AppMode::Compress && self.split_limit_bytes().is_some();
        let raw_target = match self.mode {
            AppMode::Compress => {
                let ext = self.format.extension();
                let final_name = if clean_name.to_lowercase().ends_with(ext) {
                    clean_name
                } else {
                    format!("{clean_name}{ext}")
                };
                dir.join(final_name)
            }
            AppMode::Extract => dir.join(clean_name),
        };

        if self.config.overwrite_existing {
            Some(raw_target)
        } else {
            Some(resolve_unique_path(
                &raw_target,
                self.mode == AppMode::Extract,
                will_split,
            ))
        }
    }

    fn start_task(&mut self, ctx: &egui::Context) {
        if self.is_busy || self.inputs.is_empty() {
            return;
        }

        if self.mode == AppMode::Compress {
            if !self.format.can_compress() {
                self.format = self.config.default_format;
            }
            if self.split_preset == SplitPreset::Custom && self.split_limit_bytes().is_none() {
                self.is_error = true;
                self.status_text = "Укажи размер части больше 0 МБ для Split Archive.".to_string();
                return;
            }
        }

        let Some(target) = self.target_path() else {
            self.is_error = true;
            self.status_text = "Укажи корректное имя для результата.".to_string();
            return;
        };

        if let Some(parent) = target.parent() {
            if let Err(err) = fs::create_dir_all(parent) {
                self.is_error = true;
                self.status_text = format!("Не удалось создать папку назначения: {err}");
                return;
            }
        }

        let (tx, rx) = mpsc::channel();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.rx = Some(rx);
        self.cancel_flag = Some(Arc::clone(&cancel_flag));
        self.is_busy = true;
        self.is_error = false;
        self.progress = Some(0.0_f32);
        self.bytes_done = 0;
        self.total_bytes = 0;
        self.speed_mbps = 0.0;
        self.last_result_path = None;
        self.status_text = "Подготовка...".to_string();

        let inputs = self.inputs.clone();
        let mode = self.mode;
        let format = self.format;
        let level = self.level;
        let split_bytes = self.split_limit_bytes();
        let smart_skip = self.config.smart_skip_media_zip;
        let ctx_clone = ctx.clone();

        thread::spawn(move || {
            let started = Instant::now();
            let result = match mode {
                AppMode::Compress => run_compress(
                    &inputs,
                    &target,
                    format,
                    level,
                    split_bytes,
                    smart_skip,
                    Arc::clone(&cancel_flag),
                    tx.clone(),
                    ctx_clone.clone(),
                ),
                AppMode::Extract => run_extract(
                    &inputs,
                    &target,
                    format,
                    Arc::clone(&cancel_flag),
                    tx.clone(),
                    ctx_clone.clone(),
                )
                .map(|summary| (summary, target.clone())),
            };

            match result {
                Ok((detail, final_path)) => {
                    let secs = started.elapsed().as_secs_f64();
                    let _ = tx.send(WorkerMessage::Done {
                        summary: format!("Готово за {:.2} с • {}", secs, detail),
                        result_path: final_path,
                    });
                }
                Err(err) if err.contains(CANCELLED_MARKER) => {
                    if mode == AppMode::Compress {
                        cleanup_compress_artifacts(&target);
                    }
                    let _ = tx.send(WorkerMessage::Cancelled);
                }
                Err(err) => {
                    if mode == AppMode::Compress {
                        cleanup_compress_artifacts(&target);
                    }
                    let _ = tx.send(WorkerMessage::Error(err));
                }
            }
            ctx_clone.request_repaint();
        });
    }

    fn poll_worker(&mut self) {
        let mut finished = false;
        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    WorkerMessage::Progress {
                        fraction,
                        bytes_done,
                        total_bytes,
                        speed_mbps,
                        status,
                    } => {
                        self.progress = fraction.map(|f| f.clamp(0.0_f32, 1.0_f32));
                        self.bytes_done = bytes_done;
                        self.total_bytes = total_bytes;
                        self.speed_mbps = speed_mbps;
                        self.status_text = status;
                    }
                    WorkerMessage::Done {
                        summary,
                        result_path,
                    } => {
                        self.is_busy = false;
                        self.is_error = false;
                        self.progress = None;
                        self.status_text = summary;
                        self.last_result_path = Some(result_path);
                        finished = true;
                    }
                    WorkerMessage::Cancelled => {
                        self.is_busy = false;
                        self.is_error = false;
                        self.progress = None;
                        self.status_text = "Операция отменена пользователем.".to_string();
                        finished = true;
                    }
                    WorkerMessage::Error(err) => {
                        self.is_busy = false;
                        self.is_error = true;
                        self.progress = None;
                        self.status_text = format!("Ошибка: {err}");
                        finished = true;
                    }
                }
            }
        }
        if finished {
            self.rx = None;
            self.cancel_flag = None;
        }
    }

    fn render_top_bar(&mut self, ui: &mut egui::Ui, p: Palette) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("RustArch")
                    .size(22.0_f32)
                    .strong()
                    .color(p.text_primary),
            );
            ui.label(
                RichText::new("v4 Turbo")
                    .size(12.0_f32)
                    .strong()
                    .color(p.accent),
            );
            ui.add_space(8.0_f32);
            let cores = thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            ui.label(
                RichText::new(format!("Потоков CPU: {cores}  •  mimalloc + zlib-rs"))
                    .size(12.0_f32)
                    .color(p.text_muted),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (btn_text, next_view) = match self.view {
                    ActiveView::Main => ("Настройки", ActiveView::Settings),
                    ActiveView::Settings => ("← Назад", ActiveView::Main),
                };

                let btn = egui::Button::new(RichText::new(btn_text).size(13.0_f32))
                    .min_size(Vec2::new(96.0_f32, 30.0_f32))
                    .rounding(Rounding::same(6.0_f32));

                if ui.add_enabled(!self.is_busy, btn).clicked() {
                    if next_view == ActiveView::Settings {
                        self.context_menu_enabled = win_shell::is_enabled();
                        self.context_menu_needs_update =
                            self.context_menu_enabled && win_shell::needs_path_update();
                    }
                    self.view = next_view;
                }
            });
        });
    }

    fn render_settings_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: Palette) {
        let mut changed = false;

        ui.label(
            RichText::new("Настройки приложения")
                .size(17.0_f32)
                .strong()
                .color(p.text_primary),
        );
        ui.add_space(2.0_f32);

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(10.0_f32, 10.0_f32);

            // Тема
            card_frame(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Тема оформления").strong().color(p.text_primary));
                ui.add_space(4.0_f32);
                ui.horizontal(|ui| {
                    if ui
                        .selectable_value(&mut self.config.theme, ThemeMode::Dark, "Тёмная (графит)")
                        .changed()
                    {
                        apply_theme(ctx, self.config.theme);
                        changed = true;
                    }
                    if ui
                        .selectable_value(&mut self.config.theme, ThemeMode::Light, "Светлая")
                        .changed()
                    {
                        apply_theme(ctx, self.config.theme);
                        changed = true;
                    }
                });
            });

            // Папка сохранения
            card_frame(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("Куда сохранять архивы и распакованные папки")
                        .strong()
                        .color(p.text_primary),
                );
                ui.add_space(4.0_f32);

                let dl_label = match downloads_dir() {
                    Some(d) => format!("В папку «Загрузки» ({})", d.display()),
                    None => "В папку «Загрузки»".to_string(),
                };

                if ui
                    .radio_value(
                        &mut self.config.output_location,
                        OutputLocation::Downloads,
                        dl_label,
                    )
                    .changed()
                {
                    changed = true;
                }

                if ui
                    .radio_value(
                        &mut self.config.output_location,
                        OutputLocation::NextToSource,
                        "Рядом с исходными файлами (в ту же папку)",
                    )
                    .changed()
                {
                    changed = true;
                }

                ui.horizontal(|ui| {
                    if ui
                        .radio_value(
                            &mut self.config.output_location,
                            OutputLocation::Custom,
                            "Своя папка:",
                        )
                        .changed()
                    {
                        changed = true;
                    }

                    let custom_text = self
                        .config
                        .custom_output_dir
                        .as_ref()
                        .map(|d| d.display().to_string())
                        .unwrap_or_else(|| "не выбрана".to_string());
                    ui.label(RichText::new(custom_text).size(12.5_f32).color(p.text_muted));

                    if ui.button("Выбрать...").clicked() {
                        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                            self.config.custom_output_dir = Some(folder);
                            self.config.output_location = OutputLocation::Custom;
                            changed = true;
                        }
                    }
                });
            });

            // Интеграция с Windows (Реестр ПКМ)
            card_frame(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("Интеграция с Windows (правая кнопка мыши)")
                        .strong()
                        .color(p.text_primary),
                );
                ui.add_space(4.0_f32);

                let mut toggle = self.context_menu_enabled;
                if ui
                    .checkbox(
                        &mut toggle,
                        "Добавить «Открыть в RustArch» в меню правой кнопки мыши (файлы и папки)",
                    )
                    .changed()
                {
                    match win_shell::set_enabled(toggle) {
                        Ok(()) => {
                            self.context_menu_enabled = toggle;
                            self.context_menu_needs_update = false;
                            let msg = if toggle {
                                "Пункт «Открыть в RustArch» добавлен в меню ПКМ."
                            } else {
                                "Пункт удалён из меню ПКМ."
                            };
                            self.context_menu_notice = Some((msg.to_string(), false));
                        }
                        Err(err) => {
                            self.context_menu_notice =
                                Some((format!("Не удалось обновить реестр: {err}"), true));
                        }
                    }
                }

                if self.context_menu_needs_update {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Путь к RustArch.exe изменился с момента включения.")
                                .size(12.0_f32)
                                .color(p.accent),
                        );
                        if ui.button("Обновить путь").clicked() {
                            match win_shell::set_enabled(true) {
                                Ok(()) => {
                                    self.context_menu_needs_update = false;
                                    self.context_menu_notice = Some((
                                        "Путь в контекстном меню обновлён.".to_string(),
                                        false,
                                    ));
                                }
                                Err(err) => {
                                    self.context_menu_notice =
                                        Some((format!("Ошибка обновления: {err}"), true));
                                }
                            }
                        }
                    });
                }

                if let Some((msg, is_err)) = &self.context_menu_notice {
                    ui.label(
                        RichText::new(msg)
                            .size(12.0_f32)
                            .color(if *is_err { p.error } else { p.success }),
                    );
                }

                ui.label(
                    RichText::new(
                        "Пишется только в ветку текущего пользователя (HKCU) без прав администратора. В Windows 11 пункт доступен через Shift + ПКМ или «Показать дополнительные параметры».",
                    )
                    .size(11.5_f32)
                    .color(p.text_muted),
                );
            });

            // Поведение и производительность
            card_frame(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("Поведение и производительность")
                        .strong()
                        .color(p.text_primary),
                );
                ui.add_space(4.0_f32);

                if ui
                    .checkbox(
                        &mut self.config.auto_extract_without_confirm,
                        "Распаковывать архив сразу при перетаскивании (без окна подтверждения)",
                    )
                    .changed()
                {
                    changed = true;
                }

                if ui
                    .checkbox(
                        &mut self.config.overwrite_existing,
                        "Перезаписывать существующие файлы (если выключено — добавит «(1)» к имени)",
                    )
                    .changed()
                {
                    changed = true;
                }

                if ui
                    .checkbox(
                        &mut self.config.smart_skip_media_zip,
                        "Умный пропуск сжатия в .zip для видео, фото и архивов (в разы быстрее без потери качества)",
                    )
                    .changed()
                {
                    changed = true;
                }

                ui.add_space(4.0_f32);
                ui.label(
                    RichText::new(
                        "Турбо-режим активен: аллокатор mimalloc V3, многопоточное сжатие ZIP/7Z/ZST/XZ/GZ, SIMD zlib-rs, сборка многотомных архивов (.001/.002) на лету и полная автономность без DLL.",
                    )
                    .size(12.0_f32)
                    .color(p.text_muted),
                );
            });

            if let Some(cfg_path) = config_file_path() {
                ui.label(
                    RichText::new(format!("Файл конфигурации: {}", cfg_path.display()))
                        .size(11.5_f32)
                        .color(p.text_muted),
                );
            }
        });

        if changed {
            save_config(&self.config);
        }
    }

    fn render_main_view(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        p: Palette,
        is_hovering: bool,
    ) {
        let drop_bg = if is_hovering {
            p.drop_hover_bg
        } else {
            p.drop_bg
        };
        let drop_border = if is_hovering { p.accent } else { p.border };

        let drop_resp = Frame::default()
            .fill(drop_bg)
            .rounding(Rounding::same(10.0_f32))
            .inner_margin(Margin::same(16.0_f32))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_min_height(126.0_f32);

                ui.vertical_centered(|ui| {
                    ui.add_space(4.0_f32);
                    let title = if is_hovering {
                        "Отпусти, чтобы добавить в RustArch"
                    } else {
                        "Перетащи файлы, папку или архив сюда"
                    };
                    ui.label(
                        RichText::new(title)
                            .size(17.5_f32)
                            .strong()
                            .color(if is_hovering { p.accent } else { p.text_primary }),
                    );
                    ui.add_space(4.0_f32);
                    ui.label(
                        RichText::new(
                            "Сжатие: .zip • .7z • .tar.zst • .tar.xz • .tar.br • .tar.gz • .tar.lz4  |  + .tar.bz2 и части .001",
                        )
                        .size(12.0_f32)
                        .color(p.text_muted),
                    );
                    ui.add_space(12.0_f32);

                    ui.horizontal(|ui| {
                        let buttons_width = 330.0_f32;
                        let pad = ((ui.available_width() - buttons_width) / 2.0_f32).max(0.0_f32);
                        ui.add_space(pad);

                        if ui
                            .add_enabled(
                                !self.is_busy,
                                egui::Button::new(RichText::new("Выбрать файлы").size(13.5_f32))
                                    .min_size(Vec2::new(160.0_f32, 34.0_f32))
                                    .rounding(Rounding::same(6.0_f32)),
                            )
                            .clicked()
                        {
                            if let Some(files) = rfd::FileDialog::new().pick_files() {
                                self.add_paths(files, false, ctx);
                            }
                        }

                        if ui
                            .add_enabled(
                                !self.is_busy,
                                egui::Button::new(RichText::new("Выбрать папку").size(13.5_f32))
                                    .min_size(Vec2::new(160.0_f32, 34.0_f32))
                                    .rounding(Rounding::same(6.0_f32)),
                            )
                            .clicked()
                        {
                            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                                self.add_paths(vec![folder], false, ctx);
                            }
                        }
                    });
                });
            });

        draw_dashed_rect(
            ui.painter(),
            drop_resp.response.rect,
            Stroke::new(if is_hovering { 2.0_f32 } else { 1.25_f32 }, drop_border),
            8.0_f32,
            5.0_f32,
        );

        // Список выбранных объектов
        if !self.inputs.is_empty() {
            card_frame(p).show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mode_badge = match self.mode {
                        AppMode::Compress => format!("Режим: Упаковка ({})", self.inputs.len()),
                        AppMode::Extract if self.inputs.len() > 1 => {
                            format!("Режим: Распаковка (многотомный • частей: {})", self.inputs.len())
                        }
                        AppMode::Extract => "Режим: Распаковка (1)".to_string(),
                    };
                    ui.label(RichText::new(mode_badge).strong().color(p.accent));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(!self.is_busy, egui::Button::new("Очистить"))
                            .clicked()
                        {
                            self.inputs.clear();
                            self.output_name.clear();
                            self.mode = AppMode::Compress;
                            if !self.format.can_compress() {
                                self.format = self.config.default_format;
                            }
                            self.last_result_path = None;
                            self.status_text = "Список очищен.".to_string();
                        }
                    });
                });

                ui.separator();

                let mut remove_idx: Option<usize> = None;
                egui::ScrollArea::vertical()
                    .max_height(88.0_f32)
                    .show(ui, |ui| {
                        for (idx, path) in self.inputs.iter().enumerate() {
                            ui.horizontal(|ui| {
                                let kind = if path.is_dir() { "[Папка]" } else { "[Файл]" };
                                ui.label(RichText::new(kind).size(12.0_f32).color(p.text_muted));
                                let name = path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| path.display().to_string());
                                ui.label(name).on_hover_text(path.display().to_string());

                                if self.mode == AppMode::Compress {
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .add_enabled(
                                                    !self.is_busy,
                                                    egui::Button::new("×").small(),
                                                )
                                                .clicked()
                                            {
                                                remove_idx = Some(idx);
                                            }
                                        },
                                    );
                                }
                            });
                        }
                    });

                if let Some(idx) = remove_idx {
                    self.inputs.remove(idx);
                    if self.inputs.is_empty() {
                        self.output_name.clear();
                        self.mode = AppMode::Compress;
                        if !self.format.can_compress() {
                            self.format = self.config.default_format;
                        }
                    } else {
                        self.suggest_output_name();
                    }
                }
            });
        }

        // Панель параметров задачи
        card_frame(p).show(ui, |ui| {
            ui.set_width(ui.available_width());

            let is_extractable_selection = !self.inputs.is_empty()
                && ((self.inputs.len() == 1
                    && inspect_archive_candidate(&self.inputs[0])
                        .ok()
                        .flatten()
                        .is_some())
                    || all_belong_to_same_split_archive(&self.inputs));

            if is_extractable_selection {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Действие:").strong());
                    let old_mode = self.mode;
                    ui.add_enabled_ui(!self.is_busy, |ui| {
                        ui.selectable_value(&mut self.mode, AppMode::Extract, "Распаковать");
                        ui.selectable_value(&mut self.mode, AppMode::Compress, "Переупаковать");
                    });
                    if old_mode != self.mode {
                        if self.mode == AppMode::Compress && !self.format.can_compress() {
                            self.format = self.config.default_format;
                        }
                        self.suggest_output_name();
                    }
                });
            }

            if self.mode == AppMode::Compress {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Формат:").strong());
                    let old_fmt = self.format;
                    ui.add_enabled_ui(!self.is_busy, |ui| {
                        for fmt in ArchiveFormat::COMPRESSIBLE {
                            ui.selectable_value(&mut self.format, fmt, fmt.short_label());
                        }
                    });
                    if old_fmt != self.format {
                        self.config.default_format = self.format;
                        save_config(&self.config);
                    }
                });

                ui.label(
                    RichText::new(self.format.description())
                        .size(12.0_f32)
                        .color(p.text_muted),
                );

                if self.format != ArchiveFormat::TarLz4 {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Сжатие:").strong());
                        let old_lvl = self.level;
                        ui.add_enabled_ui(!self.is_busy, |ui| {
                            ui.selectable_value(
                                &mut self.level,
                                CompressionLevel::Fast,
                                CompressionLevel::Fast.label(),
                            );
                            ui.selectable_value(
                                &mut self.level,
                                CompressionLevel::Balanced,
                                CompressionLevel::Balanced.label(),
                            );
                            ui.selectable_value(
                                &mut self.level,
                                CompressionLevel::Maximum,
                                CompressionLevel::Maximum.label(),
                            );
                        });
                        if old_lvl != self.level {
                            self.config.default_level = self.level;
                            save_config(&self.config);
                        }
                    });
                }

                // Split Archive — ровно между «Сжатие» и «Имя архива»
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Split Archive:").strong());
                    ui.add_enabled_ui(!self.is_busy, |ui| {
                        ui.selectable_value(&mut self.split_preset, SplitPreset::None, "Нет");
                        ui.selectable_value(
                            &mut self.split_preset,
                            SplitPreset::Discord19,
                            "19 МБ (Discord)",
                        );
                        ui.selectable_value(&mut self.split_preset, SplitPreset::Mb50, "50 МБ");
                        ui.selectable_value(&mut self.split_preset, SplitPreset::Mb100, "100 МБ");
                        ui.selectable_value(&mut self.split_preset, SplitPreset::Custom, "Свой");

                        if self.split_preset == SplitPreset::Custom {
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut self.custom_split_input)
                                    .hint_text("25")
                                    .desired_width(58.0_f32),
                            );
                            if resp.changed() {
                                self.custom_split_input.retain(|c| c.is_ascii_digit());
                                if let Ok(val) = self.custom_split_input.parse::<u64>() {
                                    if val > 0 {
                                        self.config.custom_split_mb = val;
                                        save_config(&self.config);
                                    }
                                }
                            }
                            ui.label(RichText::new("МБ").color(p.text_muted));
                        }
                    });
                });
            }

            ui.horizontal(|ui| {
                let name_label = match self.mode {
                    AppMode::Compress => "Имя архива:",
                    AppMode::Extract => "Папка распаковки:",
                };
                ui.label(RichText::new(name_label).strong());
                ui.add_enabled(
                    !self.is_busy,
                    egui::TextEdit::singleline(&mut self.output_name)
                        .hint_text("archive")
                        .desired_width(270.0_f32),
                );
                if self.mode == AppMode::Compress {
                    let ext_text = if self.split_limit_bytes().is_some() {
                        format!("{}.001", self.format.extension())
                    } else {
                        self.format.extension().to_string()
                    };
                    ui.label(RichText::new(ext_text).strong().color(p.accent));
                }
            });

            let dest_preview = match self.target_path() {
                Some(t) => {
                    if self.mode == AppMode::Compress {
                        if let Some(limit_lbl) = self.split_limit_label() {
                            format!(
                                "Сохранится в: {} (части .001, .002... по {limit_lbl})",
                                t.display()
                            )
                        } else {
                            format!("Сохранится в: {}", t.display())
                        }
                    } else {
                        format!("Сохранится в: {}", t.display())
                    }
                }
                None => match self.resolve_output_dir(None) {
                    Some(dir) => format!("Папка сохранения: {}", dir.display()),
                    None => "Папка сохранения: рядом с исходными файлами".to_string(),
                },
            };
            ui.label(RichText::new(dest_preview).size(12.0_f32).color(p.text_muted));
        });

        // Кнопки действия
        ui.horizontal(|ui| {
            let split_ok = self.mode != AppMode::Compress
                || self.split_preset != SplitPreset::Custom
                || self.split_limit_bytes().is_some();

            let can_run = !self.is_busy
                && !self.inputs.is_empty()
                && !sanitize_name(&self.output_name).is_empty()
                && split_ok;

            if self.is_busy {
                let cancel_btn = egui::Button::new(
                    RichText::new("Отмена")
                        .size(15.0_f32)
                        .strong()
                        .color(Color32::WHITE),
                )
                .fill(p.error)
                .min_size(Vec2::new(150.0_f32, 40.0_f32))
                .rounding(Rounding::same(6.0_f32));

                if ui.add(cancel_btn).clicked() {
                    if let Some(flag) = &self.cancel_flag {
                        flag.store(true, Ordering::Relaxed);
                        self.status_text = "Отмена операции...".to_string();
                    }
                }
            } else {
                let action_text = match self.mode {
                    AppMode::Compress => "Сжать сейчас",
                    AppMode::Extract => "Распаковать сейчас",
                };

                let main_btn = egui::Button::new(
                    RichText::new(action_text)
                        .size(15.0_f32)
                        .strong()
                        .color(if can_run { Color32::WHITE } else { p.text_muted }),
                )
                .fill(if can_run { p.accent } else { p.btn_bg })
                .min_size(Vec2::new(200.0_f32, 40.0_f32))
                .rounding(Rounding::same(6.0_f32));

                if ui.add_enabled(can_run, main_btn).clicked() {
                    self.start_task(ctx);
                }
            }

            if let Some(res_path) = &self.last_result_path {
                if ui
                    .add(
                        egui::Button::new(RichText::new("Показать в проводнике").size(13.5_f32))
                            .min_size(Vec2::new(185.0_f32, 40.0_f32))
                            .rounding(Rounding::same(6.0_f32)),
                    )
                    .clicked()
                {
                    open_in_explorer(res_path);
                }
            }
        });

        // Прогресс-бар — только во время активной операции
        if self.is_busy {
            ui.add_space(4.0_f32);
            draw_slim_progress_bar(ui, self.progress, p);

            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(&self.status_text)
                        .size(12.5_f32)
                        .color(p.text_primary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let metrics = if self.total_bytes > 0 {
                        let pct = self.progress.unwrap_or(0.0_f32) * 100.0_f32;
                        format!(
                            "{:.0}%  •  {} / {}  •  {:.1} МБ/с",
                            pct,
                            format_bytes(self.bytes_done),
                            format_bytes(self.total_bytes),
                            self.speed_mbps
                        )
                    } else if self.bytes_done > 0 {
                        format!(
                            "{}  •  {:.1} МБ/с",
                            format_bytes(self.bytes_done),
                            self.speed_mbps
                        )
                    } else {
                        String::new()
                    };
                    ui.label(RichText::new(metrics).size(12.0_f32).color(p.text_muted));
                });
            });
        } else {
            let status_color = if self.is_error {
                p.error
            } else if self.last_result_path.is_some() {
                p.success
            } else {
                p.text_muted
            };
            ui.label(
                RichText::new(&self.status_text)
                    .size(13.0_f32)
                    .color(status_color),
            );
        }
    }

    fn render_archive_confirm_modal(&mut self, ctx: &egui::Context, p: Palette) {
        let Some(pending) = self.pending_archive.as_ref() else {
            return;
        };

        let base_path = pending.base_path.clone();
        let volumes = pending.volumes.clone();
        let archive_fmt = pending.format;
        let parts_count = volumes.len();
        let file_name = base_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| base_path.display().to_string());
        let extract_preview = self
            .resolve_output_dir(Some(&volumes[0]))
            .map(|d| d.join(strip_archive_ext(&base_path)).display().to_string())
            .unwrap_or_else(|| strip_archive_ext(&base_path));

        let mut action: Option<u8> = None;

        egui::Window::new("Обнаружен архив")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .max_width(440.0_f32)
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = Vec2::new(8.0_f32, 10.0_f32);
                let header = if parts_count > 1 {
                    format!(
                        "Найден многотомный архив ({} • частей: {parts_count}):",
                        archive_fmt.short_label()
                    )
                } else {
                    format!(
                        "Ты добавил готовый архив ({}):",
                        archive_fmt.short_label()
                    )
                };
                ui.label(RichText::new(header).color(p.text_muted));
                ui.label(
                    RichText::new(&file_name)
                        .size(15.0_f32)
                        .strong()
                        .color(p.text_primary),
                );
                ui.label(
                    RichText::new(format!("Распакуется в: {extract_preview}"))
                        .size(12.0_f32)
                        .color(p.text_muted),
                );

                ui.add_space(6.0_f32);
                ui.horizontal(|ui| {
                    let extract_btn = egui::Button::new(
                        RichText::new("Распаковать")
                            .strong()
                            .color(Color32::WHITE),
                    )
                    .fill(p.accent)
                    .min_size(Vec2::new(130.0_f32, 34.0_f32))
                    .rounding(Rounding::same(6.0_f32));

                    if ui.add(extract_btn).clicked() {
                        action = Some(1);
                    }

                    if ui
                        .add(
                            egui::Button::new("Добавить к упаковке")
                                .min_size(Vec2::new(160.0_f32, 34.0_f32))
                                .rounding(Rounding::same(6.0_f32)),
                        )
                        .clicked()
                    {
                        action = Some(2);
                    }

                    if ui
                        .add(
                            egui::Button::new("Отмена")
                                .min_size(Vec2::new(80.0_f32, 34.0_f32))
                                .rounding(Rounding::same(6.0_f32)),
                        )
                        .clicked()
                    {
                        action = Some(3);
                    }
                });
            });

        match action {
            Some(1) => {
                self.pending_archive = None;
                self.setup_extract_and_start(base_path, volumes, archive_fmt, ctx);
            }
            Some(2) => {
                self.pending_archive = None;
                self.append_for_compress(volumes);
            }
            Some(3) => {
                self.pending_archive = None;
            }
            _ => {}
        }
    }
}

impl eframe::App for ArchiverApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker();

        let p = Palette::for_theme(self.config.theme);
        let is_hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        let dropped_paths: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped_paths.is_empty() && self.pending_archive.is_none() {
            self.view = ActiveView::Main;
            self.add_paths(dropped_paths, true, ctx);
        }

        let modal_open = self.pending_archive.is_some();

        egui::CentralPanel::default()
            .frame(Frame::default().fill(p.bg).inner_margin(Margin::same(16.0_f32)))
            .show(ctx, |ui| {
                ui.add_enabled_ui(!modal_open, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(10.0_f32, 10.0_f32);
                    self.render_top_bar(ui, p);
                    ui.add_space(2.0_f32);

                    match self.view {
                        ActiveView::Main => self.render_main_view(ui, ctx, p, is_hovering),
                        ActiveView::Settings => self.render_settings_view(ui, ctx, p),
                    }
                });
            });

        self.render_archive_confirm_modal(ctx, p);
    }
}

fn card_frame(p: Palette) -> Frame {
    Frame::default()
        .fill(p.card_bg)
        .stroke(Stroke::new(1.0_f32, p.border))
        .rounding(Rounding::same(8.0_f32))
        .inner_margin(Margin::same(12.0_f32))
}

fn draw_dashed_rect(
    painter: &egui::Painter,
    rect: Rect,
    stroke: Stroke,
    dash_len: f32,
    gap_len: f32,
) {
    let inset = 4.0_f32;
    let corners = [
        Pos2::new(rect.left() + inset, rect.top()),
        Pos2::new(rect.right() - inset, rect.top()),
        Pos2::new(rect.right(), rect.top() + inset),
        Pos2::new(rect.right(), rect.bottom() - inset),
        Pos2::new(rect.right() - inset, rect.bottom()),
        Pos2::new(rect.left() + inset, rect.bottom()),
        Pos2::new(rect.left(), rect.bottom() - inset),
        Pos2::new(rect.left(), rect.top() + inset),
    ];

    for pair in corners.chunks_exact(2) {
        draw_dashed_line(painter, pair[0], pair[1], stroke, dash_len, gap_len);
    }
}

fn draw_dashed_line(
    painter: &egui::Painter,
    start: Pos2,
    end: Pos2,
    stroke: Stroke,
    dash_len: f32,
    gap_len: f32,
) {
    let delta = end - start;
    let len = delta.length();
    if len <= 0.1_f32 {
        return;
    }
    let dir = delta / len;
    let mut dist = 0.0_f32;
    while dist < len {
        let seg_end = (dist + dash_len).min(len);
        painter.line_segment([start + dir * dist, start + dir * seg_end], stroke);
        dist += dash_len + gap_len;
    }
}

fn draw_slim_progress_bar(ui: &mut egui::Ui, progress: Option<f32>, p: Palette) {
    let height = 7.0_f32;
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), egui::Sense::hover());
    let painter = ui.painter();

    painter.rect_filled(rect, Rounding::same(3.5_f32), p.bar_track);

    if let Some(frac) = progress {
        let fill_w = (rect.width() * frac.clamp(0.0_f32, 1.0_f32)).max(0.0_f32);
        if fill_w > 1.0_f32 {
            let fill_rect = Rect::from_min_size(rect.min, Vec2::new(fill_w, rect.height()));
            painter.rect_filled(fill_rect, Rounding::same(3.5_f32), p.accent);
        }
    } else {
        let t = (ui.input(|i| i.time) % 1.4) as f32 / 1.4_f32;
        let seg_w = rect.width() * 0.28_f32;
        let x = rect.left() + (rect.width() + seg_w) * t - seg_w;
        let left = x.max(rect.left());
        let right = (x + seg_w).min(rect.right());
        if right > left {
            let fill_rect =
                Rect::from_min_max(Pos2::new(left, rect.top()), Pos2::new(right, rect.bottom()));
            painter.rect_filled(fill_rect, Rounding::same(3.5_f32), p.accent);
        }
        ui.ctx().request_repaint();
    }
}

// ---------- Многотомные архивы (.001, .002, .003...) ----------

struct ParsedVolume {
    base_path: PathBuf,
    digits_len: usize,
    format: ArchiveFormat,
}

fn parse_volume_path(path: &Path) -> Option<ParsedVolume> {
    let file_name = path.file_name()?.to_string_lossy();
    let dot_pos = file_name.rfind('.')?;
    let suffix = &file_name[dot_pos + 1..];
    if suffix.len() < 3 || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let part_num: usize = suffix.parse().ok()?;
    if part_num == 0 {
        return None;
    }
    let base_name = &file_name[..dot_pos];
    let format = ArchiveFormat::from_name(base_name)?;
    let base_path = path
        .parent()
        .map(|p| p.join(base_name))
        .unwrap_or_else(|| PathBuf::from(base_name));

    Some(ParsedVolume {
        base_path,
        digits_len: suffix.len(),
        format,
    })
}

fn all_belong_to_same_split_archive(paths: &[PathBuf]) -> bool {
    if paths.len() < 2 {
        return false;
    }
    let Some(first) = parse_volume_path(&paths[0]) else {
        return false;
    };
    paths[1..].iter().all(|p| {
        parse_volume_path(p)
            .map(|v| v.base_path == first.base_path)
            .unwrap_or(false)
    })
}

fn volume_part_path(base_path: &Path, part_num: usize, digits_len: usize) -> PathBuf {
    let mut os = base_path.as_os_str().to_os_string();
    os.push(format!(".{part_num:0width$}", width = digits_len));
    PathBuf::from(os)
}

fn inspect_archive_candidate(
    path: &Path,
) -> Result<Option<(PathBuf, ArchiveFormat, Vec<PathBuf>)>, String> {
    if !path.is_file() {
        return Ok(None);
    }

    if let Some(parsed) = parse_volume_path(path) {
        let parent = parsed
            .base_path
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let base_name = parsed
            .base_path
            .file_name()
            .ok_or_else(|| "Некорректное имя многотомного архива.".to_string())?
            .to_string_lossy()
            .to_string();
        let prefix = format!("{base_name}.");

        let mut found: BTreeMap<usize, PathBuf> = BTreeMap::new();
        if let Ok(read_dir) = fs::read_dir(parent) {
            for entry in read_dir.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.len() == prefix.len() + parsed.digits_len
                    && name[..prefix.len()].eq_ignore_ascii_case(&prefix)
                {
                    let suffix = &name[prefix.len()..];
                    if suffix.chars().all(|c| c.is_ascii_digit()) {
                        if let Ok(num) = suffix.parse::<usize>() {
                            if num >= 1 {
                                found.insert(num, entry.path());
                            }
                        }
                    }
                }
            }
        }

        let max_num = *found.keys().next_back().unwrap_or(&0);
        if max_num == 0 || !found.contains_key(&1) {
            let missing_name = format!(
                "{base_name}.{:0width$}",
                1,
                width = parsed.digits_len
            );
            return Err(format!("Не найдена первая часть архива: {missing_name}"));
        }

        let mut ordered = Vec::with_capacity(max_num);
        for num in 1..=max_num {
            if let Some(p) = found.get(&num) {
                ordered.push(p.clone());
            } else {
                let missing_name = format!(
                    "{base_name}.{:0width$}",
                    num,
                    width = parsed.digits_len
                );
                return Err(format!("Не хватает части архива: {missing_name}"));
            }
        }

        return Ok(Some((parsed.base_path, parsed.format, ordered)));
    }

    if let Some(fmt) = ArchiveFormat::from_path(path) {
        return Ok(Some((path.to_path_buf(), fmt, vec![path.to_path_buf()])));
    }

    Ok(None)
}

struct VolumePart {
    path: PathBuf,
    start: u64,
    len: u64,
}

/// Читает и поддерживает `Seek` поверх одного или нескольких томов (.001, .002, ...)
/// как над единым файлом в памяти — без временных копий на диске.
struct MultiVolumeReader {
    parts: Vec<VolumePart>,
    total_len: u64,
    pos: u64,
    current_idx: Option<usize>,
    current_file: Option<File>,
    current_offset: u64,
}

impl MultiVolumeReader {
    fn open(paths: &[PathBuf]) -> io::Result<Self> {
        let mut parts = Vec::with_capacity(paths.len());
        let mut offset = 0u64;
        for p in paths {
            let len = fs::metadata(p)?.len();
            parts.push(VolumePart {
                path: p.clone(),
                start: offset,
                len,
            });
            offset = offset.saturating_add(len);
        }
        Ok(Self {
            parts,
            total_len: offset,
            pos: 0,
            current_idx: None,
            current_file: None,
            current_offset: 0,
        })
    }

    fn total_len(&self) -> u64 {
        self.total_len
    }

    fn find_part_index(&self, pos: u64) -> Option<usize> {
        self.parts
            .iter()
            .position(|p| pos >= p.start && pos < p.start + p.len)
    }
}

impl Read for MultiVolumeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.total_len {
            return Ok(0);
        }
        let Some(idx) = self.find_part_index(self.pos) else {
            return Ok(0);
        };
        let target_offset = self.pos - self.parts[idx].start;
        let remain_in_part = (self.parts[idx].len - target_offset) as usize;
        let to_read = buf.len().min(remain_in_part);

        if self.current_idx != Some(idx) {
            let mut f = File::open(&self.parts[idx].path)?;
            if target_offset > 0 {
                f.seek(SeekFrom::Start(target_offset))?;
            }
            self.current_file = Some(f);
            self.current_idx = Some(idx);
            self.current_offset = target_offset;
        } else if self.current_offset != target_offset {
            if let Some(f) = self.current_file.as_mut() {
                f.seek(SeekFrom::Start(target_offset))?;
            }
            self.current_offset = target_offset;
        }

        let file = self.current_file.as_mut().unwrap();
        let n = file.read(&mut buf[..to_read])?;
        self.pos += n as u64;
        self.current_offset += n as u64;
        Ok(n)
    }
}

impl Seek for MultiVolumeReader {
    fn seek(&mut self, style: SeekFrom) -> io::Result<u64> {
        let new_pos: i128 = match style {
            SeekFrom::Start(offset) => offset as i128,
            SeekFrom::End(offset) => self.total_len as i128 + offset as i128,
            SeekFrom::Current(offset) => self.pos as i128 + offset as i128,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative position",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

fn temp_archive_path(output_path: &Path) -> PathBuf {
    let mut os = output_path.as_os_str().to_os_string();
    os.push(".rustarch_tmp");
    PathBuf::from(os)
}

fn cleanup_compress_artifacts(output_path: &Path) {
    let _ = fs::remove_file(output_path);
    let _ = fs::remove_file(temp_archive_path(output_path));
    for idx in 1..10_000usize {
        let part = volume_part_path(output_path, idx, 3);
        if part.exists() {
            let _ = fs::remove_file(&part);
        } else {
            break;
        }
    }
}

fn split_file_into_volumes(
    src_path: &Path,
    base_output: &Path,
    part_limit: u64,
    cancel: Arc<AtomicBool>,
    tx: Sender<WorkerMessage>,
    ctx: egui::Context,
) -> Result<(usize, PathBuf), String> {
    let total_size = fs::metadata(src_path).map_err(|e| e.to_string())?.len();
    let total_parts = total_size.div_ceil(part_limit).max(1) as usize;
    let tracker = ProgressTracker::new(total_size, cancel, tx, ctx);

    let mut src = BufReader::with_capacity(
        4 * 1024 * 1024,
        File::open(src_path).map_err(|e| e.to_string())?,
    );
    let mut buf = vec![0u8; 2 * 1024 * 1024];

    let mut part_idx = 0usize;
    let mut remaining_total = total_size;

    while remaining_total > 0 {
        if tracker.is_cancelled() {
            return Err(CANCELLED_MARKER.to_string());
        }
        part_idx += 1;
        let part_path = volume_part_path(base_output, part_idx, 3);
        let part_name = part_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        tracker.set_label(format!(
            "Разбиение на части ({part_idx}/{total_parts}): {part_name}"
        ));

        let out_file = File::create(&part_path).map_err(|e| e.to_string())?;
        let mut dst = BufWriter::with_capacity(2 * 1024 * 1024, out_file);
        let mut to_copy = remaining_total.min(part_limit);

        while to_copy > 0 {
            if tracker.is_cancelled() {
                return Err(CANCELLED_MARKER.to_string());
            }
            let chunk_len = (to_copy as usize).min(buf.len());
            let n = src
                .read(&mut buf[..chunk_len])
                .map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            to_copy -= n as u64;
            remaining_total -= n as u64;
            tracker.add_bytes(n as u64);
        }
        dst.flush().map_err(|e| e.to_string())?;
    }

    drop(src);
    let _ = fs::remove_file(src_path);
    Ok((part_idx, volume_part_path(base_output, 1, 3)))
}

// ---------- Адаптивные буферы, трекер прогресса и отмена ----------

fn adaptive_buffer_size(file_size: u64) -> usize {
    if file_size >= 200 * 1024 * 1024 {
        4 * 1024 * 1024
    } else if file_size >= 16 * 1024 * 1024 {
        512 * 1024
    } else {
        64 * 1024
    }
}

fn is_incompressible_ext(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "mp4"
            | "mkv"
            | "avi"
            | "mov"
            | "webm"
            | "jpg"
            | "jpeg"
            | "png"
            | "webp"
            | "gif"
            | "heic"
            | "mp3"
            | "aac"
            | "ogg"
            | "opus"
            | "flac"
            | "zip"
            | "7z"
            | "rar"
            | "gz"
            | "zst"
            | "lz4"
            | "xz"
            | "bz2"
            | "br"
            | "apk"
            | "jar"
            | "pdf"
    )
}

fn is_safe_relative_entry_path(name: &str) -> bool {
    let p = Path::new(name);
    if p.is_absolute() {
        return false;
    }
    for comp in p.components() {
        match comp {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
            _ => {}
        }
    }
    true
}

struct ProgressTracker {
    bytes_done: AtomicU64,
    total_bytes: u64,
    started: Instant,
    cancel: Arc<AtomicBool>,
    last_emit: Mutex<Instant>,
    label: Mutex<String>,
    tx: Mutex<Sender<WorkerMessage>>,
    ctx: egui::Context,
}

impl ProgressTracker {
    fn new(
        total_bytes: u64,
        cancel: Arc<AtomicBool>,
        tx: Sender<WorkerMessage>,
        ctx: egui::Context,
    ) -> Arc<Self> {
        Arc::new(Self {
            bytes_done: AtomicU64::new(0),
            total_bytes,
            started: Instant::now(),
            cancel,
            last_emit: Mutex::new(Instant::now() - Duration::from_secs(1)),
            label: Mutex::new("Обработка...".to_string()),
            tx: Mutex::new(tx),
            ctx,
        })
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn set_label(&self, text: String) {
        if let Ok(mut lbl) = self.label.lock() {
            *lbl = text;
        }
        self.emit(false);
    }

    fn add_bytes(&self, n: u64) {
        self.bytes_done.fetch_add(n, Ordering::Relaxed);
        self.emit(false);
    }

    fn emit(&self, force: bool) {
        let should_send = if force {
            true
        } else if let Ok(mut last) = self.last_emit.lock() {
            if last.elapsed() >= Duration::from_millis(60) {
                *last = Instant::now();
                true
            } else {
                false
            }
        } else {
            false
        };

        if !should_send {
            return;
        }

        let done = self.bytes_done.load(Ordering::Relaxed);
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let speed_mbps = (done as f64 / (1024.0 * 1024.0)) / elapsed;
        let fraction = if self.total_bytes > 0 {
            Some((done as f64 / self.total_bytes as f64) as f32)
        } else {
            None
        };
        let status = self
            .label
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| "Обработка...".to_string());

        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(WorkerMessage::Progress {
                fraction,
                bytes_done: done,
                total_bytes: self.total_bytes,
                speed_mbps,
                status,
            });
        }
        self.ctx.request_repaint();
    }
}

struct ProgressReader<R> {
    inner: R,
    tracker: Arc<ProgressTracker>,
}

impl<R: Read> ProgressReader<R> {
    fn new(inner: R, tracker: Arc<ProgressTracker>) -> Self {
        Self { inner, tracker }
    }
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.tracker.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, CANCELLED_MARKER));
        }
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.tracker.add_bytes(n as u64);
        }
        Ok(n)
    }
}

fn copy_with_progress<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    buf_size: usize,
    tracker: &ProgressTracker,
) -> Result<(), String> {
    let mut buf = vec![0u8; buf_size];
    loop {
        if tracker.is_cancelled() {
            return Err(CANCELLED_MARKER.to_string());
        }
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        tracker.add_bytes(n as u64);
    }
    Ok(())
}

// ---------- Многопоточный GZIP (RFC 1952, чистый Rust + zlib-rs + rayon) ----------

struct ParallelGzipWriter<W: Write> {
    inner: W,
    level: flate2::Compression,
    chunk_size: usize,
    batch_limit: usize,
    current_chunk: Vec<u8>,
    pending_chunks: Vec<Vec<u8>>,
    wrote_any: bool,
    cancel: Arc<AtomicBool>,
}

impl<W: Write> ParallelGzipWriter<W> {
    fn new(inner: W, level: flate2::Compression, cancel: Arc<AtomicBool>) -> Self {
        let threads = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 32);
        let chunk_size = 1024 * 1024;
        Self {
            inner,
            level,
            chunk_size,
            batch_limit: threads,
            current_chunk: Vec::with_capacity(chunk_size),
            pending_chunks: Vec::with_capacity(threads),
            wrote_any: false,
            cancel,
        }
    }

    fn flush_batch(&mut self) -> io::Result<()> {
        if self.pending_chunks.is_empty() {
            return Ok(());
        }
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, CANCELLED_MARKER));
        }

        let batch = std::mem::take(&mut self.pending_chunks);
        let level = self.level;
        let compressed: io::Result<Vec<Vec<u8>>> = batch
            .into_par_iter()
            .map(|raw| {
                let mut enc = GzEncoder::new(Vec::with_capacity(raw.len() / 2), level);
                enc.write_all(&raw)?;
                enc.finish()
            })
            .collect();

        for member in compressed? {
            self.inner.write_all(&member)?;
            self.wrote_any = true;
        }
        Ok(())
    }

    fn finish(mut self) -> io::Result<W> {
        if !self.current_chunk.is_empty() {
            let tail = std::mem::take(&mut self.current_chunk);
            self.pending_chunks.push(tail);
        }
        self.flush_batch()?;
        if !self.wrote_any {
            let enc = GzEncoder::new(Vec::new(), self.level);
            let empty_member = enc.finish()?;
            self.inner.write_all(&empty_member)?;
        }
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for ParallelGzipWriter<W> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, CANCELLED_MARKER));
        }
        let total = buf.len();
        while !buf.is_empty() {
            let space = self.chunk_size - self.current_chunk.len();
            let take = space.min(buf.len());
            self.current_chunk.extend_from_slice(&buf[..take]);
            buf = &buf[take..];

            if self.current_chunk.len() == self.chunk_size {
                let full =
                    std::mem::replace(&mut self.current_chunk, Vec::with_capacity(self.chunk_size));
                self.pending_chunks.push(full);
                if self.pending_chunks.len() >= self.batch_limit {
                    self.flush_batch()?;
                }
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// ---------- Логика сжатия и распаковки ----------

#[allow(clippy::too_many_arguments)]
fn run_compress(
    inputs: &[PathBuf],
    output_path: &Path,
    format: ArchiveFormat,
    level: CompressionLevel,
    split_bytes: Option<u64>,
    smart_skip_media_zip: bool,
    cancel: Arc<AtomicBool>,
    tx: Sender<WorkerMessage>,
    ctx: egui::Context,
) -> Result<(String, PathBuf), String> {
    let scan_tracker = ProgressTracker::new(0, Arc::clone(&cancel), tx.clone(), ctx.clone());
    scan_tracker.set_label("Сканирование файлов...".to_string());
    scan_tracker.emit(true);

    let entries = collect_entries(inputs, &cancel)?;
    if entries.is_empty() {
        return Err("Нет файлов для сжатия.".to_string());
    }

    let raw_size: u64 = entries.iter().map(|e| e.size).sum();
    let tracker = ProgressTracker::new(raw_size, Arc::clone(&cancel), tx.clone(), ctx.clone());

    let work_path = if split_bytes.is_some() {
        temp_archive_path(output_path)
    } else {
        output_path.to_path_buf()
    };

    match format {
        ArchiveFormat::Zip => {
            compress_zip_parallel(&entries, &work_path, level, smart_skip_media_zip, &tracker)?
        }
        ArchiveFormat::SevenZ => compress_7z(&entries, &work_path, level, &tracker)?,
        ArchiveFormat::TarZst => compress_tar_zst(&entries, &work_path, level, &tracker)?,
        ArchiveFormat::TarXz => compress_tar_xz(&entries, &work_path, level, &tracker)?,
        ArchiveFormat::TarBr => compress_tar_br(&entries, &work_path, level, &tracker)?,
        ArchiveFormat::TarGz => {
            compress_tar_gz(&entries, &work_path, level, &tracker, Arc::clone(&cancel))?
        }
        ArchiveFormat::TarLz4 => compress_tar_lz4(&entries, &work_path, &tracker)?,
        ArchiveFormat::TarBz2 => {
            return Err("Формат .tar.bz2 поддерживается только для распаковки.".to_string());
        }
    }

    let out_size = fs::metadata(&work_path).map(|m| m.len()).unwrap_or(0);
    let ratio = if raw_size > 0 {
        let diff = ((out_size as f64 / raw_size as f64) - 1.0) * 100.0;
        format!(" ({:+.1}%)", diff)
    } else {
        String::new()
    };

    if let Some(limit) = split_bytes {
        if out_size <= limit {
            if output_path.exists() {
                let _ = fs::remove_file(output_path);
            }
            fs::rename(&work_path, output_path).map_err(|e| e.to_string())?;
            return Ok((
                format!(
                    "{} -> {}{} • уместился в 1 файл",
                    format_bytes(raw_size),
                    format_bytes(out_size),
                    ratio
                ),
                output_path.to_path_buf(),
            ));
        }

        let (parts_count, first_part) =
            split_file_into_volumes(&work_path, output_path, limit, cancel, tx, ctx)?;

        return Ok((
            format!(
                "{} -> {}{} • частей: {parts_count}",
                format_bytes(raw_size),
                format_bytes(out_size),
                ratio
            ),
            first_part,
        ));
    }

    Ok((
        format!(
            "{} -> {}{}",
            format_bytes(raw_size),
            format_bytes(out_size),
            ratio
        ),
        output_path.to_path_buf(),
    ))
}

fn run_extract(
    volumes: &[PathBuf],
    extract_dir: &Path,
    format: ArchiveFormat,
    cancel: Arc<AtomicBool>,
    tx: Sender<WorkerMessage>,
    ctx: egui::Context,
) -> Result<String, String> {
    let res = match format {
        ArchiveFormat::Zip => extract_zip_parallel(volumes, extract_dir, cancel, tx, ctx),
        ArchiveFormat::SevenZ => extract_7z(volumes, extract_dir, cancel, tx, ctx),
        ArchiveFormat::TarZst
        | ArchiveFormat::TarXz
        | ArchiveFormat::TarBr
        | ArchiveFormat::TarGz
        | ArchiveFormat::TarLz4
        | ArchiveFormat::TarBz2 => {
            extract_tar_stream(volumes, extract_dir, format, cancel, tx, ctx)
        }
    };

    match res {
        Ok(()) => Ok(format!("Распаковано в {}", extract_dir.display())),
        Err(err) if err.contains(CANCELLED_MARKER) => Err(err),
        Err(err) if volumes.len() > 1 => Err(format!(
            "{err} (Проверь, что все части архива .001, .002... скачаны в одну папку)"
        )),
        Err(err) => Err(err),
    }
}

fn collect_entries(inputs: &[PathBuf], cancel: &AtomicBool) -> Result<Vec<FileEntry>, String> {
    let nested: Result<Vec<Vec<FileEntry>>, String> = inputs
        .par_iter()
        .map(|input| {
            if cancel.load(Ordering::Relaxed) {
                return Err(CANCELLED_MARKER.to_string());
            }
            let mut local = Vec::new();
            if input.is_file() {
                let name = input
                    .file_name()
                    .ok_or_else(|| format!("Некорректное имя файла: {}", input.display()))?
                    .to_string_lossy()
                    .into_owned();
                let size = fs::metadata(input).map(|m| m.len()).unwrap_or(0);
                local.push(FileEntry {
                    src_path: input.clone(),
                    archive_path: name,
                    is_dir: false,
                    size,
                });
            } else if input.is_dir() {
                let base = input.parent().unwrap_or(input);
                for entry_res in WalkDir::new(input) {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(CANCELLED_MARKER.to_string());
                    }
                    let entry = entry_res.map_err(|e| e.to_string())?;
                    let path = entry.path();
                    let rel = path
                        .strip_prefix(base)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if rel.is_empty() {
                        continue;
                    }
                    let is_dir = entry.file_type().is_dir();
                    let size = if is_dir {
                        0
                    } else {
                        entry.metadata().map(|m| m.len()).unwrap_or(0)
                    };
                    local.push(FileEntry {
                        src_path: path.to_path_buf(),
                        archive_path: rel,
                        is_dir,
                        size,
                    });
                }
            }
            Ok(local)
        })
        .collect();

    Ok(nested?.into_iter().flatten().collect())
}

#[allow(clippy::too_many_arguments)]
fn flush_parallel_zip_batch(
    batch: &mut Vec<&FileEntry>,
    writer: &mut zip::ZipWriter<BufWriter<File>>,
    level: CompressionLevel,
    use_zip64_globally: bool,
    done_counter: &Arc<AtomicUsize>,
    total_items: usize,
    threads: usize,
    tracker: &Arc<ProgressTracker>,
) -> Result<(), String> {
    if batch.is_empty() {
        return Ok(());
    }

    let compressed_entries: Result<Vec<Cursor<Vec<u8>>>, String> = batch
        .par_iter()
        .map(|item| {
            if tracker.is_cancelled() {
                return Err(CANCELLED_MARKER.to_string());
            }
            let opts = SimpleFileOptions::default()
                .compression_method(CompressionMethod::Deflated)
                .compression_level(Some(level.zip_level()))
                .large_file(use_zip64_globally || item.size >= ZIP64_THRESHOLD_BYTES);

            let mut mini_writer = zip::ZipWriter::new(Cursor::new(Vec::with_capacity(
                (item.size as usize / 2).clamp(1024, 4 * 1024 * 1024),
            )));
            mini_writer
                .start_file(&item.archive_path, opts)
                .map_err(|e| e.to_string())?;

            let mut f = File::open(&item.src_path).map_err(|e| e.to_string())?;
            let buf_sz = adaptive_buffer_size(item.size);
            copy_with_progress(&mut f, &mut mini_writer, buf_sz, tracker)?;

            let mut cursor = mini_writer.finish().map_err(|e| e.to_string())?;
            cursor.set_position(0);

            let done = done_counter.fetch_add(1, Ordering::Relaxed) + 1;
            tracker.set_label(format!(
                "ZIP [{threads} потоков] ({done}/{total_items}): {}",
                item.archive_path
            ));
            Ok(cursor)
        })
        .collect();

    batch.clear();

    for cursor in compressed_entries? {
        if tracker.is_cancelled() {
            return Err(CANCELLED_MARKER.to_string());
        }
        let mut mini_archive = zip::ZipArchive::new(cursor).map_err(|e| e.to_string())?;
        let zip_file = mini_archive.by_index(0).map_err(|e| e.to_string())?;
        writer.raw_copy_file(zip_file).map_err(|e| e.to_string())?;
    }

    Ok(())
}

fn compress_zip_parallel(
    entries: &[FileEntry],
    output_path: &Path,
    level: CompressionLevel,
    smart_skip_media: bool,
    tracker: &Arc<ProgressTracker>,
) -> Result<(), String> {
    let out_file = File::create(output_path).map_err(|e| e.to_string())?;
    let buffered_out = BufWriter::with_capacity(4 * 1024 * 1024, out_file);
    let mut writer = zip::ZipWriter::new(buffered_out);

    let threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let use_zip64_globally = tracker.total_bytes >= ZIP64_THRESHOLD_BYTES;
    let total_items = entries.len().max(1);
    let done_counter = Arc::new(AtomicUsize::new(0));

    let mut parallel_batch: Vec<&FileEntry> = Vec::with_capacity(threads * 2);
    let mut batch_bytes: u64 = 0;

    for item in entries {
        if tracker.is_cancelled() {
            return Err(CANCELLED_MARKER.to_string());
        }
        if item.src_path == output_path {
            continue;
        }

        if item.is_dir {
            let dir_name = if item.archive_path.ends_with('/') {
                item.archive_path.clone()
            } else {
                format!("{}/", item.archive_path)
            };
            let dir_opts = SimpleFileOptions::default().large_file(use_zip64_globally);
            writer
                .add_directory(dir_name, dir_opts)
                .map_err(|e| e.to_string())?;
            done_counter.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let skip_compress = smart_skip_media && is_incompressible_ext(&item.src_path);
        let can_parallel = !skip_compress && item.size <= ZIP_PARALLEL_MAX_FILE_SIZE;

        if can_parallel {
            parallel_batch.push(item);
            batch_bytes += item.size;
            if parallel_batch.len() >= threads * 2 || batch_bytes >= ZIP_PARALLEL_BATCH_BYTES {
                flush_parallel_zip_batch(
                    &mut parallel_batch,
                    &mut writer,
                    level,
                    use_zip64_globally,
                    &done_counter,
                    total_items,
                    threads,
                    tracker,
                )?;
                batch_bytes = 0;
            }
        } else {
            flush_parallel_zip_batch(
                &mut parallel_batch,
                &mut writer,
                level,
                use_zip64_globally,
                &done_counter,
                total_items,
                threads,
                tracker,
            )?;
            batch_bytes = 0;

            let large_file = use_zip64_globally || item.size >= ZIP64_THRESHOLD_BYTES;
            let options = if skip_compress {
                SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Stored)
                    .large_file(large_file)
            } else {
                SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Deflated)
                    .compression_level(Some(level.zip_level()))
                    .large_file(large_file)
            };

            let done = done_counter.fetch_add(1, Ordering::Relaxed) + 1;
            let tag = if skip_compress { " [без сжатия]" } else { "" };
            tracker.set_label(format!(
                "ZIP ({done}/{total_items}){tag}: {}",
                item.archive_path
            ));

            writer
                .start_file(&item.archive_path, options)
                .map_err(|e| e.to_string())?;
            let mut f = File::open(&item.src_path).map_err(|e| e.to_string())?;
            let buf_sz = adaptive_buffer_size(item.size);
            copy_with_progress(&mut f, &mut writer, buf_sz, tracker)?;
        }
    }

    flush_parallel_zip_batch(
        &mut parallel_batch,
        &mut writer,
        level,
        use_zip64_globally,
        &done_counter,
        total_items,
        threads,
        tracker,
    )?;

    let mut out = writer.finish().map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    Ok(())
}

fn compress_7z(
    entries: &[FileEntry],
    output_path: &Path,
    level: CompressionLevel,
    tracker: &Arc<ProgressTracker>,
) -> Result<(), String> {
    let mut writer = ArchiveWriter::create(output_path).map_err(|e| e.to_string())?;
    let threads = thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4);
    let lzma_opts = Lzma2Options::from_level_mt(level.sevenz_level(), threads, 16 * 1024 * 1024);
    writer.set_content_methods(vec![lzma_opts.into()]);

    let total_items = entries.len().max(1);

    for (idx, item) in entries.iter().enumerate() {
        if tracker.is_cancelled() {
            return Err(CANCELLED_MARKER.to_string());
        }
        if item.src_path == output_path {
            continue;
        }
        tracker.set_label(format!(
            "7Z [{threads} потоков] ({}/{total_items}): {}",
            idx + 1,
            item.archive_path
        ));
        let entry = ArchiveEntry::from_path(&item.src_path, item.archive_path.clone());
        if item.is_dir {
            writer
                .push_archive_entry(entry, None::<io::Empty>)
                .map_err(|e| e.to_string())?;
        } else {
            let f = File::open(&item.src_path).map_err(|e| e.to_string())?;
            let buf_sz = adaptive_buffer_size(item.size);
            let reader =
                BufReader::with_capacity(buf_sz, ProgressReader::new(f, Arc::clone(tracker)));
            writer
                .push_archive_entry(entry, Some(reader))
                .map_err(|e| e.to_string())?;
        }
    }

    writer.finish().map_err(|e| e.to_string())?;
    Ok(())
}

fn append_entries_to_tar<W: Write>(
    tar_builder: &mut tar::Builder<W>,
    entries: &[FileEntry],
    output_path: &Path,
    format_tag: &str,
    tracker: &Arc<ProgressTracker>,
) -> Result<(), String> {
    let total_items = entries.len().max(1);

    for (idx, item) in entries.iter().enumerate() {
        if tracker.is_cancelled() {
            return Err(CANCELLED_MARKER.to_string());
        }
        if item.src_path == output_path {
            continue;
        }
        tracker.set_label(format!(
            "{format_tag} ({}/{total_items}): {}",
            idx + 1,
            item.archive_path
        ));

        if item.is_dir {
            tar_builder
                .append_dir(&item.archive_path, &item.src_path)
                .map_err(|e| e.to_string())?;
        } else {
            let meta = fs::metadata(&item.src_path).map_err(|e| e.to_string())?;
            let f = File::open(&item.src_path).map_err(|e| e.to_string())?;
            let buf_sz = adaptive_buffer_size(meta.len());
            let reader =
                BufReader::with_capacity(buf_sz, ProgressReader::new(f, Arc::clone(tracker)));

            let mut header = tar::Header::new_gnu();
            header.set_metadata(&meta);
            header.set_size(meta.len());
            tar_builder
                .append_data(&mut header, &item.archive_path, reader)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn compress_tar_zst(
    entries: &[FileEntry],
    output_path: &Path,
    level: CompressionLevel,
    tracker: &Arc<ProgressTracker>,
) -> Result<(), String> {
    let out_file = File::create(output_path).map_err(|e| e.to_string())?;
    let buffered_out = BufWriter::with_capacity(2 * 1024 * 1024, out_file);
    let mut encoder = zstd::stream::write::Encoder::new(buffered_out, level.zstd_level())
        .map_err(|e| e.to_string())?;

    let threads = thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4);
    encoder.multithread(threads).map_err(|e| e.to_string())?;

    let mut tar_builder = tar::Builder::new(encoder);
    let tag = format!("TAR.ZST [{threads} потоков]");
    append_entries_to_tar(&mut tar_builder, entries, output_path, &tag, tracker)?;

    let encoder = tar_builder.into_inner().map_err(|e| e.to_string())?;
    let mut out = encoder.finish().map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    Ok(())
}

fn compress_tar_xz(
    entries: &[FileEntry],
    output_path: &Path,
    level: CompressionLevel,
    tracker: &Arc<ProgressTracker>,
) -> Result<(), String> {
    let out_file = File::create(output_path).map_err(|e| e.to_string())?;
    let buffered_out = BufWriter::with_capacity(2 * 1024 * 1024, out_file);

    let threads = thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4);
    let mut opts = lzma_rust2::XzOptions::with_preset(level.xz_level());
    opts.set_block_size(NonZeroU64::new(16 * 1024 * 1024));
    let encoder =
        lzma_rust2::XzWriterMt::new(buffered_out, opts, threads).map_err(|e| e.to_string())?;

    let mut tar_builder = tar::Builder::new(encoder);
    let tag = format!("TAR.XZ [{threads} потоков]");
    append_entries_to_tar(&mut tar_builder, entries, output_path, &tag, tracker)?;

    let encoder = tar_builder.into_inner().map_err(|e| e.to_string())?;
    let mut out = encoder.finish().map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    Ok(())
}

fn compress_tar_br(
    entries: &[FileEntry],
    output_path: &Path,
    level: CompressionLevel,
    tracker: &Arc<ProgressTracker>,
) -> Result<(), String> {
    let out_file = File::create(output_path).map_err(|e| e.to_string())?;
    let buffered_out = BufWriter::with_capacity(2 * 1024 * 1024, out_file);
    let encoder =
        brotli::CompressorWriter::new(buffered_out, 64 * 1024, level.brotli_level(), 22);

    let mut tar_builder = tar::Builder::new(encoder);
    append_entries_to_tar(&mut tar_builder, entries, output_path, "TAR.BR", tracker)?;

    let encoder = tar_builder.into_inner().map_err(|e| e.to_string())?;
    let mut out = encoder.into_inner();
    out.flush().map_err(|e| e.to_string())?;
    Ok(())
}

fn compress_tar_gz(
    entries: &[FileEntry],
    output_path: &Path,
    level: CompressionLevel,
    tracker: &Arc<ProgressTracker>,
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    let out_file = File::create(output_path).map_err(|e| e.to_string())?;
    let buffered_out = BufWriter::with_capacity(2 * 1024 * 1024, out_file);
    let gz = ParallelGzipWriter::new(
        buffered_out,
        flate2::Compression::new(level.gzip_level()),
        cancel,
    );

    let threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let mut tar_builder = tar::Builder::new(gz);
    let tag = format!("TAR.GZ [{threads} потоков]");
    append_entries_to_tar(&mut tar_builder, entries, output_path, &tag, tracker)?;

    let gz = tar_builder.into_inner().map_err(|e| e.to_string())?;
    let mut out = gz.finish().map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    Ok(())
}

fn compress_tar_lz4(
    entries: &[FileEntry],
    output_path: &Path,
    tracker: &Arc<ProgressTracker>,
) -> Result<(), String> {
    let out_file = File::create(output_path).map_err(|e| e.to_string())?;
    let buffered_out = BufWriter::with_capacity(4 * 1024 * 1024, out_file);
    let encoder = lz4_flex::frame::FrameEncoder::new(buffered_out);

    let mut tar_builder = tar::Builder::new(encoder);
    append_entries_to_tar(&mut tar_builder, entries, output_path, "TAR.LZ4", tracker)?;

    let encoder = tar_builder.into_inner().map_err(|e| e.to_string())?;
    let mut out = encoder.finish().map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    Ok(())
}

fn extract_zip_parallel(
    volumes: &[PathBuf],
    extract_dir: &Path,
    cancel: Arc<AtomicBool>,
    tx: Sender<WorkerMessage>,
    ctx: egui::Context,
) -> Result<(), String> {
    fs::create_dir_all(extract_dir).map_err(|e| e.to_string())?;

    let mut file_indices = Vec::new();
    let mut total_bytes: u64 = 0;
    {
        let reader = MultiVolumeReader::open(volumes).map_err(|e| e.to_string())?;
        let mut archive = zip::ZipArchive::new(reader).map_err(|e| e.to_string())?;
        for i in 0..archive.len() {
            let entry = archive.by_index(i).map_err(|e| e.to_string())?;
            let Some(enclosed) = entry.enclosed_name() else {
                continue;
            };
            let out_path = extract_dir.join(enclosed);
            if entry.is_dir() {
                fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
            } else {
                if let Some(parent) = out_path.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                total_bytes += entry.size();
                file_indices.push(i);
            }
        }
    }

    let tracker = ProgressTracker::new(total_bytes, cancel, tx, ctx);
    let total_files = file_indices.len().max(1);
    let done_files = Arc::new(AtomicUsize::new(0));

    file_indices.par_iter().try_for_each_init(
        || -> Result<zip::ZipArchive<MultiVolumeReader>, String> {
            let r = MultiVolumeReader::open(volumes).map_err(|e| e.to_string())?;
            zip::ZipArchive::new(r).map_err(|e| e.to_string())
        },
        |archive_res, &idx| -> Result<(), String> {
            if tracker.is_cancelled() {
                return Err(CANCELLED_MARKER.to_string());
            }
            let archive = archive_res.as_mut().map_err(|e| e.clone())?;
            let mut entry = archive.by_index(idx).map_err(|e| e.to_string())?;
            let Some(enclosed) = entry.enclosed_name() else {
                return Ok(());
            };
            let completed = done_files.fetch_add(1, Ordering::Relaxed) + 1;
            tracker.set_label(format!(
                "Распаковка ZIP ({completed}/{total_files}): {}",
                entry.name()
            ));

            let out_path = extract_dir.join(enclosed);
            let out_file = File::create(&out_path).map_err(|e| e.to_string())?;
            let buf_sz = adaptive_buffer_size(entry.size());
            let mut out_writer = BufWriter::with_capacity(buf_sz, out_file);
            copy_with_progress(&mut entry, &mut out_writer, buf_sz, &tracker)?;
            out_writer.flush().map_err(|e| e.to_string())?;
            Ok(())
        },
    )?;

    Ok(())
}

fn extract_7z(
    volumes: &[PathBuf],
    extract_dir: &Path,
    cancel: Arc<AtomicBool>,
    tx: Sender<WorkerMessage>,
    ctx: egui::Context,
) -> Result<(), String> {
    fs::create_dir_all(extract_dir).map_err(|e| e.to_string())?;
    let tracker = ProgressTracker::new(0, cancel, tx, ctx);
    let reader = MultiVolumeReader::open(volumes).map_err(|e| e.to_string())?;
    let mut count = 0usize;

    let res = sevenz_rust2::decompress_with_extract_fn(
        reader,
        extract_dir,
        |entry, entry_reader, dest| {
            if tracker.is_cancelled() {
                return Ok(false);
            }
            if !is_safe_relative_entry_path(entry.name()) {
                return Ok(true);
            }
            count += 1;
            tracker.set_label(format!(
                "Распаковка 7Z (файлов: {count}): {}",
                entry.name()
            ));
            let mut pr = ProgressReader::new(entry_reader, Arc::clone(&tracker));
            sevenz_rust2::default_entry_extract_fn(entry, &mut pr, dest)
        },
    );

    if tracker.is_cancelled() {
        return Err(CANCELLED_MARKER.to_string());
    }
    res.map_err(|e| e.to_string())
}

fn extract_tar_stream(
    volumes: &[PathBuf],
    extract_dir: &Path,
    format: ArchiveFormat,
    cancel: Arc<AtomicBool>,
    tx: Sender<WorkerMessage>,
    ctx: egui::Context,
) -> Result<(), String> {
    fs::create_dir_all(extract_dir).map_err(|e| e.to_string())?;
    let reader = MultiVolumeReader::open(volumes).map_err(|e| e.to_string())?;
    let total_size = reader.total_len();
    let tracker = ProgressTracker::new(total_size, cancel, tx, ctx);

    let buf_sz = adaptive_buffer_size(total_size);
    let progress_reader =
        BufReader::with_capacity(buf_sz, ProgressReader::new(reader, Arc::clone(&tracker)));

    match format {
        ArchiveFormat::TarZst => {
            let decoder = zstd::stream::read::Decoder::new(progress_reader)
                .map_err(|e| e.to_string())?;
            unpack_tar_entries(decoder, extract_dir, "TAR.ZST", &tracker)
        }
        ArchiveFormat::TarXz => {
            let decoder = lzma_rust2::XzReader::new(progress_reader, true);
            unpack_tar_entries(decoder, extract_dir, "TAR.XZ", &tracker)
        }
        ArchiveFormat::TarBr => {
            let decoder = brotli::Decompressor::new(progress_reader, 64 * 1024);
            unpack_tar_entries(decoder, extract_dir, "TAR.BR", &tracker)
        }
        ArchiveFormat::TarGz => {
            let decoder = MultiGzDecoder::new(progress_reader);
            unpack_tar_entries(decoder, extract_dir, "TAR.GZ", &tracker)
        }
        ArchiveFormat::TarLz4 => {
            let decoder = lz4_flex::frame::FrameDecoder::new(progress_reader);
            unpack_tar_entries(decoder, extract_dir, "TAR.LZ4", &tracker)
        }
        ArchiveFormat::TarBz2 => {
            let decoder = bzip2::read::MultiBzDecoder::new(progress_reader);
            unpack_tar_entries(decoder, extract_dir, "TAR.BZ2", &tracker)
        }
        _ => Err("Неподдерживаемый потоковый формат.".to_string()),
    }
}

fn unpack_tar_entries<R: Read>(
    decoder: R,
    extract_dir: &Path,
    tag: &str,
    tracker: &Arc<ProgressTracker>,
) -> Result<(), String> {
    let mut archive = tar::Archive::new(decoder);
    let mut count = 0usize;

    for entry_res in archive.entries().map_err(|e| e.to_string())? {
        if tracker.is_cancelled() {
            return Err(CANCELLED_MARKER.to_string());
        }
        let mut entry = entry_res.map_err(|e| e.to_string())?;
        let name = entry
            .path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "файл".to_string());
        count += 1;
        tracker.set_label(format!("Распаковка {tag} (файлов: {count}): {name}"));
        entry.unpack_in(extract_dir).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------- Вспомогательные функции ----------

fn strip_archive_ext(path: &Path) -> String {
    let clean_path = parse_volume_path(path)
        .map(|v| v.base_path)
        .unwrap_or_else(|| path.to_path_buf());
    let name = clean_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "extracted".to_string());
    let lower = name.to_lowercase();
    for ext in [
        ".tar.zst", ".tzst", ".tar.xz", ".txz", ".tar.br", ".tbr", ".tar.gz", ".tgz", ".tar.lz4",
        ".tlz4", ".tar.bz2", ".tbz2", ".tbz", ".zip", ".7z",
    ] {
        if lower.ends_with(ext) {
            let trimmed = &name[..name.len() - ext.len()];
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    format!("{name}_extracted")
}

fn split_name_and_ext(file_name: &str) -> (&str, &str) {
    let lower = file_name.to_lowercase();
    for ext in [
        ".tar.zst", ".tzst", ".tar.xz", ".txz", ".tar.br", ".tbr", ".tar.gz", ".tgz", ".tar.lz4",
        ".tlz4", ".tar.bz2", ".tbz2", ".tbz", ".zip", ".7z",
    ] {
        if lower.ends_with(ext) {
            let split_at = file_name.len() - ext.len();
            return (&file_name[..split_at], &file_name[split_at..]);
        }
    }
    match file_name.rfind('.') {
        Some(pos) if pos > 0 => (&file_name[..pos], &file_name[pos..]),
        _ => (file_name, ""),
    }
}

fn path_or_first_part_exists(path: &Path, will_split: bool) -> bool {
    path.exists() || (will_split && volume_part_path(path, 1, 3).exists())
}

fn resolve_unique_path(path: &Path, is_dir: bool, will_split: bool) -> PathBuf {
    if !path_or_first_part_exists(path, will_split) {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".to_string());

    let (stem, ext) = if is_dir {
        (file_name.as_str(), "")
    } else {
        split_name_and_ext(&file_name)
    };

    for i in 1..10_000 {
        let candidate = parent.join(format!("{stem} ({i}){ext}"));
        if !path_or_first_part_exists(&candidate, will_split) {
            return candidate;
        }
    }
    path.to_path_buf()
}

fn sanitize_name(name: &str) -> String {
    name.trim()
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
        .collect()
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{bytes} Б")
    } else {
        format!("{size:.2} {}", UNITS[unit_idx])
    }
}

fn open_in_explorer(path: &Path) {
    if path.is_file() {
        let _ = Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    } else {
        let _ = Command::new("explorer").arg(path).spawn();
    }
}