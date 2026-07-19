//! История ввода для TUI (образец — reedline и `~/.bash_history`).
//!
//! [`InputHistory`] хранит однострочные команды пользователя в [`VecDeque`]
//! ограниченной ёмкости: при переполнении старейшие записи вытесняются.
//! Возможности:
//!
//! - `push` с нормализацией (замена `\n`/`\r` на пробел, trim), пропуском
//!   пустых строк и дедупликацией подряд идущих дублей;
//! - навигация «вверх/вниз» с черновиком: неотправленное содержимое поля
//!   ввода сохраняется при первом листании и восстанавливается при спуске
//!   «ниже» самой новой записи (поведение bash/readline);
//! - префиксный поиск [`InputHistory::search_prefix`] — новые записи первыми;
//! - персистентность в текстовом файле: одна строка = одна запись
//!   (формат совместим с `~/.bash_history`);
//! - статистика использования [`HistoryStats`] для статус-строки TUI.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::Path;

/// Ёмкость истории по умолчанию — разумный дефолт для интерактивной сессии
/// (сопоставимо с типичным `HISTSIZE` у shell'ов).
pub const DEFAULT_CAPACITY: usize = 1000;

// === История ===

/// История ввода: ограниченный буфер однострочных команд с навигацией
/// и черновиком.
///
/// Инварианты (поддерживаются всеми методами):
/// - записи непустые и без ведущих/хвостовых пробелов;
/// - записи не содержат `\n`/`\r` (история однострочная);
/// - подряд идущие записи различны (дедупликация в [`InputHistory::push`]).
///
/// # Пример
///
/// ```
/// use theseus::history::InputHistory;
///
/// let mut history = InputHistory::new(100);
/// history.push("cargo build");
/// history.push("cargo test");
///
/// // «Вверх»: показана последняя команда, набранный текст ушёл в черновик.
/// assert_eq!(history.prev("cargo b"), Some("cargo test"));
/// // «Вниз» за самую новую запись — черновик восстановлен.
/// assert_eq!(history.next(), Some("cargo b"));
/// ```
#[derive(Debug, Clone)]
pub struct InputHistory {
    /// Записи от старых к новым (`back` — самая свежая).
    entries: VecDeque<String>,
    /// Максимальное число записей; 0 — «история отключена» (ничего не хранится).
    capacity: usize,
    /// Позиция листания: `None` — навигация не активна; `Some(i)` — сейчас
    /// показана `entries[i]`.
    cursor: Option<usize>,
    /// Черновик — содержимое поля ввода на момент начала листания.
    draft: String,
    /// Принято записей за время жизни экземпляра (включая `load`).
    pushed: u64,
    /// Отклонено при `push` (пустые после нормализации и дубли подряд).
    skipped: u64,
    /// Вытеснено старейших записей из-за переполнения (включая `load`).
    evicted: u64,
}

