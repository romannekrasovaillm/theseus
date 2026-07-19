//! Текстовые утилиты агента (образец — текстовый конвейер codex-rs:
//! усечение вывода инструментов, прикидка токенов, очистка ANSI).
//!
//! Харнесс постоянно работает с «сырым» текстом: вывод shell-команд,
//! diff'ы, логи фоновых задач, ответы модели. Этот модуль собирает
//! маленькие чистые функции над строками, которые нужны почти каждому
//! компоненту:
//!
//! * [`estimate_tokens`] — быстрая эвристика «сколько токенов в тексте»
//!   без тяжёлого токенизатора: ASCII идёт по ~4 символа за токен, CJK —
//!   по ~2, прочий юникод (кириллица, эмодзи) — по ~3, а пробельные серии
//!   учитываются со скидкой. Текст режется на серии символов одного
//!   класса, и каждая серия считается по своему «курсу», поэтому
//!   смешанный китайско-английский текст не «уезжает» в среднюю оценку.
//! * [`truncate_middle`] — усечение длинного текста посередине: голова
//!   (60% бюджета) + маркер `... скрыто N символов ...` + хвост (30%);
//!   срезы подвигаются к границам строк и никогда не рвут UTF-8.
//! * [`cap_lines`] — ограничение числа строк с пометкой «... ещё N
//!   строк» (с правильной русской плюрализацией).
//! * [`detect_indent`] — угадывание отступа (табуляция или N пробелов)
//!   по моде строк; нужно при генерации патчей под чужой стиль кода.
//! * [`slice_safe`] — срез по байтовым индексам с подвижкой к границам
//!   UTF-8 внутрь диапазона: никогда не паникует на «половине» символа.
//! * [`word_wrap`] — перенос по словам в заданную ширину; слова длиннее
//!   ширины бьются на куски, пустые строки-абзацы сохраняются.
//! * [`strip_ansi`] — очистка ANSI-последовательностей конечным
//!   автоматом: CSI, «строковые» OSC/DCS/SOS/PM/APC (с терминаторами ST
//!   и BEL), двух- и трёхсимвольные ESC-последовательности, C1-варианты.
//!
//! Все функции — чистые, без паник и без внешних зависимостей (только
//! `std`): их безопасно звать из любого места харнесса, включая горячие
//! пути рендера TUI.

use std::cmp::Reverse;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Оценка числа токенов
// ---------------------------------------------------------------------------

/// Класс символа для эвристики [`estimate_tokens`].
///
/// У разных письменностей разный «курс» символа к токену, поэтому текст
/// сначала режется на серии одинаковых классов, и каждая серия считается
/// отдельно (см. [`run_tokens`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    /// ASCII: латиница, цифры, пунктуация — основа английского текста и
    /// кода (~4 символа на токен в BPE-токенизаторах).
    Ascii,
    /// Пробельные символы (включая переводы строк). Короткие пробелы
    /// обычно сливаются с соседними словами в один токен, поэтому серия
    /// пробелов учитывается со скидкой: один токен на четыре символа с
    /// округлением вниз.
    Whitespace,
    /// CJK-иероглифы, кана и хангыль: плотная письменность, ~2 символа
    /// на токен.
    Cjk,
    /// Всё прочее: кириллица, акцентированная латиница, эмодзи и т.п.
    /// (~3 символа на токен).
    Other,
}

/// Определить класс символа. Порядок проверок важен: ASCII-пробелы —
/// это [`CharClass::Whitespace`], а не [`CharClass::Ascii`].
fn classify(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Whitespace
    } else if c.is_ascii() {
        CharClass::Ascii
    } else if is_cjk(c) {
        CharClass::Cjk
    } else {
        CharClass::Other
    }
}

/// Грубая проверка принадлежности к CJK-письменностям: основные
/// иероглифические блоки, кана, хангыль и полноширинные формы.
fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{3000}'..='\u{303F}'   // CJK-знаки пунктуации
        | '\u{3040}'..='\u{30FF}' // хирагана и катакана
        | '\u{3400}'..='\u{4DBF}' // CJK Extension A
        | '\u{4E00}'..='\u{9FFF}' // основной блок иероглифов
        | '\u{AC00}'..='\u{D7AF}' // слоги хангыля
        | '\u{F900}'..='\u{FAFF}' // иероглифы совместимости
        | '\u{FF00}'..='\u{FFEF}' // полноширинные формы
        | '\u{20000}'..='\u{2A6DF}' // CJK Extension B
    )
}

