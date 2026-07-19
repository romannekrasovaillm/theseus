//! Apply-patch: парсер и апплаер патчей в формате Codex (`*** Begin Patch` … `*** End Patch`).
//!
//! Формат (по образцу `codex-rs/apply-patch`):
//!
//! ```text
//! *** Begin Patch
//! *** Add File: path/to/new.txt
//! +строка 1
//! +строка 2
//! *** Update File: path/to/existing.txt
//! @@ fn foo():
//!  контекст
//! -старая строка
//! +новая строка
//! *** Delete File: path/to/old.txt
//! *** End Patch
//! ```
//!
//! Гарантии апплаера:
//! - запись атомарна (временный файл + `rename`), частично записанных файлов не бывает;
//! - пути с выходом за пределы `root` (абсолютные, `..`) отклоняются;
//! - для `Update` контекст ищется сначала точным совпадением блока,
//!   затем — с игнорированием хвостовых пробелов строк;
//! - ошибки содержат номер строки патча и пояснение.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Маркер начала патча.
const BEGIN_MARKER: &str = "*** Begin Patch";
/// Маркер конца патча.
const END_MARKER: &str = "*** End Patch";
/// Префикс операции добавления файла.
const ADD_PREFIX: &str = "*** Add File: ";
/// Префикс операции обновления файла.
const UPDATE_PREFIX: &str = "*** Update File: ";
/// Префикс операции удаления файла.
const DELETE_PREFIX: &str = "*** Delete File: ";
/// Префикс любой операции (для детекта границы секции).
const OP_PREFIX: &str = "*** ";

/// Одна операция патча.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchOp {
    /// Создать файл с заданным содержимым.
    Add {
        /// Относительный путь (от `root`).
        path: PathBuf,
        /// Полное содержимое файла (с финальным `\n`, если были строки).
        contents: String,
    },
    /// Обновить существующий файл по контекстным секциям.
    Update {
        /// Относительный путь (от `root`).
        path: PathBuf,
        /// Секции изменений, применяются по порядку.
        chunks: Vec<UpdateChunk>,
    },
    /// Удалить файл.
    Delete {
        /// Относительный путь (от `root`).
        path: PathBuf,
    },
}

/// Одна контекстная секция внутри `*** Update File`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateChunk {
    /// Якорь из строки `@@ <текст>` — сужает поиск (обычно сигнатура функции/класса).
    pub context: Option<String>,
    /// Строки, которые должны присутствовать в файле (контекст `' '` и удаляемые `'-'`).
    pub old_lines: Vec<String>,
    /// Строки, которыми заменяется блок (контекст `' '` и добавляемые `'+'`).
    pub new_lines: Vec<String>,
    /// Номер строки патча (1-based), где началась секция, — для сообщений об ошибках.
    pub patch_line: usize,
}

impl PatchOp {
    /// Относительный путь, затрагиваемый операцией.
    pub fn path(&self) -> &Path {
        match self {
            PatchOp::Add { path, .. } | PatchOp::Update { path, .. } | PatchOp::Delete { path } => {
                path
            }
        }
    }
}

