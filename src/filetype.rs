//! Детектор типа файла (образец — `file-search` из codex-rs: прежде чем
//! показывать файл агенту, искать по нему или вставлять в промпт, харнессу
//! нужно понять, текст это или бинарь, и на каком языке этот текст).
//!
//! Классификация трёхэтапная, от надёжного к эвристичному:
//!
//! 1. **Magic bytes** — сигнатуры в голове файла: ELF, PNG, JPEG, GIF,
//!    PDF, ZIP, gzip, SQLite. Содержимое сильнее имени: PNG-картинка,
//!    сохранённая как `avatar.txt`, всё равно распознаётся картинкой.
//! 2. **Расширение** — регистронезависимая таблица известных расширений:
//!    языки (`rs`, `py`, `ts`, ...), изображения, архивы, PDF, бинарники
//!    (`exe`, `wasm`, `db`, ...) и «просто текст» (`txt`, `log`, `csv`).
//! 3. **Эвристики по содержимому** — когда расширение не помогло: сначала
//!    шебанг первой строки (`#!/usr/bin/env python3` → [`Lang::Python`]),
//!    затем проверка на NUL-байт в первых 8 КиБ (есть NUL → бинарь), иначе
//!    файл считается «просто текстом» — [`Lang::Other`].
//!
//! Читается только голова файла (первые [`SNIFF_LEN`] байт), поэтому
//! детектор дёшев даже на огромных файлах. Любая ошибка чтения (файла
//! нет, это каталог, нет прав) даёт [`FileKind::Unknown`] — без паник.
//!
//! Известный компромисс: UTF-16-текст содержит NUL-байты и по эвристике
//! уезжает в [`FileKind::Binary`] — так же ведут себя `grep -I` и codex.

use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Сколько байт читаем от начала файла для сигнатур и эвристик.
const SNIFF_LEN: usize = 8 * 1024;

/// Язык текстового файла — для подсветки, markdown-фенсов и статистики.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    /// Rust (`.rs`).
    Rust,
    /// Python (`.py`, `.pyi`; `python3` в шебанге).
    Python,
    /// JavaScript (`.js`, `.mjs`, `.cjs`, `.jsx`; `node` в шебанге).
    Js,
    /// TypeScript (`.ts`, `.mts`, `.cts`, `.tsx`; `deno`/`tsx` в шебанге).
    Ts,
    /// TOML (`.toml`).
    Toml,
    /// JSON (`.json`, `.jsonl`, `.jsonc`).
    Json,
    /// Markdown (`.md`, `.markdown`).
    Md,
    /// YAML (`.yaml`, `.yml`).
    Yaml,
    /// C (`.c`, `.h`).
    C,
    /// C++ (`.cpp`, `.cxx`, `.cc`, `.hpp` и т.п.).
    Cpp,
    /// Go (`.go`).
    Go,
    /// POSIX-shell и диалекты (`.sh`, `.bash`, `.zsh`; `sh`/`bash` в шебанге).
    Shell,
    /// HTML (`.html`, `.htm`).
    Html,
    /// CSS (`.css`).
    Css,
    /// SQL (`.sql`).
    Sql,
    /// Текст без распознанного языка (`.txt`, `.log`, неизвестное расширение).
    Other,
}

/// Грубый класс файла: что с ним вообще можно делать в харнессе.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// Текстовый файл; вложенный [`Lang`] уточняет язык.
    Text(Lang),
    /// Бинарный файл (ELF, SQLite, NUL-байты, `exe`/`wasm`/`db` и т.п.).
    Binary,
    /// Растровое изображение (PNG, JPEG, GIF, ...).
    Image,
    /// Архив (ZIP, gzip, tar, 7z, ...).
    Archive,
    /// PDF-документ.
    Pdf,
    /// Тип не определён: файл не читается (отсутствует, каталог, нет прав).
    Unknown,
}

/// Определить тип файла по пути.
///
/// Порядок — magic bytes, расширение, шебанг, эвристика бинарности
/// (см. документацию модуля). Никогда не паникует: файл, который не
/// удалось открыть или прочитать, даёт [`FileKind::Unknown`].
pub fn detect(path: &Path) -> FileKind {
    let Some(head) = read_head(path) else {
        return FileKind::Unknown;
    };
    // 1. Сигнатуры в голове файла сильнее имени.
    if let Some(kind) = sniff_magic(&head) {
        return kind;
    }
    // 2. Известное расширение.
    if let Some(kind) = path
        .extension()
        .and_then(OsStr::to_str)
        .and_then(kind_from_extension)
    {
        return kind;
    }
    // 3. Шебанг первой строки (скрипты без расширения).
    if let Some(lang) = first_line(&head).and_then(sniff_shebang) {
        return FileKind::Text(lang);
    }
    // 4. Эвристика бинарности: NUL-байт в окне — верный признак бинаря.
    if looks_binary(&head) {
        FileKind::Binary
    } else {
        FileKind::Text(Lang::Other)
    }
}