impl InputHistory {
    /// Пустая история с ёмкостью `capacity`.
    ///
    /// `capacity == 0` легален: история ничего не хранит (аналог `HISTSIZE=0`
    /// в bash) — `push` принимает запись, но она тут же вытесняется.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity,
            cursor: None,
            draft: String::new(),
            pushed: 0,
            skipped: 0,
            evicted: 0,
        }
    }

    /// История с ёмкостью [`DEFAULT_CAPACITY`].
    #[must_use]
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }

    /// Добавить команду в историю. Возвращает `true`, если запись принята.
    ///
    /// Нормализация: `\n` и `\r` заменяются на пробел (история однострочная),
    /// затем trim. Пустые после нормализации строки и дубли последней записи
    /// отклоняются (возвращается `false`). При переполнении старейшая запись
    /// вытесняется. Любой `push` — принятый или нет — сбрасывает навигацию
    /// и черновик (как submit в bash).
    pub fn push(&mut self, raw: &str) -> bool {
        let normalized = raw.replace(['\n', '\r'], " ");
        let line = normalized.trim();
        let is_dup = self.entries.back().map(String::as_str) == Some(line);
        self.cursor = None;
        self.draft.clear();
        if line.is_empty() || is_dup {
            self.skipped += 1;
            return false;
        }
        self.entries.push_back(line.to_string());
        self.pushed += 1;
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
            self.evicted += 1;
        }
        true
    }

    /// Листание «назад» (клавиша Up). `current` — текущее содержимое поля
    /// ввода: при первом листании оно сохраняется как черновик и будет
    /// восстановлено [`InputHistory::next`] при спуске «ниже» самой новой
    /// записи. При уже активной навигации `current` игнорируется.
    ///
    /// Возвращает запись для отображения или `None`, если дальше идти некуда
    /// (история пуста или уже показана самая старая запись — тогда позиция
    /// листания не меняется).
    pub fn prev(&mut self, current: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        match self.cursor {
            None => {
                self.draft.clear();
                self.draft.push_str(current);
                let last = self.entries.len() - 1;
                self.cursor = Some(last);
                self.entries.get(last).map(String::as_str)
            }
            Some(0) => None,
            Some(i) => {
                let older = i - 1;
                self.cursor = Some(older);
                self.entries.get(older).map(String::as_str)
            }
        }
    }

    /// Листание «вперёд» (клавиша Down).
    ///
    /// Возвращает следующую (более новую) запись; при спуске «ниже» самой
    /// новой — сохранённый черновик, при этом навигация завершается.
    /// `None`, если навигация не активна (поле ввода и так актуально).
    // Имя `next` — осознанное: парное к `prev` API навигации, как в reedline;
    // история не является итератором, путаницы с `Iterator::next` не будет.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<&str> {
        match self.cursor {
            None => None,
            Some(i) if i + 1 < self.entries.len() => {
                let newer = i + 1;
                self.cursor = Some(newer);
                self.entries.get(newer).map(String::as_str)
            }
            Some(_) => {
                self.cursor = None;
                Some(self.draft.as_str())
            }
        }
    }

    /// Сбросить навигацию (например, по Esc): курсор в «не листаем», черновик
    /// очищается. [`InputHistory::push`] делает это сам.
    pub fn reset_navigation(&mut self) {
        self.cursor = None;
        self.draft.clear();
    }

    /// Записи, начинающиеся с `query`, от новых к старым.
    ///
    /// Пустой `query` возвращает всю историю (новые первыми). Сравнение
    /// посимвольное, регистрозависимое, юникод-корректное.
    #[must_use]
    pub fn search_prefix(&self, query: &str) -> Vec<&str> {
        self.entries
            .iter()
            .rev()
            .filter(|entry| entry.starts_with(query))
            .map(String::as_str)
            .collect()
    }

    /// Итератор по записям от старых к новым.
    pub fn iter(&self) -> impl Iterator<Item = &str> + '_ {
        self.entries.iter().map(String::as_str)
    }

    /// Число хранимых записей.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Пуста ли история.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Максимальная ёмкость, заданная при создании.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Активна ли навигация (между первым `prev` и возвратом к черновику).
    #[must_use]
    pub fn is_navigating(&self) -> bool {
        self.cursor.is_some()
    }

    /// Снимок статистики использования (для статус-строки TUI / диагностики).
    #[must_use]
    pub fn stats(&self) -> HistoryStats {
        HistoryStats {
            len: self.entries.len(),
            capacity: self.capacity,
            total_bytes: self.entries.iter().map(String::len).sum(),
            pushed: self.pushed,
            skipped: self.skipped,
            evicted: self.evicted,
        }
    }

    /// Загрузить историю из текстового файла (одна строка = одна запись,
    /// формат `~/.bash_history`).
    ///
    /// Строки проходят ту же нормализацию, что и при [`InputHistory::push`]:
    /// trim, пропуск пустых, дедупликация подряд идущих; если строк больше
    /// `capacity`, сохраняется «хвост» файла (самые новые записи).
    ///
    /// История — некритичные данные, поэтому функция **не паникует и не
    /// возвращает ошибку**: отсутствующий, недоступный, каталог вместо файла
    /// или битый (не UTF-8) файл дают пустую историю.
    #[must_use]
    pub fn load(path: impl AsRef<Path>, capacity: usize) -> Self {
        let mut history = Self::new(capacity);
        let Ok(bytes) = fs::read(path) else {
            return history;
        };
        // Битый UTF-8 — тоже «нет истории», а не повод упасть.
        let Ok(text) = String::from_utf8(bytes) else {
            return history;
        };
        for line in text.lines() {
            history.push(line);
        }
        history
    }

    /// Сохранить историю в текстовый файл (одна строка = одна запись).
    ///
    /// Существующий файл перезаписывается. Каталоги-предки не создаются —
    /// отсутствующий каталог приведёт к ошибке создания файла.
    ///
    /// # Errors
    /// Ошибка ввода-вывода при создании, записи или сбросе буфера файла.
    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let file = fs::File::create(path)?;
        let mut writer = BufWriter::new(file);
        for entry in &self.entries {
            writeln!(writer, "{entry}")?;
        }
        writer.flush()
    }
}