/// «Курс обмена» серии однородных символов на токены.
fn run_tokens(class: CharClass, len: usize) -> usize {
    match class {
        CharClass::Ascii => len.div_ceil(4),
        CharClass::Cjk => len.div_ceil(2),
        CharClass::Other => len.div_ceil(3),
        // Пробелы считаем с округлением вниз: одиночный пробел между
        // словами отдельным токеном почти никогда не бывает, а вот
        // длинные отступы и серии переводов строк — уже заметная доля.
        CharClass::Whitespace => len / 4,
    }
}

/// Оценить число токенов в `text` без полноценного токенизатора.
///
/// Эвристика по сериям: текст разбивается на блоки символов одного
/// класса ([`CharClass`]), и каждый блок считается по своему курсу —
/// ASCII ~4 символа за токен, CJK ~2, прочий юникод ~3, пробелы ~4 с
/// округлением вниз. Пустая строка стоит 0 токенов, любой непустой
/// текст — минимум 1 (даже пара пробелов).
///
/// Точность — порядка ±20% от реального BPE; для решений «влезет ли это
/// в контекст» и прогресс-баров её достаточно, для биллинга — нет.
///
/// ```
/// use theseus::textutil::estimate_tokens;
///
/// assert_eq!(estimate_tokens(""), 0);
/// assert_eq!(estimate_tokens("abcdefgh"), 2); // 8 ASCII / 4
/// assert_eq!(estimate_tokens("你好世界"), 2); // 4 CJK / 2
/// ```
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut tokens = 0usize;
    // Стартовый класс не важен: длина серии 0, и первая же «промывка»
    // добавит 0 токенов.
    let mut class = CharClass::Whitespace;
    let mut run_len = 0usize;
    for c in text.chars() {
        let cc = classify(c);
        if cc == class {
            run_len += 1;
        } else {
            tokens += run_tokens(class, run_len);
            class = cc;
            run_len = 1;
        }
    }
    tokens += run_tokens(class, run_len);
    // Любой непустой текст стоит хотя бы один токен.
    tokens.max(1)
}

// ---------------------------------------------------------------------------
// Усечение посередине
// ---------------------------------------------------------------------------

/// Минимальный бюджет [`truncate_middle`], при котором есть смысл
/// вставлять маркер «скрыто N символов» (он сам занимает ~25–30
/// символов). При меньшем бюджете возвращается просто голова.
pub const MIN_TRUNCATE_BUDGET: usize = 40;

/// Байтовый индекс `char_idx`-го символа (или длина строки, если символов
/// меньше). O(n) — приемлемо для утилитных срезов.
fn byte_of_char(text: &str, char_idx: usize) -> usize {
    match text.char_indices().nth(char_idx) {
        Some((byte, _)) => byte,
        None => text.len(),
    }
}

/// Срез головы: не дальше `budget_chars` символов от начала, с подвижкой
/// назад к ближайшему концу строки (сам `\n` включается в голову).
/// Возвращает байтовый индекс среза. Если в бюджете нет перевода строки
/// (или он стоит в самом начале), режем ровно по бюджету — всё равно по
/// границе символа, так что UTF-8 не страдает ни в одном случае.
fn cut_head(text: &str, budget_chars: usize) -> usize {
    let byte = byte_of_char(text, budget_chars);
    match text[..byte].rfind('\n') {
        Some(pos) if pos > 0 => pos + 1,
        _ => byte,
    }
}

/// Срез хвоста: не раньше чем за `budget_chars` символов от конца, с
/// подвижкой вперёд к ближайшему началу строки (хвост начинается сразу
/// после `\n`). Если в бюджете нет перевода строки (или найденный `\n` —
/// последний байт текста), режем ровно по бюджету.
fn cut_tail(text: &str, budget_chars: usize) -> usize {
    let total = text.chars().count();
    let start_char = total.saturating_sub(budget_chars);
    let byte = byte_of_char(text, start_char);
    match text[byte..].find('\n') {
        Some(rel) if byte + rel + 1 < text.len() => byte + rel + 1,
        _ => byte,
    }
}