/// Тег языка для markdown-фенса (```` ```rust ````).
///
/// Для [`Lang::Other`] возвращает `"text"` — явная пометка «просто текст»
/// вместо пустого фенса. Теги выбраны по каноническим имена́м подсветчиков
/// (GitHub Linguist / highlight.js): `javascript`, `typescript`, `bash`.
pub fn lang_tag(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => "rust",
        Lang::Python => "python",
        Lang::Js => "javascript",
        Lang::Ts => "typescript",
        Lang::Toml => "toml",
        Lang::Json => "json",
        Lang::Md => "markdown",
        Lang::Yaml => "yaml",
        Lang::C => "c",
        Lang::Cpp => "cpp",
        Lang::Go => "go",
        Lang::Shell => "bash",
        Lang::Html => "html",
        Lang::Css => "css",
        Lang::Sql => "sql",
        Lang::Other => "text",
    }
}

/// Текстовый ли это класс файла (то есть [`FileKind::Text`] с любым языком).
///
/// Текст можно показывать агенту как есть; `Binary`/`Image`/`Archive`/`Pdf`
/// требуют специальной обработки, а `Unknown` — аккуратного повторного
/// чтения перед показом.
pub fn is_text(kind: FileKind) -> bool {
    matches!(kind, FileKind::Text(_))
}

/// Распознать язык по шебангу первой строки (`#!/usr/bin/env python3`).
///
/// Принимает первую строку файла целиком; если она не начинается с `#!`,
/// возвращает `None`. Понимает два стиля:
///
/// * прямой путь — `#!/bin/bash`, `#!/usr/bin/python`;
/// * через `env` — `#!/usr/bin/env python3`, в том числе с опциями
///   (`#!/usr/bin/env -S python3 -u`): опции и присваивания `VAR=val`
///   пропускаются, берётся первый «настоящий» токен.
///
/// У интерпретатора отрезается версионный хвост (`python3.11` → `python`).
/// Незнакомые интерпретаторы (perl, ruby, ...) дают `None`.
pub fn sniff_shebang(first_line: &str) -> Option<Lang> {
    let body = first_line.strip_prefix("#!")?.trim();
    let mut words = body.split_whitespace();
    let first = words.next()?;
    let interpreter = if base_name(first) == "env" {
        words.find(|w| !w.starts_with('-') && !w.contains('='))?
    } else {
        first
    };
    lang_from_interpreter(base_name(interpreter))
}