/// Распарсить текст патча в список операций.
///
/// Проверяет только грамматику: обрамление `*** Begin/End Patch`, маркеры операций
/// и префиксы строк (`+`, `-`, `' '`, `@@`). Применимость к ФС здесь не проверяется.
///
/// # Ошибки
/// Возвращает ошибку с номером строки патча (1-based) при любом нарушении формата:
/// отсутствие обрамления, неизвестный маркер, пустая секция `Update` и т.п.
pub fn parse_patch(text: &str) -> Result<Vec<PatchOp>> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 2 {
        bail!("строка 1: патч пуст или слишком короткий; нужны маркеры '*** Begin Patch' и '*** End Patch'");
    }
    if lines[0].trim() != BEGIN_MARKER {
        bail!("строка 1: первая строка патча должна быть '{BEGIN_MARKER}'");
    }
    // Индекс последней строки обязан быть маркером конца (пустые хвостовые строки допускаем).
    let mut last = lines.len() - 1;
    while last > 0 && lines[last].trim().is_empty() {
        last -= 1;
    }
    if lines[last].trim() != END_MARKER {
        bail!(
            "строка {}: последняя строка патча должна быть '{END_MARKER}'",
            last + 1
        );
    }

    let mut ops = Vec::new();
    let mut i = 1;
    while i < last {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix(ADD_PREFIX) {
            let path = parse_path(rest, i + 1)?;
            i += 1;
            let mut body = Vec::new();
            while i < last && !lines[i].starts_with(OP_PREFIX) {
                let content = lines[i].strip_prefix('+').ok_or_else(|| {
                    anyhow::anyhow!(
                        "строка {}: в секции Add File '{}' строка должна начинаться с '+'",
                        i + 1,
                        path.display()
                    )
                })?;
                body.push(content);
                i += 1;
            }
            if body.is_empty() {
                bail!("строка {i}: секция Add File '{}' пуста", path.display());
            }
            let mut contents = body.join("\n");
            contents.push('\n');
            ops.push(PatchOp::Add { path, contents });
        } else if let Some(rest) = line.strip_prefix(DELETE_PREFIX) {
            let path = parse_path(rest, i + 1)?;
            ops.push(PatchOp::Delete { path });
            i += 1;
        } else if let Some(rest) = line.strip_prefix(UPDATE_PREFIX) {
            let path = parse_path(rest, i + 1)?;
            i += 1;
            let mut chunks = Vec::new();
            while i < last && !lines[i].starts_with(OP_PREFIX) {
                let chunk_start = i + 1;
                let mut context = None;
                if let Some(after) = lines[i].strip_prefix("@@") {
                    let anchor = after.trim();
                    if !anchor.is_empty() {
                        context = Some(anchor.to_string());
                    }
                    i += 1;
                }
                let mut old_lines = Vec::new();
                let mut new_lines = Vec::new();
                while i < last && !lines[i].starts_with(OP_PREFIX) && !lines[i].starts_with("@@") {
                    let l = lines[i];
                    // Пустая строка трактуется как контекстная строка с пустым содержимым.
                    match l.chars().next() {
                        Some(' ') => {
                            old_lines.push(l[1..].to_string());
                            new_lines.push(l[1..].to_string());
                        }
                        Some('-') => old_lines.push(l[1..].to_string()),
                        Some('+') => new_lines.push(l[1..].to_string()),
                        None => {
                            old_lines.push(String::new());
                            new_lines.push(String::new());
                        }
                        _ => bail!(
                            "строка {}: в секции Update File '{}' строка должна начинаться с ' ', '-' или '+'",
                            i + 1,
                            path.display()
                        ),
                    }
                    i += 1;
                }
                if old_lines.is_empty() && new_lines.is_empty() {
                    bail!(
                        "строка {chunk_start}: пустая секция изменений в Update File '{}'",
                        path.display()
                    );
                }
                chunks.push(UpdateChunk { context, old_lines, new_lines, patch_line: chunk_start });
            }
            if chunks.is_empty() {
                bail!("строка {i}: секция Update File '{}' не содержит изменений", path.display());
            }
            ops.push(PatchOp::Update { path, chunks });
        } else {
            bail!(
                "строка {}: неизвестный маркер; ожидались '{ADD_PREFIX}', '{UPDATE_PREFIX}' или '{DELETE_PREFIX}'",
                i + 1
            );
        }
    }
    Ok(ops)
}

/// Применить патч к файловой системе под корнем `root`.
///
/// Возвращает абсолютные пути всех затронутых файлов (в порядке операций).
/// Операции применяются последовательно; запись каждого файла атомарна
/// (временный файл в том же каталоге + `rename`). При ошибке на середине
/// ранее применённые операции не откатываются — это осознанное упрощение,
/// как и в эталонном `apply_patch` из Codex.
///
/// # Ошибки
/// - путь выходит за пределы `root` (абсолютный путь или `..`);
/// - `Update`/`Delete` несуществующего файла;
/// - контекст секции `Update` не найден (ошибка с номером строки патча);
/// - ошибки ввода-вывода с указанием пути.
pub fn apply_patch(text: &str, root: &Path) -> Result<Vec<PathBuf>> {
    let ops = parse_patch(text)?;
    apply_ops(&ops, root)
}