/// Усечь текст посередине до бюджета примерно `max_chars` символов.
///
/// Структура результата: голова (60% бюджета) + маркер `"\n[... скрыто
/// N символов ...]\n"` + хвост (30% бюджета); оставшиеся ~10% условно
/// зарезервированы под сам маркер. Оба среза подвигаются к границам
/// строк ([`cut_head`]/[`cut_tail`]), поэтому вывод не рвёт строки
/// посередине; резка по символам, а не по байтам, гарантирует, что
/// многобайтовый UTF-8 (кириллица, CJK, 4-байтные эмодзи) не ломается.
///
/// Голова и хвост не пересекаются: их суммарный бюджет 0.9·`max_chars`
/// строго меньше длины текста (иначе усечение не выполняется вовсе).
///
/// Особые случаи:
///
/// * текст не длиннее `max_chars` — возвращается без изменений;
/// * `max_chars < MIN_TRUNCATE_BUDGET` — маркер съест весь бюджет,
///   поэтому возвращается просто голова из `max_chars` символов;
/// * из-за маркера и подвижки к границам строк результат может
///   немного отличаться от `max_chars` — это бюджет, а не жёсткий лимит.
pub fn truncate_middle(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    if max_chars < MIN_TRUNCATE_BUDGET {
        return text.chars().take(max_chars).collect();
    }
    let head_budget = max_chars * 6 / 10;
    let tail_budget = max_chars * 3 / 10;
    let head = &text[..cut_head(text, head_budget)];
    let tail = &text[cut_tail(text, tail_budget)..];
    let hidden = total - head.chars().count() - tail.chars().count();
    format!(
        "{head}\n[... скрыто {hidden} {} ...]\n{tail}",
        plural_ru(hidden, "символ", "символа", "символов")
    )
}

// ---------------------------------------------------------------------------
// Ограничение числа строк
// ---------------------------------------------------------------------------

/// Русская плюрализация: 1 → `one`, 2–4 → `few`, прочее → `many`, с
/// исключением для 11–14 («11 строк», а не «11 строка»).
fn plural_ru(n: usize, one: &'static str, few: &'static str, many: &'static str) -> &'static str {
    let mod100 = n % 100;
    if (11..=14).contains(&mod100) {
        return many;
    }
    match mod100 % 10 {
        1 => one,
        2..=4 => few,
        _ => many,
    }
}

/// Оставить первые `max_lines` строк текста, добавив пометку о скрытых.
///
/// Если строк не больше лимита, текст возвращается без изменений. Иначе
/// результат — первые `max_lines` строк плюс финальная строка-заметка
/// `"... ещё N строк"` (с правильной плюрализацией; заметка идёт сверх
/// лимита, т.е. итоговый вывод может содержать `max_lines + 1` строк).
///
/// Строки считаются через [`str::lines`], поэтому финальный перевод
/// строки отдельной строкой не считается. Пустой текст и лимит 0
/// обрабатываются естественно: `cap_lines("", _)` → `""`, а
/// `cap_lines("a\nb", 0)` → `"... ещё 2 строки"`.
pub fn cap_lines(text: &str, max_lines: usize) -> String {
    let total = text.lines().count();
    if total <= max_lines {
        return text.to_string();
    }
    let hidden = total - max_lines;
    let mut out = text.lines().take(max_lines).collect::<Vec<_>>().join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&format!(
        "... ещё {hidden} {}",
        plural_ru(hidden, "строка", "строки", "строк")
    ));
    out
}

// ---------------------------------------------------------------------------
// Определение отступа
// ---------------------------------------------------------------------------

/// Отступ по умолчанию, когда по строкам определить ничего нельзя
/// (пустой вход или ни одной строки с отступом): четыре пробела.
pub const DEFAULT_INDENT: &str = "    ";