/// Прочитать голову файла (до [`SNIFF_LEN`] байт); `None` при любой ошибке
/// ввода-вывода — отсутствующий файл, каталог, отказ в доступе.
fn read_head(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let mut buf = vec![0u8; SNIFF_LEN];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// Распознать формат по сигнатуре в голове файла.
///
/// Сигнатуры — по спецификациям форматов; для ZIP учитываются обе магии:
/// обычный архив (`PK\x03\x04`) и «пустой» (`PK\x05\x06`, только central
/// directory). Короткая голова просто не матчится — [`starts_with`] сам
/// проверяет длину.
///
/// [`starts_with`]: slice::starts_with
fn sniff_magic(head: &[u8]) -> Option<FileKind> {
    const ELF: &[u8] = b"\x7FELF";
    const PNG: &[u8] = b"\x89PNG";
    const JPEG: &[u8] = b"\xFF\xD8\xFF";
    const GIF: &[u8] = b"GIF8";
    const PDF: &[u8] = b"%PDF";
    const ZIP: &[u8] = b"PK\x03\x04";
    const ZIP_EMPTY: &[u8] = b"PK\x05\x06";
    const GZIP: &[u8] = b"\x1F\x8B";
    const SQLITE: &[u8] = b"SQLite format 3\x00";

    if head.starts_with(ELF) || head.starts_with(SQLITE) {
        Some(FileKind::Binary)
    } else if head.starts_with(PNG) || head.starts_with(JPEG) || head.starts_with(GIF) {
        Some(FileKind::Image)
    } else if head.starts_with(PDF) {
        Some(FileKind::Pdf)
    } else if head.starts_with(ZIP) || head.starts_with(ZIP_EMPTY) || head.starts_with(GZIP) {
        Some(FileKind::Archive)
    } else {
        None
    }
}

/// Распознать тип по расширению (уже без точки, в любом регистре).
///
/// Таблица намеренно консервативна: сюда входят только расширения, чей
/// класс не вызывает сомнений. Всё незнакомое возвращает `None`, и
/// [`detect`] уходит к шебангу и эвристике бинарности.
fn kind_from_extension(ext: &str) -> Option<FileKind> {
    let kind = match ext.to_ascii_lowercase().as_str() {
        // Языки.
        "rs" => FileKind::Text(Lang::Rust),
        "py" | "pyi" | "pyw" => FileKind::Text(Lang::Python),
        "js" | "mjs" | "cjs" | "jsx" => FileKind::Text(Lang::Js),
        "ts" | "mts" | "cts" | "tsx" => FileKind::Text(Lang::Ts),
        "toml" => FileKind::Text(Lang::Toml),
        "json" | "jsonl" | "jsonc" => FileKind::Text(Lang::Json),
        "md" | "markdown" => FileKind::Text(Lang::Md),
        "yaml" | "yml" => FileKind::Text(Lang::Yaml),
        "c" | "h" => FileKind::Text(Lang::C),
        "cpp" | "cxx" | "cc" | "c++" | "hpp" | "hxx" | "hh" => FileKind::Text(Lang::Cpp),
        "go" => FileKind::Text(Lang::Go),
        "sh" | "bash" | "zsh" => FileKind::Text(Lang::Shell),
        "html" | "htm" => FileKind::Text(Lang::Html),
        "css" => FileKind::Text(Lang::Css),
        "sql" => FileKind::Text(Lang::Sql),
        // Заведомо текстовые форматы без собственного языка.
        "txt" | "text" | "log" | "csv" | "tsv" | "xml" | "svg" | "ini" | "cfg" | "conf"
        | "diff" | "patch" => FileKind::Text(Lang::Other),
        // Изображения.
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "tif" | "tiff" => {
            FileKind::Image
        }
        // Архивы.
        "zip" | "jar" | "war" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "tar" => {
            FileKind::Archive
        }
        "pdf" => FileKind::Pdf,
        // Заведомо бинарные форматы.
        "exe" | "dll" | "so" | "dylib" | "o" | "obj" | "a" | "wasm" | "class" | "pyc" | "pyo"
        | "sqlite" | "sqlite3" | "db" | "bin" => FileKind::Binary,
        _ => return None,
    };
    Some(kind)
}

/// Сопоставить имя интерпретатора (basename, с возможной версией) языку.
fn lang_from_interpreter(name: &str) -> Option<Lang> {
    // Отрезаем версионный хвост: python3.11 → python, pypy3 → pypy.
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    match base {
        "python" | "pypy" => Some(Lang::Python),
        "sh" | "bash" | "zsh" | "dash" | "ash" | "ksh" => Some(Lang::Shell),
        "node" | "nodejs" => Some(Lang::Js),
        "deno" | "bun" | "tsx" | "ts-node" => Some(Lang::Ts),
        _ => None,
    }
}

/// Basename пути из шебанга: `/usr/bin/python3` → `python3`.
fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Первая строка головы файла как UTF-8; `None`, если строка не UTF-8.
fn first_line(head: &[u8]) -> Option<&str> {
    let end = head.iter().position(|&b| b == b'\n').unwrap_or(head.len());
    std::str::from_utf8(&head[..end]).ok()
}

/// Эвристика бинарности: NUL-байт в окне — признак бинарного файла
/// (как `grep -I`). UTF-16-текст при этом классифицируется бинарём —
/// осознанный компромисс, общий с codex.
fn looks_binary(head: &[u8]) -> bool {
    head.contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Минимальный tempdir без внешних крейтов: уникальное имя + чистка в Drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("theseus-filetype-{pid}-{n}-{nanos}"));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Создать файл с заданным содержимым во временном каталоге.
    fn write(dir: &TempDir, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, content).unwrap();
        path
    }

    // ------------------------------------------------------------------
    // Magic bytes
    // ------------------------------------------------------------------

    #[test]
    fn magic_elf_is_binary() {
        let dir = TempDir::new();
        let path = write(&dir, "tool", b"\x7FELF\x02\x01\x01\x00\xDE\xAD\xBE\xEF");
        assert_eq!(detect(&path), FileKind::Binary);
    }

    #[test]
    fn magic_sqlite_is_binary() {
        let dir = TempDir::new();
        let mut content = b"SQLite format 3\x00".to_vec();
        content.extend_from_slice(&[0u8; 64]);
        let path = write(&dir, "data.db", &content);
        assert_eq!(detect(&path), FileKind::Binary);
    }

    #[test]
    fn magic_png_is_image() {
        let dir = TempDir::new();
        let path = write(&dir, "logo", b"\x89PNG\x0D\x0A\x1A\x0A\x00\x00\x00\x0DIHDR");
        assert_eq!(detect(&path), FileKind::Image);
    }

    #[test]
    fn magic_jpeg_is_image() {
        let dir = TempDir::new();
        let path = write(&dir, "photo", b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01");
        assert_eq!(detect(&path), FileKind::Image);
    }

    #[test]
    fn magic_gif_is_image() {
        let dir = TempDir::new();
        let path = write(&dir, "anim", b"GIF89a\x01\x00\x01\x00\x80\x00\x00");
        assert_eq!(detect(&path), FileKind::Image);
    }

    #[test]
    fn magic_pdf_is_pdf() {
        let dir = TempDir::new();
        let path = write(&dir, "doc", b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n1 0 obj\n");
        assert_eq!(detect(&path), FileKind::Pdf);
    }

    #[test]
    fn magic_zip_is_archive() {
        let dir = TempDir::new();
        // Обычный архив.
        let zip = write(&dir, "bundle", b"PK\x03\x04\x14\x00\x00\x00\x08\x00");
        assert_eq!(detect(&zip), FileKind::Archive);
        // «Пустой» архив: только central directory.
        let empty = write(&dir, "empty", b"PK\x05\x06\x00\x00\x00\x00");
        assert_eq!(detect(&empty), FileKind::Archive);
    }

    #[test]
    fn magic_gzip_is_archive() {
        let dir = TempDir::new();
        let path = write(&dir, "dump", b"\x1F\x8B\x08\x00\x00\x00\x00\x00\x00\x03");
        assert_eq!(detect(&path), FileKind::Archive);
    }

    #[test]
    fn magic_beats_extension() {
        // PNG-содержимое в файле с текстовым расширением — всё равно картинка.
        let dir = TempDir::new();
        let path = write(&dir, "avatar.txt", b"\x89PNG\x0D\x0A\x1A\x0A");
        assert_eq!(detect(&path), FileKind::Image);
    }

    #[test]
    fn short_or_truncated_signature_falls_through() {
        // Два байта «PK» — ещё не ZIP; без расширения и NUL это текст.
        let dir = TempDir::new();
        let path = write(&dir, "stub", b"PK");
        assert_eq!(detect(&path), FileKind::Text(Lang::Other));
        // Три байта «\x7FEL» — ещё не ELF.
        let path = write(&dir, "stub2", b"\x7FEL");
        assert_eq!(detect(&path), FileKind::Text(Lang::Other));
    }

    // ------------------------------------------------------------------
    // Расширения
    // ------------------------------------------------------------------

    #[test]
    fn extension_text_languages() {
        let dir = TempDir::new();
        let cases: &[(&str, Lang)] = &[
            ("rs", Lang::Rust),
            ("py", Lang::Python),
            ("pyi", Lang::Python),
            ("js", Lang::Js),
            ("mjs", Lang::Js),
            ("jsx", Lang::Js),
            ("ts", Lang::Ts),
            ("tsx", Lang::Ts),
            ("toml", Lang::Toml),
            ("json", Lang::Json),
            ("md", Lang::Md),
            ("yaml", Lang::Yaml),
            ("yml", Lang::Yaml),
            ("c", Lang::C),
            ("h", Lang::C),
            ("cpp", Lang::Cpp),
            ("hpp", Lang::Cpp),
            ("go", Lang::Go),
            ("sh", Lang::Shell),
            ("bash", Lang::Shell),
            ("html", Lang::Html),
            ("css", Lang::Css),
            ("sql", Lang::Sql),
        ];
        for &(ext, lang) in cases {
            let name = format!("main.{ext}");
            let path = write(&dir, &name, b"plain ascii content\n");
            assert_eq!(
                detect(&path),
                FileKind::Text(lang),
                "расширение {ext} должно давать Text({lang:?})"
            );
        }
    }

    #[test]
    fn extension_nontext_kinds() {
        let dir = TempDir::new();
        let cases: &[(&str, FileKind)] = &[
            ("png", FileKind::Image),
            ("jpg", FileKind::Image),
            ("webp", FileKind::Image),
            ("zip", FileKind::Archive),
            ("gz", FileKind::Archive),
            ("tar", FileKind::Archive),
            ("7z", FileKind::Archive),
            ("pdf", FileKind::Pdf),
            ("exe", FileKind::Binary),
            ("wasm", FileKind::Binary),
            ("pyc", FileKind::Binary),
            ("db", FileKind::Binary),
        ];
        for &(ext, kind) in cases {
            let name = format!("blob.{ext}");
            // Текстовое содержимое нарочно: проверяем, что сработало имя.
            let path = write(&dir, &name, b"not really that format\n");
            assert_eq!(detect(&path), kind, "расширение {ext} должно давать {kind:?}");
        }
    }

    #[test]
    fn extension_plain_text_is_other() {
        let dir = TempDir::new();
        for ext in ["txt", "log", "csv", "xml"] {
            let name = format!("notes.{ext}");
            let path = write(&dir, &name, b"just some text\n");
            assert_eq!(
                detect(&path),
                FileKind::Text(Lang::Other),
                "расширение {ext} — «просто текст»"
            );
        }
    }

    #[test]
    fn extension_is_case_insensitive() {
        let dir = TempDir::new();
        let path = write(&dir, "MAIN.RS", b"fn main() {}\n");
        assert_eq!(detect(&path), FileKind::Text(Lang::Rust));
        let path = write(&dir, "Photo.JPG", b"not a real jpeg\n");
        assert_eq!(detect(&path), FileKind::Image);
    }

    #[test]
    fn extension_beats_shebang() {
        // Файл называется .sh, но внутри python-шебанг: имя важнее.
        let dir = TempDir::new();
        let path = write(&dir, "deploy.sh", b"#!/usr/bin/env python3\nprint(1)\n");
        assert_eq!(detect(&path), FileKind::Text(Lang::Shell));
    }

    #[test]
    fn extension_beats_nul_heuristic() {
        // Известное текстовое расширение проверяется раньше NUL-эвристики.
        let dir = TempDir::new();
        let path = write(&dir, "weird.py", b"ab\x00cd\n");
        assert_eq!(detect(&path), FileKind::Text(Lang::Python));
    }

    // ------------------------------------------------------------------
    // Эвристика бинарности по NUL
    // ------------------------------------------------------------------

    #[test]
    fn nul_byte_marks_binary() {
        let dir = TempDir::new();
        let path = write(&dir, "blob", b"hello\x00world\n");
        assert_eq!(detect(&path), FileKind::Binary);
    }

    #[test]
    fn nul_at_last_sniffed_byte_marks_binary() {
        // NUL на последнем байте окна ещё попадает в проверку.
        let dir = TempDir::new();
        let mut content = vec![b'a'; SNIFF_LEN - 1];
        content.push(0);
        let path = write(&dir, "edge", &content);
        assert_eq!(detect(&path), FileKind::Binary);
    }

    #[test]
    fn nul_after_sniff_window_is_ignored() {
        // NUL за пределами окна уже не виден: читаем только первые 8 КиБ.
        let dir = TempDir::new();
        let mut content = vec![b'a'; SNIFF_LEN];
        content.push(0);
        let path = write(&dir, "long", &content);
        assert_eq!(detect(&path), FileKind::Text(Lang::Other));
    }

    #[test]
    fn utf16_text_marks_binary() {
        // Задокументированный компромисс: UTF-16 содержит NUL-байты.
        let dir = TempDir::new();
        let path = write(&dir, "wide", b"H\x00i\x00 \x00there\x00");
        assert_eq!(detect(&path), FileKind::Binary);
    }

    // ------------------------------------------------------------------
    // Шебанг
    // ------------------------------------------------------------------

    #[test]
    fn shebang_python_via_env() {
        let dir = TempDir::new();
        let path = write(&dir, "script", b"#!/usr/bin/env python3\nprint(1)\n");
        assert_eq!(detect(&path), FileKind::Text(Lang::Python));
    }

    #[test]
    fn shebang_bash_direct() {
        let dir = TempDir::new();
        let path = write(&dir, "run", b"#!/bin/bash\nset -euo pipefail\n");
        assert_eq!(detect(&path), FileKind::Text(Lang::Shell));
    }

    #[test]
    fn shebang_node_via_env() {
        let dir = TempDir::new();
        let path = write(&dir, "tool", b"#!/usr/bin/env node\nconsole.log(1)\n");
        assert_eq!(detect(&path), FileKind::Text(Lang::Js));
    }

    #[test]
    fn shebang_deno_via_env_with_options() {
        let dir = TempDir::new();
        let path = write(&dir, "serve", b"#!/usr/bin/env -S deno run --allow-net\n");
        assert_eq!(detect(&path), FileKind::Text(Lang::Ts));
    }

    #[test]
    fn sniff_shebang_variants() {
        let cases: &[(&str, Option<Lang>)] = &[
            ("#!/bin/sh", Some(Lang::Shell)),
            ("#!/usr/bin/zsh", Some(Lang::Shell)),
            ("#!/usr/bin/python", Some(Lang::Python)),
            ("#!/usr/bin/env python3.11", Some(Lang::Python)),
            ("#!/usr/bin/env -S python3 -u", Some(Lang::Python)),
            ("#!/usr/bin/env node", Some(Lang::Js)),
            ("#!/usr/bin/env -S deno run", Some(Lang::Ts)),
            ("#! /bin/bash", Some(Lang::Shell)),
            // Незнакомые интерпретаторы и мусор — None.
            ("#!/usr/bin/perl", None),
            ("#!/usr/bin/env", None),
            ("#!", None),
            ("", None),
            ("just a comment #!/bin/sh", None),
        ];
        for &(line, expected) in cases {
            assert_eq!(sniff_shebang(line), expected, "строка: {line:?}");
        }
    }

    // ------------------------------------------------------------------
    // Прочие случаи detect()
    // ------------------------------------------------------------------

    #[test]
    fn no_extension_plain_text_is_other() {
        let dir = TempDir::new();
        let path = write(&dir, "README", b"Theseus is an agent harness.\n");
        assert_eq!(detect(&path), FileKind::Text(Lang::Other));
    }

    #[test]
    fn empty_files() {
        let dir = TempDir::new();
        // Пустой файл без расширения — «просто текст» (NUL-а нет).
        let bare = write(&dir, "empty", b"");
        assert_eq!(detect(&bare), FileKind::Text(Lang::Other));
        // Пустой файл с расширением классифицируется по имени.
        let rust = write(&dir, "empty.rs", b"");
        assert_eq!(detect(&rust), FileKind::Text(Lang::Rust));
    }

    #[test]
    fn missing_file_is_unknown() {
        let dir = TempDir::new();
        let path = dir.path().join("no-such-file.rs");
        // Не паникает, возвращает Unknown.
        assert_eq!(detect(&path), FileKind::Unknown);
    }

    #[test]
    fn directory_is_unknown() {
        let dir = TempDir::new();
        assert_eq!(detect(dir.path()), FileKind::Unknown);
    }

    // ------------------------------------------------------------------
    // lang_tag / is_text
    // ------------------------------------------------------------------

    #[test]
    fn lang_tag_all_variants() {
        let cases: &[(Lang, &str)] = &[
            (Lang::Rust, "rust"),
            (Lang::Python, "python"),
            (Lang::Js, "javascript"),
            (Lang::Ts, "typescript"),
            (Lang::Toml, "toml"),
            (Lang::Json, "json"),
            (Lang::Md, "markdown"),
            (Lang::Yaml, "yaml"),
            (Lang::C, "c"),
            (Lang::Cpp, "cpp"),
            (Lang::Go, "go"),
            (Lang::Shell, "bash"),
            (Lang::Html, "html"),
            (Lang::Css, "css"),
            (Lang::Sql, "sql"),
            (Lang::Other, "text"),
        ];
        for &(lang, tag) in cases {
            assert_eq!(lang_tag(lang), tag, "язык {lang:?}");
        }
    }

    #[test]
    fn is_text_classification() {
        for kind in [FileKind::Text(Lang::Rust), FileKind::Text(Lang::Other)] {
            assert!(is_text(kind), "{kind:?} — текст");
        }
        for kind in [
            FileKind::Binary,
            FileKind::Image,
            FileKind::Archive,
            FileKind::Pdf,
            FileKind::Unknown,
        ] {
            assert!(!is_text(kind), "{kind:?} — не текст");
        }
    }
}