/// Применить уже распарсенные операции (см. [`apply_patch`]).
pub fn apply_ops(ops: &[PatchOp], root: &Path) -> Result<Vec<PathBuf>> {
    let mut touched = Vec::with_capacity(ops.len());
    for op in ops {
        let abs = safe_join(root, op.path())?;
        match op {
            PatchOp::Add { contents, .. } => {
                if let Some(parent) = abs.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("не удалось создать каталог {}", parent.display()))?;
                }
                atomic_write(&abs, contents.as_bytes())?;
            }
            PatchOp::Update { chunks, .. } => {
                let original = fs::read_to_string(&abs)
                    .with_context(|| format!("не удалось прочитать файл {} для обновления", abs.display()))?;
                let updated = apply_chunks(&original, chunks, op.path())?;
                atomic_write(&abs, updated.as_bytes())?;
            }
            PatchOp::Delete { .. } => {
                fs::remove_file(&abs)
                    .with_context(|| format!("не удалось удалить файл {}", abs.display()))?;
            }
        }
        touched.push(abs);
    }
    Ok(touched)
}

/// Извлечь и провалидировать путь из хвоста маркера операции.
fn parse_path(raw: &str, line_no: usize) -> Result<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("строка {line_no}: пустой путь в маркере операции");
    }
    Ok(PathBuf::from(trimmed))
}

/// Проверка path traversal: путь обязан быть относительным и без компонентов `..`.
fn safe_join(root: &Path, rel: &Path) -> Result<PathBuf> {
    if rel.as_os_str().is_empty() {
        bail!("пустой путь в операции патча");
    }
    for comp in rel.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                bail!("путь '{}' выходит за пределы рабочего корня (компонент '..')", rel.display())
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("абсолютный путь '{}' запрещён; указывайте пути относительно корня", rel.display())
            }
        }
    }
    Ok(root.join(rel))
}