/// Угадать отступ, принятый в тексте: табуляция (`"\t"`) или серия
/// пробелов (`" ".repeat(n)`).
///
/// Каждая непустая строка с отступом «голосует»: строка, начинающаяся с
/// табуляции, — за табуляцию; строка, начинающаяся с пробелов, — за
/// пробелы с шириной, равной числу ведущих пробелов. Побеждает мода
/// (самый частый вариант); среди пробельных ширин при равенстве голосов
/// выбирается меньшая, а при общей ничьей «табуляция против пробелов»
/// побеждают пробелы — более частый случай в коде, с которым работает
/// агент. Строки без отступа и пустые строки не голосуют.
///
/// Если доказательств нет вовсе, возвращается [`DEFAULT_INDENT`].
pub fn detect_indent(lines: &[&str]) -> String {
    let mut tab_votes = 0usize;
    // Ширина пробельного отступа -> число проголосовавших строк.
    let mut space_votes: BTreeMap<usize, usize> = BTreeMap::new();

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            tab_votes += 1;
        } else if line.starts_with(' ') {
            let width = line.chars().take_while(|c| *c == ' ').count();
            *space_votes.entry(width).or_insert(0) += 1;
        }
        // Строки без отступа не голосуют.
    }

    // Лучшая пробельная ширина: максимум голосов, при ничьей — меньшая.
    let best_spaces = space_votes
        .iter()
        .max_by_key(|(width, votes)| (*votes, Reverse(*width)))
        .map(|(width, votes)| (*width, *votes));

    match best_spaces {
        None if tab_votes == 0 => DEFAULT_INDENT.to_string(),
        None => "\t".to_string(),
        Some((_, votes)) if tab_votes > votes => "\t".to_string(),
        Some((width, _)) => " ".repeat(width),
    }
}

// ---------------------------------------------------------------------------
// Безопасный срез по байтам
// ---------------------------------------------------------------------------

/// Срез `text[start..end]`, устойчивый к неграничным байтовым индексам.
///
/// Оба индекса зажимаются в длину строки и подвигаются к ближайшим
/// границам UTF-8 **внутрь** диапазона: `start` — вперёд, `end` — назад.
/// Такой срез никогда не выходит за запрошенный диапазон и никогда не
/// паникует на «половине» многобайтового символа; ценой является то, что
/// пограничный символ может быть отброшен целиком. Если после подвижки
/// диапазон схлопнулся (или `start > end`), возвращается пустая строка.
///
/// ```
/// use theseus::textutil::slice_safe;
///
/// let s = "a🦀b"; // байты: a = 0..1, 🦀 = 1..5, b = 5..6
/// assert_eq!(slice_safe(s, 2, 6), "b"); // start дополз до границы 5
/// assert_eq!(slice_safe(s, 0, 3), "a"); // end отполз до границы 1
/// ```
pub fn slice_safe(text: &str, start: usize, end: usize) -> &str {
    let len = text.len();
    let mut s = start.min(len);
    let mut e = end.min(len);
    if s > e {
        return "";
    }
    // Сужаем диапазон внутрь до ближайших границ символов.
    while s < e && !text.is_char_boundary(s) {
        s += 1;
    }
    while e > s && !text.is_char_boundary(e) {
        e -= 1;
    }
    &text[s..e]
}

// ---------------------------------------------------------------------------
// Перенос по словам
// ---------------------------------------------------------------------------

/// Перенести текст по словам в строки шириной не больше `width` символов.
///
/// Жадный алгоритм: слова (разделённые любым пробельным символом)
/// накапливаются в текущей строке, пока помещаются; серии пробелов
/// схлопываются в один разделитель. Слово длиннее `width` режется на
/// куски по `width` символов (по границам UTF-8), причём остаток
/// последнего куска продолжает строку, и следующие слова могут
/// присоединиться к нему. Переводы строк исходного текста сохраняются:
/// каждый входной абзац переносится отдельно, а пустые строки
/// переходят в пустые строки результата.
///
/// Ширина считается в символах (Unicode scalar values), а не в
/// терминальных колонках: широкие CJK-иероглифы и комбинирующие знаки
/// считаются за один символ — для точной раскладки в TUI поверх этой
/// функции нужен display-width.
///
/// Особый случай: `width == 0` отключает перенос — возвращаются строки
/// исходного текста как есть.
pub fn word_wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return text.lines().map(str::to_string).collect();
    }
    let mut out = Vec::new();
    for line in text.lines() {
        wrap_line(line, width, &mut out);
    }
    out
}

/// Перенос одной строки (без переводов строк) в `out`.
fn wrap_line(line: &str, width: usize, out: &mut Vec<String>) {
    let mut cur = String::new();
    let mut cur_len = 0usize;
    let mut words_seen = 0usize;

    for word in line.split_whitespace() {
        words_seen += 1;
        let wlen = word.chars().count();
        if wlen > width {
            // Длинное слово: добиваем текущую строку и режем слово.
            // cur_len ниже в этой ветке всегда перезаписывается из rest,
            // поэтому обнулять его после take не нужно.
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let mut rest = word;
            while rest.chars().count() > width {
                let cut = byte_of_char(rest, width);
                out.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            // Остаток (короче ширины) — начало следующей строки; к нему
            // могут присоединиться следующие слова.
            cur = rest.to_string();
            cur_len = rest.chars().count();
        } else if cur.is_empty() {
            cur.push_str(word);
            cur_len = wlen;
        } else if cur_len + 1 + wlen <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_len += 1 + wlen;
        } else {
            out.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_len = wlen;
        }
    }

    if words_seen == 0 {
        // Строка без слов (пустая или одни пробелы) — сохраняем пустую.
        out.push(String::new());
    } else if !cur.is_empty() {
        out.push(cur);
    }
}

