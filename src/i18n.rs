//! Runtime localization of the user interface.
//!
//! Every user-facing string lives in the [`strings!`] table below with one
//! translation per [`Lang`]. The active language is a process-global atomic so
//! any thread can format a message; it is set once from the saved [`Settings`]
//! at start-up and whenever the user picks another language.
//!
//! [`Settings`]: crate::settings::Settings
//!
//! Call sites use the [`t!`](crate::t) macro, which resolves the key through
//! [`key`] and is therefore checked at compile time:
//!
//! ```ignore
//! ui.label(t!(NAV_HOME));                       // &'static str
//! ui.label(t!(READY_TO_RECORD, source_label));  // String, fills {0}
//! ```
//!
//! Placeholders are `{0}`, `{1}`, … so translations may reorder them.

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// A language the interface can be shown in. English is the fallback for any
/// string a translation is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Lang {
    #[default]
    #[serde(rename = "en")]
    En,
    #[serde(rename = "ro")]
    Ro,
    #[serde(rename = "ru")]
    Ru,
}

impl Lang {
    pub const ALL: [Lang; 3] = [Lang::En, Lang::Ro, Lang::Ru];

    /// ISO 639-1 code, also what is written to `settings.json`.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ro => "ro",
            Lang::Ru => "ru",
        }
    }

    /// Name of the language in that language, for the picker.
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ro => "Română",
            Lang::Ru => "Русский",
        }
    }

    pub fn from_code(code: &str) -> Option<Lang> {
        let code = code.trim().to_ascii_lowercase();
        Lang::ALL.into_iter().find(|l| l.code() == code || code.starts_with(&format!("{}-", l.code())))
    }

    fn from_u8(v: u8) -> Lang {
        Lang::ALL.get(v as usize).copied().unwrap_or(Lang::En)
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The language the interface is currently drawn in.
pub fn lang() -> Lang {
    Lang::from_u8(CURRENT.load(Ordering::Relaxed))
}

/// Switches the interface language; takes effect on the next repaint.
pub fn set_lang(lang: Lang) {
    let idx = Lang::ALL.iter().position(|l| *l == lang).unwrap_or(0) as u8;
    CURRENT.store(idx, Ordering::Relaxed);
}

/// Translates `key` into the active language, falling back to English.
pub fn t(key: &str) -> &'static str {
    lookup(lang(), key).or_else(|| lookup(Lang::En, key)).unwrap_or_else(|| {
        debug_assert!(false, "missing translation key: {key}");
        ""
    })
}

/// Like [`t`] but substitutes `{0}`, `{1}`, … with `args`.
pub fn tf(key: &str, args: &[&str]) -> String {
    let mut out = t(key).to_string();
    for (i, arg) in args.iter().enumerate() {
        out = out.replace(&format!("{{{i}}}"), arg);
    }
    out
}

/// Looks up a translated string. `t!(KEY)` returns `&'static str`;
/// `t!(KEY, a, b)` returns a `String` with `{0}`/`{1}` filled in.
#[macro_export]
macro_rules! t {
    ($name:ident) => {
        $crate::i18n::t($crate::i18n::key::$name)
    };
    ($name:ident, $($arg:expr),+ $(,)?) => {
        $crate::i18n::tf($crate::i18n::key::$name, &[$(&::std::string::ToString::to_string(&$arg)),+])
    };
}

/// Declares the translation table and the compile-time-checked key constants.
macro_rules! strings {
    ($($name:ident => $en:expr, $ro:expr, $ru:expr;)*) => {
        /// String keys, so `t!(SOME_KEY)` fails to compile on a typo.
        pub mod key {
            $(pub const $name: &str = stringify!($name);)*
        }

        fn lookup(lang: Lang, key: &str) -> Option<&'static str> {
            match key {
                $(stringify!($name) => Some(match lang {
                    Lang::En => $en,
                    Lang::Ro => $ro,
                    Lang::Ru => $ru,
                }),)*
                _ => None,
            }
        }

        /// Every key in the table, for the completeness test below.
        #[cfg(test)]
        const ALL_KEYS: &[&str] = &[$(stringify!($name)),*];
    };
}