// === Статистика ===

/// Снимок состояния и счётчиков истории (см. [`InputHistory::stats`]).
///
/// Счётчики накопительные: ведутся с момента создания экземпляра и учитывают
/// в том числе записи, прошедшие через [`InputHistory::load`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryStats {
    /// Число хранимых записей сейчас.
    pub len: usize,
    /// Максимальная ёмкость.
    pub capacity: usize,
    /// Суммарный размер записей в байтах (UTF-8).
    pub total_bytes: usize,
    /// Принято записей за время жизни экземпляра.
    pub pushed: u64,
    /// Отклонено при `push`: пустые после нормализации и дубли подряд.
    pub skipped: u64,
    /// Вытеснено старейших записей из-за переполнения.
    pub evicted: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Уникальный путь во временном каталоге (тесты бегут параллельно,
    /// поэтому pid + тег + атомарный счётчик).
    fn temp_path(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "theseus_history_{}_{tag}_{n}.txt",
            std::process::id()
        ))
    }

    /// RAII-удаление временного файла по окончании теста.
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(tag: &str) -> Self {
            Self(temp_path(tag))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    /// Собрать записи в вектор для сравнения с эталоном (старые → новые).
    fn entries(history: &InputHistory) -> Vec<&str> {
        history.iter().collect()
    }

    #[test]
    fn push_trims_and_stores() {
        let mut h = InputHistory::new(10);
        assert!(h.push("  ls -la  "));
        assert_eq!(entries(&h), ["ls -la"]);
        assert_eq!(h.len(), 1);
        assert!(!h.is_empty());
    }

    #[test]
    fn push_skips_empty_and_blank() {
        let mut h = InputHistory::new(10);
        assert!(!h.push(""));
        assert!(!h.push("   "));
        assert!(!h.push("\n \n\t\n")); // после замены \n — одни пробелы
        assert!(h.is_empty());
        assert_eq!(h.stats().skipped, 3);
        assert_eq!(h.stats().pushed, 0);
    }

    #[test]
    fn push_replaces_newlines_with_spaces() {
        let mut h = InputHistory::new(10);
        // Многострочная вставка из буфера обмена склеивается в одну строку;
        // внутренние пробелы не схлопываются — только trim по краям.
        assert!(h.push("echo one\ntwo\r\nthree"));
        assert_eq!(entries(&h), ["echo one two  three"]);
    }

    #[test]
    fn push_dedups_only_consecutive_duplicates() {
        let mut h = InputHistory::new(10);
        assert!(h.push("ls"));
        assert!(!h.push("  ls  ")); // дубль после trim — тоже дубль
        assert!(h.push("pwd"));
        assert!(h.push("ls")); // уже не подряд — принимается
        assert_eq!(entries(&h), ["ls", "pwd", "ls"]);
        assert_eq!(h.stats().skipped, 1);
    }

    #[test]
    fn capacity_zero_stores_nothing() {
        let mut h = InputHistory::new(0);
        assert_eq!(h.capacity(), 0);
        assert!(h.push("anything")); // принята, но сразу вытеснена
        assert!(h.is_empty());
        assert_eq!(h.stats().evicted, 1);
        assert_eq!(h.prev("x"), None);
    }

    #[test]
    fn overflow_evicts_oldest_first() {
        let mut h = InputHistory::new(3);
        for i in 1..=5 {
            assert!(h.push(&format!("cmd{i}")));
        }
        assert_eq!(h.len(), 3);
        assert_eq!(entries(&h), ["cmd3", "cmd4", "cmd5"]);
        let st = h.stats();
        assert_eq!(st.pushed, 5);
        assert_eq!(st.evicted, 2);
    }

    #[test]
    fn prev_walks_backwards_and_stops_at_oldest() {
        let mut h = InputHistory::new(10);
        h.push("a");
        h.push("b");
        h.push("c");
        assert_eq!(h.prev("черновик"), Some("c"));
        assert_eq!(h.prev("игнорируется"), Some("b"));
        assert_eq!(h.prev(""), Some("a"));
        // Дальше некуда: None, но позиция не съезжает — «вниз» идём с «a».
        assert_eq!(h.prev(""), None);
        assert!(h.is_navigating());
        assert_eq!(h.next(), Some("b"));
    }

    #[test]
    fn next_past_newest_restores_draft() {
        let mut h = InputHistory::new(10);
        h.push("one");
        h.push("two");
        assert_eq!(h.prev("набрано, но не отправлено"), Some("two"));
        assert_eq!(h.prev(""), Some("one"));
        assert_eq!(h.next(), Some("two"));
        // Спуск «ниже» самой новой записи — черновик на месте.
        assert_eq!(h.next(), Some("набрано, но не отправлено"));
        assert!(!h.is_navigating());
        // Навигация завершена — дальше next молчит.
        assert_eq!(h.next(), None);
    }

    #[test]
    fn draft_is_empty_when_nothing_was_typed() {
        let mut h = InputHistory::new(10);
        h.push("a");
        assert_eq!(h.prev(""), Some("a"));
        assert_eq!(h.next(), Some(""));
    }

    #[test]
    fn navigation_on_empty_history_returns_none() {
        let mut h = InputHistory::new(5);
        assert_eq!(h.prev("x"), None);
        assert!(!h.is_navigating());
        assert_eq!(h.next(), None);
    }

    #[test]
    fn push_resets_navigation_and_clears_draft() {
        let mut h = InputHistory::new(10);
        h.push("a");
        h.push("b");
        assert_eq!(h.prev("старый черновик"), Some("b"));
        assert!(h.is_navigating());
        // Submit во время листания: навигация сброшена, черновик забыт.
        h.push("c");
        assert!(!h.is_navigating());
        assert_eq!(h.next(), None);
        // Новое листание сохраняет новый current, а не старый черновик.
        assert_eq!(h.prev("новый"), Some("c"));
        assert_eq!(h.next(), Some("новый"));
    }

    #[test]
    fn reset_navigation_clears_state() {
        let mut h = InputHistory::new(10);
        h.push("a");
        assert_eq!(h.prev("draft"), Some("a"));
        assert!(h.is_navigating());
        h.reset_navigation();
        assert!(!h.is_navigating());
        assert_eq!(h.next(), None);
        // Черновик забыт: повторное листание сохраняет новый current.
        assert_eq!(h.prev("другое"), Some("a"));
        assert_eq!(h.next(), Some("другое"));
    }

    #[test]
    fn search_prefix_returns_newest_first_and_handles_unicode() {
        let mut h = InputHistory::new(10);
        h.push("cargo build");
        h.push("ls -la");
        h.push("cargo test");
        h.push("эхо привет");
        assert_eq!(h.search_prefix("cargo"), ["cargo test", "cargo build"]);
        assert_eq!(h.search_prefix("эхо"), ["эхо привет"]);
        assert!(h.search_prefix("git").is_empty());
        // Префикс — не подстрока: «test» внутри «cargo test» не ищется.
        assert!(h.search_prefix("test").is_empty());
    }

    #[test]
    fn search_prefix_empty_query_returns_all_newest_first() {
        let mut h = InputHistory::new(10);
        h.push("first");
        h.push("second");
        assert_eq!(h.search_prefix(""), ["second", "first"]);
        assert!(InputHistory::new(5).search_prefix("").is_empty());
    }

    #[test]
    fn load_missing_directory_or_broken_utf8_gives_empty_without_panic() {
        // Несуществующий путь.
        let h = InputHistory::load(temp_path("no_such_file"), 10);
        assert!(h.is_empty());
        // Каталог вместо файла — fs::read вернёт ошибку.
        let h = InputHistory::load(std::env::temp_dir(), 10);
        assert!(h.is_empty());
        // Битый UTF-8 в существующем файле.
        let tmp = TempFile::new("broken");
        fs::write(tmp.path(), [0xFF, 0xFE, 0x30, 0x0A, 0x80]).unwrap();
        let h = InputHistory::load(tmp.path(), 10);
        assert!(h.is_empty());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let mut h = InputHistory::new(100);
        h.push("cargo build");
        h.push("cargo test");
        h.push("git commit -m 'сообщение'");
        let tmp = TempFile::new("roundtrip");
        h.save(tmp.path()).unwrap();

        let loaded = InputHistory::load(tmp.path(), 100);
        assert_eq!(entries(&loaded), entries(&h));
        assert_eq!(loaded.len(), 3);
    }

    #[test]
    fn load_keeps_newest_tail_and_skips_blanks_and_dups() {
        let tmp = TempFile::new("tail");
        // Пустые строки и CRLF-окончания обрабатываются; «cmd2» продублирована.
        fs::write(tmp.path(), "\r\n\ncmd1\r\ncmd2\ncmd2\ncmd3\ncmd4\n").unwrap();
        let h = InputHistory::load(tmp.path(), 2);
        assert_eq!(entries(&h), ["cmd3", "cmd4"]);
        let st = h.stats();
        assert_eq!(st.pushed, 4);
        assert_eq!(st.skipped, 3); // 2 пустые + 1 дубль
        assert_eq!(st.evicted, 2);
    }

    #[test]
    fn load_empty_file_gives_empty_history() {
        let tmp = TempFile::new("empty");
        fs::write(tmp.path(), "").unwrap();
        let h = InputHistory::load(tmp.path(), 10);
        assert!(h.is_empty());
        assert_eq!(h.stats().pushed, 0);
    }

    #[test]
    fn save_into_missing_directory_errors_without_panic() {
        let mut h = InputHistory::new(1);
        h.push("x");
        let bad = temp_path("no_such_dir").join("history.txt");
        let result = h.save(&bad);
        assert!(result.is_err());
    }

    #[test]
    fn stats_track_lifetime_counters() {
        let mut h = InputHistory::new(2);
        h.push("aa"); // принята
        h.push(""); // пропущена: пустая
        h.push("aa"); // пропущена: дубль
        h.push("bb"); // принята
        h.push("ccc"); // принята, «aa» вытеснена
        let st = h.stats();
        assert_eq!(
            st,
            HistoryStats {
                len: 2,
                capacity: 2,
                total_bytes: "bb".len() + "ccc".len(),
                pushed: 3,
                skipped: 2,
                evicted: 1,
            }
        );
    }

    #[test]
    fn default_capacity_constructor_matches_constant() {
        let h = InputHistory::with_default_capacity();
        assert_eq!(h.capacity(), DEFAULT_CAPACITY);
        assert!(h.is_empty());
    }
}