// ---------------------------------------------------------------------------
// Очистка ANSI-последовательностей
// ---------------------------------------------------------------------------

/// Состояния конечного автомата [`strip_ansi`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnsiState {
    /// Обычный текст: всё копируется в вывод.
    Ground,
    /// Пойман ESC, ждём байт типа последовательности.
    Esc,
    /// Пойман ESC + байт семейства `( ) * + # %` — ждём ещё один байт
    /// аргумента (трёхсимвольные последовательности выбора кодировки
    /// и т.п.).
    EscArg,
    /// Внутри CSI (`ESC [` или C1 CSI): параметры и промежуточные байты,
    /// конец — финальный байт `@`..=`~`.
    Csi,
    /// Внутри «строковой» последовательности (OSC/DCS/SOS/PM/APC):
    /// пропускаем всё до терминатора (BEL, C1 ST или `ESC \`).
    Str,
    /// Внутри строковой последовательности пойман ESC: если дальше `\`,
    /// это ST и конец, иначе строковая последовательность прервана и
    /// началась новая ESC-последовательность.
    StrEsc,
}

/// Переход после ESC (в т.ч. после ESC, прервавшего строковую
/// последовательность): выбор типа последовательности по байту `c`.
/// Двухсимвольные Fe/Fs-последовательности (например `ESC c`, `ESC M`)
/// считаются сразу завершёнными — их байт уже «съеден».
fn after_esc(c: char) -> AnsiState {
    match c {
        '[' => AnsiState::Csi,
        ']' | 'P' | 'X' | '^' | '_' => AnsiState::Str,
        '(' | ')' | '*' | '+' | '#' | '%' => AnsiState::EscArg,
        '\u{1b}' => AnsiState::Esc,
        _ => AnsiState::Ground,
    }
}