strings! {
    // ----- generic ---------------------------------------------------------
    OK => "OK", "OK", "OK";
    CANCEL => "Cancel", "Anulează", "Отмена";
    CLOSE => "Close", "Închide", "Закрыть";
    DELETE => "Delete", "Șterge", "Удалить";
    OPEN => "Open", "Deschide", "Открыть";
    SETTINGS => "Settings", "Setări", "Настройки";
    NONE_PAREN => "(none)", "(niciunul)", "(нет)";
    ON => "on", "pornit", "вкл";
    OFF => "off", "oprit", "выкл";
    AUTO => "Auto", "Automat", "Авто";
    YES_GPU => "yes (GPU)", "da (GPU)", "да (GPU)";
    NO_CPU => "no (CPU)", "nu (CPU)", "нет (CPU)";
    MONO => "Mono", "Mono", "Моно";
    STEREO => "Stereo", "Stereo", "Стерео";
    MONO_LOWER => "mono", "mono", "моно";
    STEREO_LOWER => "stereo", "stereo", "стерео";

    // ----- languages -------------------------------------------------------
    LANGUAGE => "Language", "Limbă", "Язык";
    LANGUAGE_HINT => "Applies immediately; saved with your settings.",
        "Se aplică imediat; se salvează împreună cu setările.",
        "Применяется сразу и сохраняется вместе с настройками.";

    // ----- toolbar ---------------------------------------------------------
    MODE_REGION => "Region", "Regiune", "Область";
    MODE_MONITOR => "Monitor", "Monitor", "Монитор";
    MODE_WINDOW => "Window", "Fereastră", "Окно";
    MODE_TIP => "{0} recording mode", "Mod de înregistrare: {0}", "Режим записи: {0}";
    SYSTEM_AUDIO => "System audio", "Sunet de sistem", "Системный звук";
    MICROPHONE => "Microphone", "Microfon", "Микрофон";
    SHOW_CURSOR => "Show cursor", "Arată cursorul", "Показывать курсор";
    CURSOR => "Cursor", "Cursor", "Курсор";
    TIP_SNAPSHOT => "Take a snapshot (PNG)", "Fă o captură de ecran (PNG)", "Сделать снимок экрана (PNG)";
    TIP_MINIBAR => "Mini bar: collapse to a small floating recorder bar (close the bar to come back)",
        "Bară mică: restrânge fereastra la o bară plutitoare (închide bara pentru a reveni)",
        "Мини-панель: свернуть в маленькую плавающую панель записи (закройте панель, чтобы вернуться)";
    TIP_REC_START => "Start recording", "Începe înregistrarea", "Начать запись";
    TIP_REC_STOP => "Stop recording", "Oprește înregistrarea", "Остановить запись";
    TIP_REC_CANCEL => "Cancel the countdown", "Anulează numărătoarea inversă", "Отменить обратный отсчёт";
    TIP_PAUSE => "Pause recording", "Suspendă înregistrarea", "Приостановить запись";
    TIP_RESUME => "Resume recording", "Reia înregistrarea", "Продолжить запись";

    // ----- countdown -------------------------------------------------------
    COUNTDOWN_TITLE => "Recording starts in", "Înregistrarea începe în", "Запись начнётся через";
    COUNTDOWN_CANCEL => "Cancel (Esc)", "Anulează (Esc)", "Отмена (Esc)";
    COUNTDOWN_STATUS => "Recording starts in {0}…", "Înregistrarea începe în {0}…", "Запись начнётся через {0}…";
    COUNTDOWN_ESC => "(Esc cancels)", "(Esc anulează)", "(Esc — отмена)";

    // ----- status strip ----------------------------------------------------
    STATUS_PAUSED => "PAUSED  {0}", "SUSPENDAT  {0}", "ПАУЗА  {0}";
    STATUS_REC => "REC  {0}", "REC  {0}", "REC  {0}";
    STATUS_COUNTERS => "{0}×{1}   {2} fps   {3} dropped   {4}",
        "{0}×{1}   {2} fps   {3} pierdute   {4}",
        "{0}×{1}   {2} fps   потеряно: {3}   {4}";
    STATUS_PICKING => "Drag a rectangle on the screen to select the recording region (Esc to cancel)",
        "Trage un dreptunghi pe ecran pentru a alege regiunea înregistrată (Esc pentru anulare)",
        "Выделите прямоугольник на экране, чтобы выбрать область записи (Esc — отмена)";
    STATUS_READY => "Ready to record: {0}", "Gata de înregistrare: {0}", "Готово к записи: {0}";
    STATUS_NO_SOURCE => "Please select a recording source", "Alege o sursă de înregistrare", "Выберите источник записи";
    STATUS_CHANGE => "Change…", "Schimbă…", "Изменить…";
    STATUS_CHANGE_TIP => "Choose the monitor, window or region", "Alege monitorul, fereastra sau regiunea",
        "Выберите монитор, окно или область";

    // ----- navigation ------------------------------------------------------
    NAV_HOME => "Home", "Acasă", "Главная";
    NAV_GENERAL => "General", "General", "Общие";
    NAV_VIDEO => "Video", "Video", "Видео";
    NAV_IMAGE => "Image", "Imagine", "Изображение";
    NAV_ABOUT => "About", "Despre", "О программе";

    // ----- Home tabs / library ---------------------------------------------
    TAB_VIDEOS => "Videos", "Videoclipuri", "Видео";
    TAB_IMAGES => "Images", "Imagini", "Изображения";
    TAB_AUDIOS => "Audios", "Fișiere audio", "Аудио";
    TAB_PREVIEW => "Preview", "Previzualizare", "Предпросмотр";
    NO_VIDEOS_YET => "No videos yet", "Niciun videoclip încă", "Пока нет видео";
    NO_IMAGES_YET => "No images yet", "Nicio imagine încă", "Пока нет изображений";
    NO_AUDIOS_YET => "No audio files yet", "Niciun fișier audio încă", "Пока нет аудиофайлов";
    OUTPUT_FOLDER_TIP => "Output folder (change it under General)", "Folderul de ieșire (îl poți schimba în General)",
        "Папка сохранения (изменяется в разделе «Общие»)";
    REFRESH_LIST => "Refresh list", "Reîmprospătează lista", "Обновить список";
    OPEN_FOLDER => "Open folder", "Deschide folderul", "Открыть папку";
    PLAY => "Play", "Redă", "Воспроизвести";
    FOLDER => "Folder", "Folder", "Папка";

    // ----- source row ------------------------------------------------------
    NO_MONITORS => "No monitors", "Niciun monitor", "Мониторы не найдены";
    NO_WINDOWS => "No windows", "Nicio fereastră", "Окна не найдены";
    NO_MONITOR_SELECTED => "No monitor", "Niciun monitor", "Монитор не выбран";
    NO_WINDOW_SELECTED => "No window selected", "Nicio fereastră selectată", "Окно не выбрано";
    NO_REGION_SELECTED => "No region selected", "Nicio regiune selectată", "Область не выбрана";
    REGION_LABEL => "Region {0}×{1} at ({2}, {3}) on {4}", "Regiune {0}×{1} la ({2}, {3}) pe {4}",
        "Область {0}×{1} в ({2}, {3}) на {4}";
    SELECT_REGION => "Select region…", "Alege regiunea…", "Выбрать область…";
    REFRESH_SOURCES_TIP => "Refresh monitors, windows and audio devices",
        "Reîmprospătează monitoarele, ferestrele și dispozitivele audio",
        "Обновить список мониторов, окон и аудиоустройств";

    // ----- preview ---------------------------------------------------------
    PREVIEW_UNAVAILABLE => "Preview unavailable: {0}", "Previzualizare indisponibilă: {0}", "Предпросмотр недоступен: {0}";
    PREVIEW_PICK_SOURCE => "Select a monitor, window or region to preview",
        "Alege un monitor, o fereastră sau o regiune pentru previzualizare",
        "Выберите монитор, окно или область для предпросмотра";
    PREVIEW_STARTING => "Starting preview…", "Se pornește previzualizarea…", "Запуск предпросмотра…";

    // ----- General page ----------------------------------------------------
    SECTION_OUTPUT => "Output", "Ieșire", "Сохранение";
    ROW_SAVE_TO => "Save to", "Salvează în", "Сохранять в";
    CHOOSE_FOLDER => "Choose folder…", "Alege folderul…", "Выбрать папку…";
    ROW_FILE_PREFIX => "File name prefix", "Prefixul numelui", "Префикс имени файла";
    SECTION_RECORDING => "Recording", "Înregistrare", "Запись";
    ROW_COUNTDOWN => "Countdown", "Numărătoare inversă", "Обратный отсчёт";
    COUNTDOWN_CHECKBOX => "Count down before recording starts", "Numără invers înainte de începerea înregistrării",
        "Обратный отсчёт перед началом записи";
    COUNTDOWN_NOTE => "shown in the window and the mini bar, never in the video",
        "afișată în fereastră și în bara mică, niciodată în videoclip",
        "показывается в окне и на мини-панели, но не попадает в видео";
    SECTION_SOURCES => "Sources", "Surse", "Источники";
    ROW_DEVICES => "Devices", "Dispozitive", "Устройства";
    REFRESH_DEVICES => "Refresh monitors, windows and devices", "Reîmprospătează monitoarele, ferestrele și dispozitivele",
        "Обновить мониторы, окна и устройства";
    ROW_SETTINGS_FILE => "Settings file", "Fișier de setări", "Файл настроек";
    SECTION_APPEARANCE => "Appearance", "Aspect", "Оформление";

    // ----- Video page ------------------------------------------------------
    TAB_RECORD => "Record", "Înregistrare", "Запись";
    TAB_MOUSE => "Mouse", "Mouse", "Мышь";
    CHK_SHOW_CURSOR => "Show mouse cursor", "Arată cursorul mouse-ului", "Показывать курсор мыши";
    CHK_CLICK_EFFECTS => "Add mouse click effects", "Adaugă efecte la clic", "Добавлять эффекты нажатий мыши";
    CHK_CLICK_EFFECT => "Add mouse click effect", "Adaugă efect la clic", "Добавлять эффект нажатия мыши";
    CHK_HIGHLIGHT => "Add mouse highlight effect", "Adaugă efect de evidențiere a mouse-ului",
        "Добавлять подсветку курсора";
    CHK_SYSTEM_AUDIO => "Record what you hear (speakers / headphones)", "Înregistrează ce auzi (difuzoare / căști)",
        "Записывать то, что слышно (динамики / наушники)";
    CHK_MICROPHONE => "Record microphone", "Înregistrează microfonul", "Записывать микрофон";
    ROW_DEVICE => "Device", "Dispozitiv", "Устройство";
    NO_INPUT_DEVICES => "No input devices", "Niciun dispozitiv de intrare", "Нет устройств ввода";
    SECTION_FORMAT => "Format – {0}", "Format – {0}", "Формат – {0}";
    BOX_VIDEO => "Video", "Video", "Видео";
    BOX_AUDIO => "Audio", "Audio", "Аудио";
    FORMAT_SETTINGS_TIP => "Container, codecs, size and quality", "Container, codecuri, dimensiune și calitate",
        "Контейнер, кодеки, размер и качество";
    SCANNING_ENCODERS => "scanning encoders…", "se scanează codificatoarele…", "поиск кодировщиков…";
    SRC_SYSTEM_AND_MIC => "system audio + microphone", "sunet de sistem + microfon", "системный звук + микрофон";
    SRC_SYSTEM => "system audio", "sunet de sistem", "системный звук";
    SRC_MIC => "microphone", "microfon", "микрофон";
    SRC_NO_AUDIO => "no audio", "fără sunet", "без звука";

    // ----- Mouse tab -------------------------------------------------------
    SECTION_MOUSE_FX => "Mouse effects", "Efecte de mouse", "Эффекты мыши";
    ROW_SIZE_INDENT => "      Size", "      Dimensiune", "      Размер";
    ROW_LEFT_CLICK_COLOR => "      Left click color", "      Culoare clic stânga", "      Цвет левого клика";
    ROW_RIGHT_CLICK_COLOR => "      Right click color", "      Culoare clic dreapta", "      Цвет правого клика";
    ROW_HIGHLIGHT_COLOR => "      Highlight color", "      Culoare evidențiere", "      Цвет подсветки";
    ROW_OPACITY => "      Opacity", "      Opacitate", "      Прозрачность";
    APP_DRAWN => "(app-drawn)", "(desenat de aplicație)", "(рисует приложение)";
    FX_PREVIEW_HINT => "Click inside to test the click effect.", "Dă clic înăuntru pentru a testa efectul.",
        "Щёлкните внутри, чтобы проверить эффект нажатия.";

    // ----- Image page ------------------------------------------------------
    SECTION_SNAPSHOT => "Snapshot", "Captură de ecran", "Снимок экрана";
    BOX_IMAGE => "Image", "Imagine", "Изображение";
    SNAPSHOT_DETAIL => "Full size, saved next to your recordings", "Dimensiune completă, salvată lângă înregistrări",
        "Полный размер, сохраняется рядом с записями";
    ROW_CAPTURE => "Capture", "Captură", "Съёмка";
    TAKE_SNAPSHOT_NOW => "Take snapshot now", "Fă o captură acum", "Сделать снимок сейчас";
    ROW_SOURCE => "Source", "Sursă", "Источник";

    // ----- About page ------------------------------------------------------
    ABOUT_VERSION => "Version {0}", "Versiunea {0}", "Версия {0}";
    ABOUT_TAGLINE => "A self-contained screen recorder: no ffmpeg, no codec packs.",
        "Un recorder de ecran de sine stătător: fără ffmpeg, fără pachete de codecuri.",
        "Самодостаточная программа записи экрана: без ffmpeg и наборов кодеков.";
    ABOUT_VIDEO => "Video: H.264 via bundled OpenH264, plus hardware H.264 / HEVC (NVENC, AMF, Quick Sync) through Windows Media Foundation",
        "Video: H.264 prin OpenH264 inclus, plus H.264 / HEVC hardware (NVENC, AMF, Quick Sync) prin Windows Media Foundation",
        "Видео: H.264 через встроенный OpenH264, а также аппаратные H.264 / HEVC (NVENC, AMF, Quick Sync) через Windows Media Foundation";
    ABOUT_AUDIO => "Audio: MP3 via bundled LAME, AAC (Windows Media Foundation) or PCM · Containers: in-house MP4 and AVI muxers",
        "Audio: MP3 prin LAME inclus, AAC (Windows Media Foundation) sau PCM · Containere: muxere MP4 și AVI proprii",
        "Аудио: MP3 через встроенный LAME, AAC (Windows Media Foundation) или PCM · Контейнеры: собственные мультиплексоры MP4 и AVI";
    ABOUT_LICENSE => "Licensed under Apache-2.0. OpenH264 is BSD-2-Clause (source build, no Cisco patent coverage); LAME is LGPL.",
        "Licențiat Apache-2.0. OpenH264 este BSD-2-Clause (compilat din surse, fără acoperirea brevetelor Cisco); LAME este LGPL.",
        "Лицензия Apache-2.0. OpenH264 — BSD-2-Clause (сборка из исходников, без патентного покрытия Cisco); LAME — LGPL.";

    // ----- footer / messages ------------------------------------------------
    FOOTER_IDLE => "openclip – screen recorder", "openclip – recorder de ecran", "openclip — запись экрана";
    WHAT_RECORDING => "Recording", "Înregistrarea", "Запись";
    WHAT_SNAPSHOT => "Snapshot", "Captura", "Снимок";
    MSG_SAVED => "{0} saved: {1} ({2})", "{0} a fost salvată: {1} ({2})", "{0} сохранена: {1} ({2})";
    MSG_SELECT_SOURCE_FIRST => "Select something to record first.", "Alege mai întâi ce vrei să înregistrezi.",
        "Сначала выберите, что записывать.";
    MSG_SELECT_CAPTURE_FIRST => "Select something to capture first.", "Alege mai întâi ce vrei să capturezi.",
        "Сначала выберите, что снимать.";
    MSG_CANCELLED => "Recording cancelled.", "Înregistrare anulată.", "Запись отменена.";
    MSG_START_FAILED => "Could not start recording: {0}", "Înregistrarea nu a putut porni: {0}",
        "Не удалось начать запись: {0}";
    MSG_RECORDING_FAILED => "Recording failed: {0}", "Înregistrarea a eșuat: {0}", "Ошибка записи: {0}";
    MSG_SNAPSHOT_FAILED => "Snapshot failed: {0}", "Captura a eșuat: {0}", "Не удалось сделать снимок: {0}";
    MSG_PICKER_FAILED => "Region picker: {0}", "Selectorul de regiune: {0}", "Выбор области: {0}";
    MSG_DELETED => "Deleted {0}", "Șters: {0}", "Удалено: {0}";
    MSG_DELETE_FAILED => "Could not delete: {0}", "Nu s-a putut șterge: {0}", "Не удалось удалить: {0}";

    // ----- delete dialog ---------------------------------------------------
    DELETE_TITLE => "Delete file?", "Ștergi fișierul?", "Удалить файл?";
    DELETE_BODY => "This permanently deletes the file from disk.", "Fișierul va fi șters definitiv de pe disc.",
        "Файл будет безвозвратно удалён с диска.";

    // ----- format dialog ---------------------------------------------------
    FMT_TITLE => "Format settings", "Setări de format", "Настройки формата";
    FMT_GROUP_FILE_TYPE => "File Type", "Tip de fișier", "Тип файла";
    FMT_LOCKED => "Settings cannot be changed while recording.", "Setările nu pot fi schimbate în timpul înregistrării.",
        "Настройки нельзя менять во время записи.";
    FMT_HELP => "[ Help ]", "[ Ajutor ]", "[ Справка ]";
    FMT_ROW_SIZE => "Size", "Dimensiune", "Размер";
    FMT_FULL_SIZE => "Full Size", "Dimensiune completă", "Полный размер";
    FMT_HALF_SIZE => "Half Size", "Jumătate de dimensiune", "Половина размера";
    FMT_CUSTOM_PERCENT => "Custom (W% × H%)", "Personalizat (L% × Î%)", "Свой (Ш% × В%)";
    FMT_CUSTOM_SIZE_LABEL => "Custom {0}% × {1}%", "Personalizat {0}% × {1}%", "Свой {0}% × {1}%";
    FMT_ROW_FPS => "FPS", "FPS", "Кадры/с";
    FMT_CUSTOM_FPS => "Custom ({0})", "Personalizat ({0})", "Свой ({0})";
    FMT_CUSTOM => "Custom…", "Personalizat…", "Свой…";
    FMT_ROW_CODEC => "Codec", "Codec", "Кодек";
    FMT_AUTO_TIP => "Uses a GPU encoder when one is available, otherwise OpenH264",
        "Folosește un codificator GPU dacă există, altfel OpenH264",
        "Использует кодировщик GPU, если он доступен, иначе OpenH264";
    FMT_ENCODER_MISSING => "This encoder was not found on this system", "Acest codificator nu a fost găsit pe acest sistem",
        "Этот кодировщик не найден в системе";
    FMT_NO_MF_ENCODERS => "No Media Foundation encoders found", "Niciun codificator Media Foundation găsit",
        "Кодировщики Media Foundation не найдены";
    FMT_MF_WINDOWS_ONLY => "Hardware encoders are only available on Windows",
        "Codificatoarele hardware sunt disponibile doar pe Windows",
        "Аппаратные кодировщики доступны только в Windows";
    FMT_ENCODER_DETAILS => "Encoder details", "Detalii codificator", "Сведения о кодировщике";
    FMT_ROW_QUALITY => "Quality", "Calitate", "Качество";
    FMT_QUALITY_BEST => "100 (best)", "100 (cea mai bună)", "100 (лучшее)";
    FMT_QUALITY_SMALLEST => "10 (smallest file)", "10 (fișier minim)", "10 (наименьший файл)";
    FMT_BITRATE_TIP => "Bitrate and keyframe settings", "Setări de bitrate și cadre-cheie",
        "Настройки битрейта и ключевых кадров";
    FMT_ROW_PROFILE => "Profile", "Profil", "Профиль";
    FMT_PROFILE_HELP => "Auto lets the encoder choose. Baseline plays everywhere but compresses worst; High gives the best quality per bitrate on modern players.",
        "Automat lasă codificatorul să aleagă. Baseline se redă oriunde, dar comprimă cel mai slab; High oferă cea mai bună calitate per bitrate pe playerele moderne.",
        "«Авто» оставляет выбор кодировщику. Baseline воспроизводится везде, но сжимает хуже всех; High даёт лучшее качество при том же битрейте на современных плеерах.";
    FMT_ROW_BITRATE => "Bitrate", "Bitrate", "Битрейт";
    FMT_AAC_NEEDS_WINDOWS => "AAC needs Windows (Media Foundation)", "AAC necesită Windows (Media Foundation)",
        "AAC требует Windows (Media Foundation)";
    FMT_PCM_AVI_ONLY => "PCM is only available in AVI", "PCM este disponibil doar în AVI", "PCM доступен только в AVI";
    FMT_ROW_CHANNELS => "Channels", "Canale", "Каналы";
    FMT_ROW_FREQUENCY => "Frequency", "Frecvență", "Частота";
    FMT_QUALITY_MODE => "Quality-based (bitrate derived from quality)", "În funcție de calitate (bitrate dedus din calitate)",
        "По качеству (битрейт выводится из качества)";
    FMT_CBR_MODE => "Constant bitrate", "Bitrate constant", "Постоянный битрейт";
    FMT_BITRATE_ESTIMATE => "≈ {0} kbps at {1}×{2}, {3} fps", "≈ {0} kbps la {1}×{2}, {3} fps",
        "≈ {0} кбит/с при {1}×{2}, {3} кадр/с";
    FMT_ROW_KEYFRAME => "Keyframe every", "Cadru-cheie la fiecare", "Ключевой кадр каждые";
    FMT_RATE_CONTROL_NOTE => "Quality mode lets the encoder vary the bitrate with the content (hardware encoders use their own quality rate control). Constant bitrate gives predictable file sizes.",
        "Modul calitate lasă codificatorul să varieze bitrate-ul în funcție de conținut (codificatoarele hardware folosesc propriul control al calității). Bitrate-ul constant oferă dimensiuni previzibile ale fișierelor.",
        "Режим качества позволяет кодировщику менять битрейт в зависимости от содержимого (аппаратные кодировщики используют собственное управление качеством). Постоянный битрейт даёт предсказуемый размер файла.";
    FMT_ROW_ENCODER => "Encoder", "Codificator", "Кодировщик";
    FMT_ROW_VENDOR => "Vendor", "Producător", "Производитель";
    FMT_ROW_HARDWARE => "Hardware", "Hardware", "Аппаратный";
    FMT_ROW_TRANSFORM => "Transform", "Transformare", "Преобразование";
    FMT_ROW_DETAILS => "Details", "Detalii", "Подробности";
    FMT_OPENH264_DETAILS => "Bundled OpenH264 (Cisco), software encoder on the CPU. Works everywhere; use a hardware encoder for high resolutions and frame rates.",
        "OpenH264 inclus (Cisco), codificator software pe procesor. Funcționează oriunde; folosește un codificator hardware pentru rezoluții și rate de cadre mari.",
        "Встроенный OpenH264 (Cisco), программный кодировщик на процессоре. Работает везде; для высоких разрешений и частоты кадров используйте аппаратный кодировщик.";
    FMT_NOT_FOUND => "Not found on this system.", "Nu a fost găsit pe acest sistem.", "Не найден в этой системе.";
    FMT_MF_COUNT => "{0} Media Foundation encoder(s) found.", "{0} codificator(oare) Media Foundation găsite.",
        "Найдено кодировщиков Media Foundation: {0}.";
    FMT_RESCAN => "Rescan encoders", "Rescanează codificatoarele", "Повторный поиск кодировщиков";

    // ----- format summaries (settings.rs) ----------------------------------
    CODEC_AUTO_GENERIC => "Auto (hardware H.264 if available)", "Automat (H.264 hardware dacă există)",
        "Авто (аппаратный H.264, если доступен)";
    CODEC_OPENH264 => "H264 (OpenH264, CPU)", "H264 (OpenH264, CPU)", "H264 (OpenH264, CPU)";
    CODEC_MF_GENERIC => "{0} (Media Foundation encoder)", "{0} (codificator Media Foundation)",
        "{0} (кодировщик Media Foundation)";
    CODEC_AUTO_RESOLVED => "Auto → {0}", "Automat → {0}", "Авто → {0}";
    QUALITY_LABEL => "quality {0}", "calitate {0}", "качество {0}";
    CBR_LABEL => "{0} kbps CBR", "{0} kbps CBR", "{0} кбит/с CBR";
    VIDEO_SUMMARY => "{0}, {1} fps, {2}{3}, {4} profile", "{0}, {1} fps, {2}{3}, profil {4}",
        "{0}, {1} кадр/с, {2}{3}, профиль {4}";
    VIDEO_SUMMARY_BITRATE => " (≈ {0} kbps)", " (≈ {0} kbps)", " (≈ {0} кбит/с)";
    AUDIO_SUMMARY_PCM => "{0}, {1}, 16-bit – {2}", "{0}, {1}, 16 biți – {2}", "{0}, {1}, 16 бит — {2}";
    AUDIO_SUMMARY => "{0}, {1}, {2}kbps – {3}", "{0}, {1}, {2}kbps – {3}", "{0}, {1}, {2} кбит/с — {3}";

    // ----- normalize() notes ------------------------------------------------
    NOTE_PCM_AVI_ONLY => "PCM audio is only available in AVI; using {0}.",
        "Sunetul PCM este disponibil doar în AVI; se folosește {0}.",
        "Звук PCM доступен только в AVI; используется {0}.";
    NOTE_AAC_NEEDS_MF => "AAC needs Windows Media Foundation; using MP3.",
        "AAC necesită Windows Media Foundation; se folosește MP3.",
        "AAC требует Windows Media Foundation; используется MP3.";
    NOTE_HEVC_MP4_ONLY => "HEVC is only written to MP4; using {0}.", "HEVC se scrie doar în MP4; se folosește {0}.",
        "HEVC записывается только в MP4; используется {0}.";
    NOTE_ENCODER_MISSING => "The selected {0} encoder is not available on this system; using OpenH264.",
        "Codificatorul {0} selectat nu este disponibil pe acest sistem; se folosește OpenH264.",
        "Выбранный кодировщик {0} недоступен в этой системе; используется OpenH264.";

    // ----- mini bar --------------------------------------------------------
    BAR_RECORDING_AREA => "Recording Area", "Zona înregistrată", "Область записи";
    BAR_INPUTS => "Recorded inputs", "Intrări înregistrate", "Записываемые входы";
    BAR_DIMENSIONS => "Dimensions", "Dimensiuni", "Размеры";
    BAR_PICK => "Pick…", "Alege…", "Выбрать…";
    BAR_SELECT => "Select…", "Alege…", "Выбрать…";
    BAR_DRAG_NEW_REGION => "Drag a new region", "Trage o regiune nouă", "Выделите новую область";
    BAR_REFRESH_TIP => "Refresh monitors and windows", "Reîmprospătează monitoarele și ferestrele",
        "Обновить мониторы и окна";
    BAR_FORMAT_TIP => "Format settings – {0} / {1}", "Setări de format – {0} / {1}", "Настройки формата — {0} / {1}";
    BAR_STARTING_IN => "Starting in {0}", "Începe în {0}", "Начало через {0}";
    BAR_ESC_CANCELS => "Esc cancels", "Esc anulează", "Esc — отмена";
    BAR_READY => "Ready", "Gata", "Готово";
    BAR_PICK_SOURCE => "Pick a source", "Alege o sursă", "Выберите источник";

    // ----- runtime status notes ---------------------------------------------
    NOTE_SYSTEM_AUDIO_UNAVAILABLE => "system audio unavailable: {0}", "sunetul de sistem nu este disponibil: {0}",
        "системный звук недоступен: {0}";
    NOTE_MIC_UNAVAILABLE => "microphone unavailable: {0}", "microfonul nu este disponibil: {0}",
        "микрофон недоступен: {0}";
    NOTE_AAC_UNAVAILABLE => "AAC unavailable ({0}); recorded MP3", "AAC indisponibil ({0}); s-a înregistrat MP3",
        "AAC недоступен ({0}); записан MP3";
    NOTE_AAC_NEEDS_WINDOWS => "AAC needs Windows; recorded MP3", "AAC necesită Windows; s-a înregistrat MP3",
        "AAC требует Windows; записан MP3";
    NOTE_ENCODER_NOT_FOUND => "the selected {0} encoder was not found", "codificatorul {0} selectat nu a fost găsit",
        "выбранный кодировщик {0} не найден";
    NOTE_ENCODER_FAILED => "{0} failed ({1})", "{0} a eșuat ({1})", "сбой {0} ({1})";
    NOTE_USING_ENCODER => "{0}; using {1}", "{0}; se folosește {1}", "{0}; используется {1}";
    NOTE_FELL_BACK_OPENH264 => "{0}; recorded H.264 with OpenH264", "{0}; s-a înregistrat H.264 cu OpenH264",
        "{0}; записан H.264 через OpenH264";
    NOTE_HW_NEEDS_WINDOWS => "{0} hardware encoding needs Windows", "codificarea hardware {0} necesită Windows",
        "аппаратное кодирование {0} требует Windows";

    // ----- region picker ---------------------------------------------------
    PICKER_TITLE => "Select region — drag to select, Esc to cancel",
        "Alege regiunea — trage pentru a selecta, Esc pentru anulare",
        "Выбор области — выделите мышью, Esc для отмены";
    PICKER_HINT => "Drag to select a region · Esc to cancel", "Trage pentru a selecta o regiune · Esc pentru anulare",
        "Выделите область мышью · Esc для отмены";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_resolves_in_every_language() {
        for key in ALL_KEYS {
            for lang in Lang::ALL {
                assert!(lookup(lang, key).is_some(), "{key} missing for {}", lang.code());
                assert!(!lookup(lang, key).unwrap().is_empty(), "{key} empty for {}", lang.code());
            }
        }
    }

    #[test]
    fn translations_keep_their_placeholders() {
        for key in ALL_KEYS {
            let en = lookup(Lang::En, key).unwrap();
            for i in 0..6 {
                let ph = format!("{{{i}}}");
                let expected = en.contains(&ph);
                for lang in [Lang::Ro, Lang::Ru] {
                    assert_eq!(
                        lookup(lang, key).unwrap().contains(&ph),
                        expected,
                        "{key}: {ph} mismatch in {}",
                        lang.code()
                    );
                }
            }
        }
    }

    #[test]
    fn language_codes_round_trip() {
        for lang in Lang::ALL {
            assert_eq!(Lang::from_code(lang.code()), Some(lang));
        }
        assert_eq!(Lang::from_code("ro-MD"), Some(Lang::Ro));
        assert_eq!(Lang::from_code("de"), None);
    }

    #[test]
    fn tf_substitutes_positionally() {
        set_lang(Lang::En);
        assert_eq!(tf(key::MSG_DELETED, &["a.mp4"]), "Deleted a.mp4");
        set_lang(Lang::Ro);
        assert_eq!(tf(key::MSG_DELETED, &["a.mp4"]), "Șters: a.mp4");
        set_lang(Lang::En);
    }
}