/// Атомарная запись: временный файл в том же каталоге, fsync, затем rename.
/// При любой ошибке временный файл удаляется — частичного состояния не остаётся.
fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("некорректный путь для записи: {}", path.display()))?;
    let tmp = path.with_file_name(format!(".{}.patch-tmp-{}", file_name.to_string_lossy(), std::process::id()));
    let result = (|| -> Result<()> {
        {
            use std::io::Write;
            let mut f = fs::File::create(&tmp)
                .with_context(|| format!("не удалось создать временный файл {}", tmp.display()))?;
            f.write_all(contents)
                .with_context(|| format!("не удалось записать временный файл {}", tmp.display()))?;
            f.sync_all()
                .with_context(|| format!("не удалось сбросить на диск {}", tmp.display()))?;
        }
        fs::rename(&tmp, path).with_context(|| {
            format!("не удалось переименовать {} в {}", tmp.display(), path.display())
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Применить секции обновления к содержимому файла, вернуть новое содержимое.
fn apply_chunks(original: &str, chunks: &[UpdateChunk], rel_path: &Path) -> Result<String> {
    let had_trailing_nl = original.ends_with('\n') || original.is_empty();
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
    // Курсор: секции должны идти по файлу сверху вниз (как в эталонном apply_patch).
    let mut cursor = 0usize;
    for chunk in chunks {
        // Стартовая точка поиска — якорь `@@`, если он задан.
        let mut search_from = cursor;
        if let Some(anchor) = &chunk.context {
            match seek_sequence(&lines, std::slice::from_ref(anchor), cursor) {
                Some(idx) => search_from = idx + 1,
                None => bail!(
                    "строка {}: якорь '@@ {anchor}' не найден в файле '{}'",
                    chunk.patch_line,
                    rel_path.display()
                ),
            }
        }
        if chunk.old_lines.is_empty() {
            // Чистая вставка: на текущую позицию (после якоря либо от курсора).
            let at = search_from.min(lines.len());
            lines.splice(at..at, chunk.new_lines.iter().cloned());
            cursor = at + chunk.new_lines.len();
            continue;
        }
        match seek_sequence(&lines, &chunk.old_lines, search_from) {
            Some(idx) => {
                lines.splice(idx..idx + chunk.old_lines.len(), chunk.new_lines.iter().cloned());
                cursor = idx + chunk.new_lines.len();
            }
            None => {
                let preview: Vec<&str> = chunk.old_lines.iter().take(3).map(String::as_str).collect();
                bail!(
                    "строка {}: контекст не найден в файле '{}' (ищем блок из {} строк: {}{})",
                    chunk.patch_line,
                    rel_path.display(),
                    chunk.old_lines.len(),
                    preview.join(" / "),
                    if chunk.old_lines.len() > 3 { " …" } else { "" }
                )
            }
        }
    }
    let mut out = lines.join("\n");
    if had_trailing_nl && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// Найти подпоследовательность `pattern` в `lines` начиная с индекса `start`.
///
/// Двухпроходный поиск: сначала точное совпадение, затем — с игнорированием
/// хвостовых пробельных символов каждой строки (частый случай: редактор
/// подрезал trailing whitespace). Пустой `pattern` совпадает в позиции `start`.
fn seek_sequence(lines: &[String], pattern: &[String], start: usize) -> Option<usize> {
    if pattern.is_empty() {
        return (start <= lines.len()).then_some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let last_start = lines.len() - pattern.len();
    let from = start.min(last_start + 1);
    // Проход 1: точное совпадение.
    for i in from..=last_start {
        if lines[i..i + pattern.len()] == *pattern {
            return Some(i);
        }
    }
    // Проход 2: совпадение с trim окончаний строк.
    for i in from..=last_start {
        if lines[i..i + pattern.len()]
            .iter()
            .zip(pattern.iter())
            .all(|(have, want)| have.trim_end() == want.trim_end())
        {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Уникальный временный каталог на тест.
    fn temp_root(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "theseus-patch-test-{tag}-{}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read(root: &Path, rel: &str) -> String {
        fs::read_to_string(root.join(rel)).unwrap()
    }

    /// Нет ли в каталоге забытых временных файлов апплаера.
    fn no_tmp_left(root: &Path) -> bool {
        !fs::read_dir(root)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".patch-tmp-"))
    }

    // ---------- парсер ----------

    #[test]
    fn parse_add_file() {
        let ops = parse_patch("*** Begin Patch\n*** Add File: foo.txt\n+hello\n+world\n*** End Patch").unwrap();
        assert_eq!(
            ops,
            vec![PatchOp::Add {
                path: PathBuf::from("foo.txt"),
                contents: "hello\nworld\n".to_string()
            }]
        );
    }

    #[test]
    fn parse_delete_file() {
        let ops = parse_patch("*** Begin Patch\n*** Delete File: old.py\n*** End Patch").unwrap();
        assert_eq!(ops, vec![PatchOp::Delete { path: PathBuf::from("old.py") }]);
    }

    #[test]
    fn parse_update_with_context_anchor() {
        let patch = "*** Begin Patch\n\
                     *** Update File: a.py\n\
                     @@ def f():\n\
                     -    pass\n\
                     +    return 1\n\
                     *** End Patch";
        let ops = parse_patch(patch).unwrap();
        match &ops[0] {
            PatchOp::Update { path, chunks } => {
                assert_eq!(path, &PathBuf::from("a.py"));
                assert_eq!(chunks.len(), 1);
                assert_eq!(chunks[0].context.as_deref(), Some("def f():"));
                assert_eq!(chunks[0].old_lines, vec!["    pass"]);
                assert_eq!(chunks[0].new_lines, vec!["    return 1"]);
            }
            other => panic!("ожидался Update, получено {other:?}"),
        }
    }

    #[test]
    fn parse_update_multiple_chunks_and_context_lines() {
        // Raw-строка: ведущий пробел контекстной строки значим, `\`-продолжение его бы съело.
        let patch = r#"*** Begin Patch
*** Update File: a.txt
 keep
-gone
+born
@@
+tail
*** End Patch"#;
        let ops = parse_patch(patch).unwrap();
        match &ops[0] {
            PatchOp::Update { chunks, .. } => {
                assert_eq!(chunks.len(), 2);
                assert_eq!(chunks[0].old_lines, vec!["keep", "gone"]);
                assert_eq!(chunks[0].new_lines, vec!["keep", "born"]);
                assert!(chunks[1].context.is_none());
                assert!(chunks[1].old_lines.is_empty());
                assert_eq!(chunks[1].new_lines, vec!["tail"]);
            }
            other => panic!("ожидался Update, получено {other:?}"),
        }
    }

    #[test]
    fn parse_multi_hunk_patch() {
        let patch = "*** Begin Patch\n\
                     *** Add File: n.txt\n\
                     +x\n\
                     *** Delete File: d.txt\n\
                     *** Update File: u.txt\n\
                     -a\n\
                     +b\n\
                     *** End Patch";
        let ops = parse_patch(patch).unwrap();
        assert_eq!(ops.len(), 3);
        assert!(matches!(ops[0], PatchOp::Add { .. }));
        assert!(matches!(ops[1], PatchOp::Delete { .. }));
        assert!(matches!(ops[2], PatchOp::Update { .. }));
    }

    #[test]
    fn parse_rejects_missing_begin() {
        let err = parse_patch("*** Add File: f\n+x\n*** End Patch").unwrap_err();
        assert!(err.to_string().contains("строка 1"), "{err}");
    }

    #[test]
    fn parse_rejects_missing_end() {
        let err = parse_patch("*** Begin Patch\n*** Add File: f\n+x").unwrap_err();
        assert!(err.to_string().contains(END_MARKER), "{err}");
    }

    #[test]
    fn parse_error_has_line_number() {
        // Плохая строка внутри Add File — на 3-й строке патча.
        let err = parse_patch("*** Begin Patch\n*** Add File: f\noops\n*** End Patch").unwrap_err();
        assert!(err.to_string().contains("строка 3"), "{err}");
        // Пустая секция Update.
        let err = parse_patch("*** Begin Patch\n*** Update File: f\n*** End Patch").unwrap_err();
        assert!(err.to_string().contains("не содержит изменений"), "{err}");
    }

    // ---------- апплаер ----------

    #[test]
    fn apply_add_creates_file_with_dirs() {
        let root = temp_root("add");
        let patch = "*** Begin Patch\n*** Add File: sub/dir/new.txt\n+hello\n*** End Patch";
        let touched = apply_patch(patch, &root).unwrap();
        assert_eq!(read(&root, "sub/dir/new.txt"), "hello\n");
        assert_eq!(touched, vec![root.join("sub/dir/new.txt")]);
        assert!(no_tmp_left(&root));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn apply_update_exact_match() {
        let root = temp_root("upd");
        fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: a.txt\n two\n-three\n+THREE\n*** End Patch";
        apply_patch(patch, &root).unwrap();
        assert_eq!(read(&root, "a.txt"), "one\ntwo\nTHREE\n");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn apply_update_falls_back_to_trim_end() {
        let root = temp_root("trim");
        // В файле у целевых строк есть хвостовые пробелы, в патче — нет.
        fs::write(root.join("t.txt"), "alpha  \nbeta\t\ngamma\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: t.txt\n alpha\n-beta\n+BETA\n*** End Patch";
        apply_patch(patch, &root).unwrap();
        // Блок заменяется строками патча: хвостовые пробелы исходника не сохраняются.
        assert_eq!(read(&root, "t.txt"), "alpha\nBETA\ngamma\n");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn apply_update_anchor_selects_right_block() {
        let root = temp_root("anchor");
        fs::write(root.join("f.py"), "def a():\n    x = 1\n\ndef b():\n    x = 1\n").unwrap();
        let patch = "*** Begin Patch\n\
                     *** Update File: f.py\n\
                     @@ def b():\n\
                     -    x = 1\n\
                     +    x = 2\n\
                     *** End Patch";
        apply_patch(patch, &root).unwrap();
        assert_eq!(read(&root, "f.py"), "def a():\n    x = 1\n\ndef b():\n    x = 2\n");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn apply_update_pure_insertion() {
        let root = temp_root("ins");
        fs::write(root.join("i.txt"), "top\nbottom\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: i.txt\n top\n+middle\n*** End Patch";
        apply_patch(patch, &root).unwrap();
        assert_eq!(read(&root, "i.txt"), "top\nmiddle\nbottom\n");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn apply_update_context_failure_mentions_patch_line() {
        let root = temp_root("fail");
        fs::write(root.join("c.txt"), "actual\ncontent\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: c.txt\n-missing\n+new\n*** End Patch";
        let err = apply_patch(patch, &root).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("строка 3"), "{msg}");
        assert!(msg.contains("контекст не найден"), "{msg}");
        // Файл не тронут, временных файлов нет.
        assert_eq!(read(&root, "c.txt"), "actual\ncontent\n");
        assert!(no_tmp_left(&root));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn apply_update_missing_file_errors() {
        let root = temp_root("nofile");
        let patch = "*** Begin Patch\n*** Update File: absent.txt\n-a\n+b\n*** End Patch";
        let err = apply_patch(patch, &root).unwrap_err();
        assert!(format!("{err:#}").contains("absent.txt"), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn apply_delete_removes_file() {
        let root = temp_root("del");
        fs::write(root.join("gone.txt"), "bye\n").unwrap();
        let patch = "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch";
        let touched = apply_patch(patch, &root).unwrap();
        assert!(!root.join("gone.txt").exists());
        assert_eq!(touched, vec![root.join("gone.txt")]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn apply_multi_file_patch_end_to_end() {
        let root = temp_root("multi");
        fs::write(root.join("u.txt"), "keep\nold\n").unwrap();
        fs::write(root.join("d.txt"), "trash\n").unwrap();
        let patch = r#"*** Begin Patch
*** Add File: new.txt
+fresh
*** Update File: u.txt
 keep
-old
+new
*** Delete File: d.txt
*** End Patch"#;
        let touched = apply_patch(patch, &root).unwrap();
        assert_eq!(touched.len(), 3);
        assert_eq!(read(&root, "new.txt"), "fresh\n");
        assert_eq!(read(&root, "u.txt"), "keep\nnew\n");
        assert!(!root.join("d.txt").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    // ---------- безопасность путей ----------

    #[test]
    fn traversal_parent_dir_rejected() {
        let root = temp_root("trav");
        let patch = "*** Begin Patch\n*** Add File: ../evil.txt\n+boom\n*** End Patch";
        let err = apply_patch(patch, &root).unwrap_err();
        assert!(err.to_string().contains(".."), "{err}");
        assert!(!root.join("../evil.txt").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn traversal_absolute_path_rejected() {
        let root = temp_root("abs");
        let patch = "*** Begin Patch\n*** Delete File: /etc/passwd\n*** End Patch";
        let err = apply_patch(patch, &root).unwrap_err();
        assert!(err.to_string().contains("абсолютный путь"), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn traversal_nested_parent_rejected() {
        let root = temp_root("nest");
        let patch = "*** Begin Patch\n*** Update File: a/../../b.txt\n-x\n+y\n*** End Patch";
        let err = apply_patch(patch, &root).unwrap_err();
        assert!(err.to_string().contains(".."), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    // ---------- атомарность ----------

    #[test]
    fn atomic_write_leaves_no_partial_state_on_failure() {
        let root = temp_root("atomic");
        fs::write(root.join("ok.txt"), "v1\n").unwrap();
        // Первая операция успешна, вторая падает на отсутствующем файле.
        let patch = "*** Begin Patch\n\
                     *** Add File: created.txt\n\
                     +data\n\
                     *** Update File: nope.txt\n\
                     -a\n\
                     +b\n\
                     *** End Patch";
        let err = apply_patch(patch, &root).unwrap_err();
        assert!(format!("{err:#}").contains("nope.txt"), "{err}");
        // Успешная часть применена, частичных/временных файлов нигде нет.
        assert_eq!(read(&root, "created.txt"), "data\n");
        assert_eq!(read(&root, "ok.txt"), "v1\n");
        assert!(no_tmp_left(&root));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn atomic_write_overwrites_existing_file() {
        let root = temp_root("over");
        fs::write(root.join("o.txt"), "before\n").unwrap();
        let patch = "*** Begin Patch\n*** Add File: o.txt\n+after\n*** End Patch";
        apply_patch(patch, &root).unwrap();
        assert_eq!(read(&root, "o.txt"), "after\n");
        assert!(no_tmp_left(&root));
        fs::remove_dir_all(&root).unwrap();
    }

    // ---------- граничные случаи ----------

    #[test]
    fn update_preserves_missing_trailing_newline() {
        let root = temp_root("nonl");
        fs::write(root.join("n.txt"), "a\nb").unwrap();
        let patch = "*** Begin Patch\n*** Update File: n.txt\n-b\n+B\n*** End Patch";
        apply_patch(patch, &root).unwrap();
        assert_eq!(read(&root, "n.txt"), "a\nB");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn empty_patch_is_valid() {
        let ops = parse_patch("*** Begin Patch\n*** End Patch").unwrap();
        assert!(ops.is_empty());
        let root = temp_root("empty");
        let touched = apply_patch("*** Begin Patch\n*** End Patch", &root).unwrap();
        assert!(touched.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn seek_sequence_behaviour() {
        let lines: Vec<String> = ["a", "b ", "c"].iter().map(|s| (*s).to_string()).collect();
        let pat = vec!["b".to_string()];
        // Точного совпадения нет ("b " != "b"), но trim-проход находит индекс 1.
        assert_eq!(seek_sequence(&lines, &pat, 0), Some(1));
        // Курсор за целью — не находим.
        assert_eq!(seek_sequence(&lines, &pat, 2), None);
        // Пустой паттерн совпадает на курсоре.
        assert_eq!(seek_sequence(&lines, &[], 3), Some(3));
        // Паттерн длиннее файла — промах.
        let long = vec!["a".to_string(), "b".to_string(), "c".to_string(), "d".to_string()];
        assert_eq!(seek_sequence(&lines, &long, 0), None);
    }
}