/// Удалить из текста ANSI escape-последовательности конечным автоматом.
///
/// Распознаются все формы, встречающиеся в выводе терминальных
/// программ:
///
/// * **CSI** — `ESC [` параметры финальный-байт: цвета SGR
///   (`\x1b[1;31m`), управление курсором (`\x1b[2J`, `\x1b[H`),
///   скроллинг и т.д.;
/// * **«строковые»** — OSC (`ESC ]`, гиперссылки и заголовки окон),
///   DCS (`ESC P`), SOS, PM, APC; закрываются терминатором ST (`ESC \`),
///   C1 ST (U+009C) или BEL (xterm-стиль для OSC);
/// * **двухсимвольные** — `ESC c` (RIS), `ESC M` (reverse index),
///   `ESC 7`/`ESC 8` (сохранить/восстановить курсор) и т.п.;
/// * **трёхсимвольные** — `ESC ( B` и прочие последовательности выбора
///   кодировки/режима (`( ) * + # %`);
/// * **C1-варианты** — одиночные байты U+009B (CSI), U+0090 (DCS),
///   U+009D (OSC) и др.
///
/// Весь остальной текст — включая переводы строк, табуляции и юникод —
/// сохраняется без изменений. Недописанная последовательность в конце
/// строки молча отбрасывается (её символы уже «проглочены» автоматом).
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut state = AnsiState::Ground;
    for c in text.chars() {
        state = match state {
            AnsiState::Ground => match c {
                '\u{1b}' => AnsiState::Esc,
                '\u{9b}' => AnsiState::Csi,
                // C1-варианты строковых последовательностей:
                // DCS, SOS, OSC, PM, APC.
                '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => AnsiState::Str,
                _ => {
                    out.push(c);
                    AnsiState::Ground
                }
            },
            AnsiState::Esc => after_esc(c),
            AnsiState::EscArg => AnsiState::Ground,
            AnsiState::Csi => match c {
                // Финальный байт CSI: 0x40..=0x7E.
                '@'..='~' => AnsiState::Ground,
                _ => AnsiState::Csi,
            },
            AnsiState::Str => match c {
                // BEL (xterm-стиль) или C1 ST закрывают строковую.
                '\u{07}' | '\u{9c}' => AnsiState::Ground,
                '\u{1b}' => AnsiState::StrEsc,
                _ => AnsiState::Str,
            },
            AnsiState::StrEsc => match c {
                // ESC \ — последовательный терминатор ST.
                '\\' => AnsiState::Ground,
                _ => after_esc(c),
            },
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- estimate_tokens ------------------------------------------------

    #[test]
    fn estimate_empty_is_zero() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_ascii_runs() {
        assert_eq!(estimate_tokens("abcdefgh"), 2); // 8 / 4
        assert_eq!(estimate_tokens("ab"), 1); // округление вверх
        // «hello» (2) + одиночный пробел (0) + «world» (2).
        assert_eq!(estimate_tokens("hello world"), 4);
    }

    #[test]
    fn estimate_cjk_runs() {
        assert_eq!(estimate_tokens("你好世界"), 2); // 4 / 2
        assert_eq!(estimate_tokens("こんにちは"), 3); // 5 каны -> ceil(5/2)
    }

    #[test]
    fn estimate_mixed_blocks_counted_separately() {
        // Блоки разных классов считаются по своему курсу, а не по среднему.
        assert_eq!(estimate_tokens("ab你好cd"), 3); // 1 + 1 + 1
        assert_eq!(estimate_tokens("fn привет"), 3); // 1 + 0 + 2
    }

    #[test]
    fn estimate_spaces_discounted() {
        assert_eq!(estimate_tokens("x        y"), 4); // 1 + 8/4 + 1
        assert_eq!(estimate_tokens("a b"), 2); // одиночный пробел -> 0
        // Непустой текст стоит минимум один токен.
        assert_eq!(estimate_tokens("  "), 1);
    }

    #[test]
    fn estimate_emoji_and_cyrillic() {
        assert_eq!(estimate_tokens("🦀🦀🦀"), 1); // 3 / 3
        assert_eq!(estimate_tokens("🦀🦀🦀🦀🦀🦀🦀"), 3); // 7 -> ceil(7/3)
        assert_eq!(estimate_tokens("привет"), 2); // 6 / 3
    }

    // ---- truncate_middle -------------------------------------------------

    #[test]
    fn truncate_short_text_unchanged() {
        assert_eq!(truncate_middle("hello", 10), "hello");
        // Длина ровно в бюджет — тоже без изменений.
        assert_eq!(truncate_middle("hello", 5), "hello");
    }

    #[test]
    fn truncate_multiline_cuts_on_line_boundaries() {
        // 20 строк по 5 символов («l000\n»…«l019\n») = 100 символов.
        let text: String = (0..20).map(|i| format!("l{i:03}\n")).collect();
        // Бюджет 40: голова 24 -> по границе строки 20, хвост 12 -> по
        // границе строки 10; скрыто 100 - 20 - 10 = 70.
        let expected = "l000\nl001\nl002\nl003\n\n[... скрыто 70 символов ...]\nl018\nl019\n";
        assert_eq!(truncate_middle(&text, 40), expected);
    }

    #[test]
    fn truncate_single_huge_line() {
        // Ни одного перевода строки — срезы ровно по бюджету (60/30).
        let text = "a".repeat(1000);
        let expected = format!(
            "{}\n[... скрыто 910 символов ...]\n{}",
            "a".repeat(60),
            "a".repeat(30)
        );
        assert_eq!(truncate_middle(&text, 100), expected);
    }

    #[test]
    fn truncate_emoji_never_split() {
        // 4-байтные эмодзи: срез обязан проходить по границе символа.
        let text = "🦀".repeat(50);
        let out = truncate_middle(&text, 40);
        let expected = format!(
            "{}\n[... скрыто 14 символов ...]\n{}",
            "🦀".repeat(24),
            "🦀".repeat(12)
        );
        assert_eq!(out, expected);
        // Все 36 оставшихся крабов — целиком.
        assert_eq!(out.matches('🦀').count(), 36);
    }

    #[test]
    fn truncate_cjk_huge_line() {
        let text = "你".repeat(100);
        let expected = format!(
            "{}\n[... скрыто 64 символа ...]\n{}",
            "你".repeat(24),
            "你".repeat(12)
        );
        assert_eq!(truncate_middle(&text, 40), expected);
    }

    #[test]
    fn truncate_tiny_budget_returns_head() {
        let text = "x".repeat(100);
        assert_eq!(truncate_middle(&text, 10), "x".repeat(10));
        assert_eq!(truncate_middle("abc", 0), "");
    }

    // ---- cap_lines --------------------------------------------------------

    #[test]
    fn cap_lines_within_limit_unchanged() {
        assert_eq!(cap_lines("a\nb\nc", 5), "a\nb\nc");
        assert_eq!(cap_lines("a\nb\nc", 3), "a\nb\nc");
    }

    #[test]
    fn cap_lines_overflow_adds_note() {
        assert_eq!(cap_lines("a\nb\nc", 2), "a\nb\n... ещё 1 строка");
        // Финальный перевод строки отдельной строкой не считается.
        assert_eq!(cap_lines("a\nb\nc\n", 2), "a\nb\n... ещё 1 строка");
    }

    #[test]
    fn cap_lines_plural_forms() {
        let case = |max: usize, total: usize| -> String {
            let text: String = (0..total).map(|i| format!("s{i}\n")).collect();
            cap_lines(&text, max)
        };
        assert!(case(3, 4).ends_with("... ещё 1 строка"));
        assert!(case(3, 5).ends_with("... ещё 2 строки"));
        assert!(case(3, 8).ends_with("... ещё 5 строк"));
        // 11–14 — исключение русской плюрализации.
        assert!(case(3, 14).ends_with("... ещё 11 строк"));
        assert!(case(3, 24).ends_with("... ещё 21 строка"));
    }

    #[test]
    fn cap_lines_zero_and_empty() {
        assert_eq!(cap_lines("a\nb", 0), "... ещё 2 строки");
        assert_eq!(cap_lines("", 0), "");
    }

    // ---- detect_indent ----------------------------------------------------

    #[test]
    fn detect_indent_tabs_win() {
        let lines = ["\tfn main() {", "\t\tbody();", "\t}"];
        assert_eq!(detect_indent(&lines), "\t");
    }

    #[test]
    fn detect_indent_four_spaces_mode() {
        // Мода — 4 пробела (две строки), хотя есть строка с 8.
        let lines = ["    a", "        b", "    c"];
        assert_eq!(detect_indent(&lines), "    ");
    }

    #[test]
    fn detect_indent_two_spaces_majority() {
        let lines = ["  a", "    b", "  c", "  d"];
        assert_eq!(detect_indent(&lines), "  ");
    }

    #[test]
    fn detect_indent_default_when_no_evidence() {
        assert_eq!(detect_indent(&[]), DEFAULT_INDENT);
        assert_eq!(detect_indent(&["fn main() {}", "}"]), DEFAULT_INDENT);
        // Пустые и пробельные строки не голосуют.
        assert_eq!(detect_indent(&["", "   ", "\tx"]), "\t");
    }

    #[test]
    fn detect_indent_tie_prefers_spaces() {
        assert_eq!(detect_indent(&["\ta", "    b"]), "    ");
    }

    // ---- slice_safe -------------------------------------------------------

    #[test]
    fn slice_safe_exact_boundaries() {
        let s = "a🦀b"; // байты: a = 0..1, 🦀 = 1..5, b = 5..6
        assert_eq!(slice_safe(s, 0, 6), "a🦀b");
        assert_eq!(slice_safe(s, 1, 5), "🦀");
    }

    #[test]
    fn slice_safe_shrinks_to_char_boundaries() {
        let s = "a🦀b";
        assert_eq!(slice_safe(s, 2, 6), "b"); // start дополз до 5
        assert_eq!(slice_safe(s, 0, 3), "a"); // end отполз до 1
        assert_eq!(slice_safe(s, 2, 5), ""); // схлопнулся внутри эмодзи
        assert_eq!(slice_safe("你a", 1, 4), "a");
    }

    #[test]
    fn slice_safe_clamps_and_empty() {
        let s = "a🦀b";
        assert_eq!(slice_safe(s, 0, 100), s); // за концом — обрезка
        assert_eq!(slice_safe(s, 4, 1), ""); // start > end
        assert_eq!(slice_safe(s, 6, 6), "");
    }

    // ---- word_wrap --------------------------------------------------------

    #[test]
    fn wrap_greedy_by_words() {
        assert_eq!(word_wrap("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        // Слова, встающие впритык, не переносятся.
        assert_eq!(word_wrap("aa bb", 5), vec!["aa bb"]);
    }

    #[test]
    fn wrap_collapses_whitespace_runs() {
        // Серии пробелов и табуляции схлопываются в один разделитель.
        assert_eq!(word_wrap("a   b\tc", 10), vec!["a b c"]);
    }

    #[test]
    fn wrap_breaks_long_word() {
        // 20 символов при ширине 6: 6 + 6 + 6 + 2.
        assert_eq!(
            word_wrap("supercalifragilistic", 6),
            vec!["superc", "alifra", "gilist", "ic"]
        );
    }

    #[test]
    fn wrap_long_word_between_words() {
        assert_eq!(
            word_wrap("hey supercalifragilistic yo", 10),
            vec!["hey", "supercalif", "ragilistic", "yo"]
        );
    }

    #[test]
    fn wrap_long_word_remainder_merges() {
        // Остаток битого слова продолжает строку — следующее слово липнет.
        assert_eq!(word_wrap("aaaaaaaaa bb", 6), vec!["aaaaaa", "aaa bb"]);
        // Длина кратна ширине — пустой строки в хвосте не остаётся.
        assert_eq!(word_wrap("aaaaaaaaaaaa bb", 6), vec!["aaaaaa", "aaaaaa", "bb"]);
    }

    #[test]
    fn wrap_preserves_blank_lines_and_width_zero() {
        assert_eq!(word_wrap("aa bb\n\ncc", 5), vec!["aa bb", "", "cc"]);
        // Ширина 0 отключает перенос: строки возвращаются как есть.
        assert_eq!(word_wrap("aa  bb\ncc", 0), vec!["aa  bb", "cc"]);
    }

    #[test]
    fn wrap_counts_cjk_as_chars() {
        assert_eq!(word_wrap("你好 世界", 5), vec!["你好 世界"]);
        assert_eq!(word_wrap("你好 世界", 4), vec!["你好", "世界"]);
    }

    // ---- strip_ansi -------------------------------------------------------

    #[test]
    fn strip_sgr_colors() {
        assert_eq!(strip_ansi("\x1b[1;31mпривет\x1b[0m!"), "привет!");
        assert_eq!(
            strip_ansi("\x1b[32m✓\x1b[0m ok\n\x1b[31m✗\x1b[0m fail"),
            "✓ ok\n✗ fail"
        );
    }

    #[test]
    fn strip_csi_cursor_and_erase() {
        assert_eq!(strip_ansi("\x1b[2J\x1b[Habc\x1b[1;5r"), "abc");
        // C1-вариант CSI (U+009B вместо ESC [).
        assert_eq!(strip_ansi("\u{9b}31mred\u{9b}0m"), "red");
    }

    #[test]
    fn strip_osc_both_terminators() {
        // OSC-гиперссылка, закрытая BEL (xterm-стиль).
        assert_eq!(
            strip_ansi("\x1b]8;;https://example.com\x07link\x1b]8;;\x07!"),
            "link!"
        );
        // OSC-заголовок окна, закрытый ST (ESC \).
        assert_eq!(strip_ansi("\x1b]0;title\x1b\\x"), "x");
    }

    #[test]
    fn strip_short_esc_sequences() {
        // Двухсимвольные: ESC c (RIS), ESC M (RI), ESC 7/8 (курсор).
        assert_eq!(strip_ansi("\x1bc\x1bM\x1b7abc\x1b8"), "abc");
        // Трёхсимвольные: ESC ( B (кодировка), ESC # 8 (режим экрана).
        assert_eq!(strip_ansi("\x1b(B\x1b#8abc"), "abc");
    }

    #[test]
    fn strip_dcs_and_apc() {
        // DCS ... ST.
        assert_eq!(strip_ansi("\x1bP1$r q\x1b\\ok"), "ok");
        // APC ... BEL.
        assert_eq!(strip_ansi("\x1b_Ga=aaa\x07ok"), "ok");
    }

    #[test]
    fn strip_unterminated_tail_dropped() {
        assert_eq!(strip_ansi("abc\x1b[31"), "abc");
        assert_eq!(strip_ansi("abc\x1b]0;t"), "abc");
        assert_eq!(strip_ansi("abc\x1b"), "abc");
    }

    #[test]
    fn strip_plain_text_untouched() {
        let plain = "обычный текст\nс табуляцией\tи 100% значением";
        assert_eq!(strip_ansi(plain), plain);
        assert_eq!(strip_ansi(""), "");
    }
}
