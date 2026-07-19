//! Понимание концептов из библиотеки `/home/roman/library`.
//!
//! Библиотека хранит карточки концепций файлами `concepts/<тип>/<slug>.md`:
//! YAML frontmatter между маркерами `---` (поля `slug`, `type`, `level`,
//! `formality`, `title`, `aliases`, `related`, `family`, `sources`) и
//! markdown-тело с секциями (`## Определение`, `## Мотивация` и др.).
//!
//! [`parse_card`] разбирает одну карточку, а [`ConceptIndex`] строит по корню
//! библиотеки индекс с регистронезависим поиском, компактным рендером карточки
//! для контекста модели ([`ConceptIndex::explain`]) и обходом графа
//! related-связей ([`ConceptIndex::related`]).

use std::collections::{hash_map::Entry, BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

/// Вес точного совпадения slug в поисковой выдаче.
const SCORE_SLUG: u32 = 100;
/// Вес точного совпадения по одному из alias.
const SCORE_ALIAS: u32 = 80;
/// Вес вхождения запроса в заголовок карточки.
const SCORE_TITLE: u32 = 60;
/// Вес вхождения запроса в тело карточки.
const SCORE_BODY: u32 = 10;

/// Сколько related-ссылок максимум попадает в компактный рендер
/// [`ConceptIndex::explain`].
const EXPLAIN_MAX_RELATED: usize = 5;

/// Карточка концепции из библиотеки (frontmatter + тело).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConceptCard {
    /// Уникальный идентификатор концепции (поле `slug`).
    pub slug: String,
    /// Тип концепции (поле `type`: `algorithmic`, `theorem`, `benchmark`, ...).
    pub card_type: String,
    /// Уровень обоснованности (например, `α` — формальный, `β` — эмпирический).
    pub level: String,
    /// Степень формальности описания (`A`/`B`/`C`).
    pub formality: String,
    /// Человекочитаемый заголовок концепции.
    pub title: String,
    /// Альтернативные названия (поле `aliases`).
    pub aliases: Vec<String>,
    /// Slug-ссылки на связанные концепции (поле `related`).
    pub related: Vec<String>,
    /// Тематическое семейство концепции (поле `family`).
    pub family: String,
    /// Источники, например `arxiv.2509.01938` (поле `sources`).
    pub sources: Vec<String>,
    /// Markdown-тело карточки — всё после закрывающего маркера `---`.
    pub body: String,
}

/// Ключ списка, который сейчас наполняется при построчном разборе frontmatter.
#[derive(Clone, Copy)]
enum ListKey {
    Aliases,
    Related,
    Sources,
}

/// Разбирает текст карточки: frontmatter между маркерами `---` + markdown-тело.
///
/// Списки (`aliases`, `related`, `sources`) поддерживаются в двух формах:
/// инлайн `[a, b]` и построчно — строками `- элемент` после ключа с пустым
/// значением. Обязательное поле — `slug`; остальные при отсутствии остаются
/// пустыми. Неизвестные ключи (и их построчные списки) игнорируются,
/// с значений снимаются обрамляющие кавычки.
///
/// # Ошибки
/// Ошибка возвращается, если текст пуст, не начинается с `---`, не содержит
/// закрывающего `---` либо поле `slug` отсутствует или пусто.
pub fn parse_card(text: &str) -> Result<ConceptCard> {
    let text = text.trim_start_matches('\u{feff}');
    let lines: Vec<&str> = text.lines().collect();
    let Some(first) = lines.first() else {
        bail!("пустой текст: карточка должна начинаться с маркера `---`");
    };
    if first.trim() != "---" {
        bail!("карточка не начинается с маркера frontmatter `---`");
    }
    let close = lines[1..]
        .iter()
        .position(|line| line.trim() == "---")
        .map(|pos| pos + 1);
    let Some(close) = close else {
        bail!("frontmatter не закрыт: не найден второй маркер `---`");
    };

    let mut slug = String::new();
    let mut card_type = String::new();
    let mut level = String::new();
    let mut formality = String::new();
    let mut title = String::new();
    let mut family = String::new();
    let mut aliases = Vec::new();
    let mut related = Vec::new();
    let mut sources = Vec::new();
    let mut current_list: Option<ListKey> = None;

    for raw in &lines[1..close] {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue; // пустые строки и комментарии
        }
        if let Some(item) = line.strip_prefix("- ") {
            // Элемент построчного списка: относится к последнему ключу
            // с пустым значением (если это известный список).
            let item = unquote(item);
            if !item.is_empty() {
                match current_list {
                    Some(ListKey::Aliases) => aliases.push(item),
                    Some(ListKey::Related) => related.push(item),
                    Some(ListKey::Sources) => sources.push(item),
                    None => {}
                }
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue; // строка без двоеточия — не ключ, пропускаем
        };
        current_list = None;
        let value = value.trim();
        match key.trim() {
            "slug" => slug = unquote(value),
            "type" => card_type = unquote(value),
            "level" => level = unquote(value),
            "formality" => formality = unquote(value),
            "title" => title = unquote(value),
            "family" => family = unquote(value),
            "aliases" => {
                if value.is_empty() {
                    current_list = Some(ListKey::Aliases);
                } else {
                    aliases = parse_inline_list(value);
                }
            }
            "related" => {
                if value.is_empty() {
                    current_list = Some(ListKey::Related);
                } else {
                    related = parse_inline_list(value);
                }
            }
            "sources" => {
                if value.is_empty() {
                    current_list = Some(ListKey::Sources);
                } else {
                    sources = parse_inline_list(value);
                }
            }
            _ => {} // неизвестные ключи игнорируем — прямая совместимость
        }
    }
    if slug.is_empty() {
        bail!("в карточке отсутствует обязательное поле `slug`");
    }
    let body = lines[close + 1..].join("\n").trim().to_string();
    Ok(ConceptCard {
        slug,
        card_type,
        level,
        formality,
        title,
        aliases,
        related,
        family,
        sources,
        body,
    })
}

/// Снимает с значения обрамляющие одинарные или двойные кавычки, если они есть.
fn unquote(value: &str) -> String {
    let value = value.trim();
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

/// Разбирает инлайн-список вида `[a, b]`; одиночное значение без скобок
/// допускается и даёт список из одного элемента.
fn parse_inline_list(value: &str) -> Vec<String> {
    let inner = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(value);
    inner
        .split(',')
        .map(unquote)
        .filter(|item| !item.is_empty())
        .collect()
}

/// Вырезает из тела карточки содержимое секции `## <name>` (всё до следующего
/// заголовка `## `). Отсутствующая или пустая секция даёт `None`.
fn extract_section(body: &str, name: &str) -> Option<String> {
    let header = format!("## {name}");
    let mut inside = false;
    let mut collected: Vec<&str> = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim_end();
        if trimmed.starts_with("## ") {
            if inside {
                break; // следующая секция — конец текущей
            }
            if trimmed == header {
                inside = true;
                continue;
            }
        }
        if inside {
            collected.push(line);
        }
    }
    let text = collected.join("\n");
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Рекурсивно собирает в `out` пути всех `*.md` под каталогом `dir`.
/// Симлинки не разворачиваются (защита от циклов), нечитаемые каталоги
/// пропускаются.
fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_markdown(&path, out);
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "md") {
            out.push(path);
        }
    }
}

/// Вес релевантности карточки запросу (запрос уже в нижнем регистре).
/// Берётся максимум из категорий: slug — 100, alias — 80, title — 60, body — 10.
fn score_card(card: &ConceptCard, needle: &str) -> u32 {
    if card.slug.eq_ignore_ascii_case(needle) {
        return SCORE_SLUG;
    }
    if card.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(needle)) {
        return SCORE_ALIAS;
    }
    if card.title.to_lowercase().contains(needle) {
        return SCORE_TITLE;
    }
    if card.body.to_lowercase().contains(needle) {
        return SCORE_BODY;
    }
    0
}

/// Индекс библиотеки концепций: все карточки + быстрый доступ по slug
/// и счётчики по типам. Строится один раз по каталогу, дальше только читается.
#[derive(Debug, Default)]
pub struct ConceptIndex {
    /// Все успешно разобранные карточки (в отсортированном порядке путей).
    cards: Vec<ConceptCard>,
    /// Отображение slug → позиция в `cards` (дубликаты slug не попадают сюда).
    by_slug: HashMap<String, usize>,
    /// Счётчики карточек по типам (поле `type`).
    by_type: BTreeMap<String, usize>,
}

impl ConceptIndex {
    /// Строит индекс по корню библиотеки: рекурсивно собирает все `*.md`,
    /// разбирает их и считает статистику по типам.
    ///
    /// Файлы, которые не читаются или не разбираются (например, усечённые
    /// карточки без закрывающего `---`), пропускаются. При дубликатах `slug`
    /// побеждает первая карточка в отсортированном порядке путей.
    pub fn build(root: &Path) -> ConceptIndex {
        let mut paths = Vec::new();
        collect_markdown(root, &mut paths);
        paths.sort();
        let mut index = ConceptIndex::default();
        for path in &paths {
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            let Ok(card) = parse_card(&text) else {
                continue;
            };
            if let Entry::Vacant(slot) = index.by_slug.entry(card.slug.clone()) {
                slot.insert(index.cards.len());
                *index.by_type.entry(card.card_type.clone()).or_insert(0) += 1;
                index.cards.push(card);
            }
        }
        index
    }

    /// Возвращает карточку по точному slug (`None`, если такой нет).
    pub fn get(&self, slug: &str) -> Option<&ConceptCard> {
        self.by_slug.get(slug).map(|&pos| &self.cards[pos])
    }

    /// Ищет карточки по запросу (регистронезависимо), не более `limit` штук.
    ///
    /// Ранжирование по максимальному весу категории совпадения: точный slug —
    /// 100, совпадение alias — 80, вхождение в title — 60, вхождение в body —
    /// 10. Внутри одного веса карточки упорядочены по slug. Пустой запрос или
    /// `limit == 0` дают пустой результат.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&ConceptCard> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut scored: Vec<(u32, &ConceptCard)> = self
            .cards
            .iter()
            .filter_map(|card| {
                let score = score_card(card, &needle);
                (score > 0).then_some((score, card))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.slug.cmp(&b.1.slug)));
        scored.truncate(limit);
        scored.into_iter().map(|(_, card)| card).collect()
    }

    /// Компактный рендер карточки для контекста модели: заголовок, уровень,
    /// формальность, секции «Определение» и «Мотивация», до 5 related-ссылок.
    ///
    /// `None`, если карточки с таким slug нет в индексе.
    pub fn explain(&self, slug: &str) -> Option<String> {
        let card = self.get(slug)?;
        let mut out = String::new();
        out.push_str(&format!("# {} ({})\n", card.title, card.slug));
        let mut meta = vec![
            format!("Тип: {}", card.card_type),
            format!("Уровень: {}", card.level),
            format!("Формальность: {}", card.formality),
        ];
        if !card.family.is_empty() {
            meta.push(format!("Семейство: {}", card.family));
        }
        out.push_str(&meta.join(" | "));
        out.push('\n');
        for section in ["Определение", "Мотивация"] {
            if let Some(text) = extract_section(&card.body, section) {
                out.push_str(&format!("\n## {section}\n{text}\n"));
            }
        }
        if !card.related.is_empty() {
            let shown: Vec<&str> = card
                .related
                .iter()
                .take(EXPLAIN_MAX_RELATED)
                .map(String::as_str)
                .collect();
            out.push_str(&format!("\nСвязанные: {}\n", shown.join(", ")));
        }
        Some(out)
    }

    /// Обходит граф related-ссылок в ширину до глубины `depth` включительно
    /// (`depth == 1` — только прямые связи стартовой карточки).
    ///
    /// Сама стартовая карточка в результат не входит; циклы и повторные
    /// посещения отсекаются; ссылки на отсутствующие в индексе slug
    /// пропускаются. Порядок — BFS от стартовой карточки.
    pub fn related(&self, slug: &str, depth: usize) -> Vec<&ConceptCard> {
        let mut found = Vec::new();
        let mut seen: HashSet<&str> = HashSet::from([slug]);
        let mut frontier: Vec<&ConceptCard> = self.get(slug).into_iter().collect();
        for _ in 0..depth {
            if frontier.is_empty() {
                break;
            }
            let mut next = Vec::new();
            for card in &frontier {
                for link in &card.related {
                    if seen.insert(link.as_str()) {
                        if let Some(target) = self.get(link) {
                            found.push(target);
                            next.push(target);
                        }
                    }
                }
            }
            frontier = next;
        }
        found
    }

    /// Статистика индекса: (всего карточек, карта «тип → число карточек»).
    pub fn stats(&self) -> (usize, BTreeMap<String, usize>) {
        (self.cards.len(), self.by_type.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Временная библиотека карточек во временном каталоге; чистится при Drop.
    struct TestLib {
        root: PathBuf,
    }

    impl TestLib {
        fn new() -> TestLib {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("theseus_ml_concepts_{}_{stamp}", std::process::id()));
            TestLib { root }
        }

        /// Пишет файл по относительному пути (`<тип>/<slug>.md`).
        fn write_card(&self, rel_path: &str, text: &str) {
            let path = self.root.join(rel_path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        /// Строит индекс по временному корню.
        fn build(&self) -> ConceptIndex {
            ConceptIndex::build(&self.root)
        }
    }

    impl Drop for TestLib {
        fn drop(&mut self) {
            if let Err(err) = fs::remove_dir_all(&self.root) {
                eprintln!("не удалось убрать временный каталог {:?}: {err}", self.root);
            }
        }
    }

    /// Полная карточка: все поля + инлайн-списки + три секции в теле.
    const FULL_CARD: &str = r#"---
slug: grpo
type: algorithmic
level: β
formality: B
title: Group Relative Policy Optimization
aliases: [GRPO, group relative policy optimization]
related: [ppo, dr_grpo, kl_penalty]
family: RL post-training
sources: [arxiv.2402.03300]
---

## Определение
GRPO усредняет преимущество по группе ответов на один промпт и обходится без критика.

## Мотивация
PPO требует отдельной value-модели, что заметно увеличивает память при RL-обучении LLM.

## Отличие от альтернативы
В отличие от PPO, baseline вычисляется по группе сэмплов, а не обучаемым критиком.
"#;

    /// Минимальная корректная карточка: обязательный slug и тип.
    fn minimal_card(slug: &str, card_type: &str) -> String {
        format!(
            "---\nslug: {slug}\ntype: {card_type}\n---\n\n## Определение\n{slug} — тестовый концепт.\n"
        )
    }

    /// Библиотека для тестов поиска: запрос «grpo» цепляет карточки на всех
    /// четырёх уровнях весов + одна карточка-промах.
    fn search_fixture() -> (TestLib, ConceptIndex) {
        let lib = TestLib::new();
        lib.write_card("algorithmic/grpo.md", FULL_CARD);
        lib.write_card(
            "task/alias_hit.md",
            "---\nslug: alias_hit\ntype: task\ntitle: Alias Holder\naliases: [grpo]\n---\n\n## Определение\nКарточка без совпадений в заголовке и теле.\n",
        );
        lib.write_card(
            "task/title_hit.md",
            "---\nslug: title_hit\ntype: task\ntitle: All about GRPO tricks\n---\n\n## Определение\nПро групповую оптимизацию без явных слов.\n",
        );
        lib.write_card(
            "task/body_hit.md",
            "---\nslug: body_hit\ntype: task\ntitle: Body Holder\n---\n\n## Определение\nВнутри тела встречается метод grpo для сравнения.\n",
        );
        lib.write_card(
            "task/miss.md",
            "---\nslug: miss\ntype: task\ntitle: Совсем другая карточка\n---\n\n## Определение\nНичего общего.\n",
        );
        let index = lib.build();
        (lib, index)
    }

    #[test]
    fn parse_full_card_all_fields() {
        let card = parse_card(FULL_CARD).unwrap();
        assert_eq!(card.slug, "grpo");
        assert_eq!(card.card_type, "algorithmic");
        assert_eq!(card.level, "β");
        assert_eq!(card.formality, "B");
        assert_eq!(card.title, "Group Relative Policy Optimization");
        assert_eq!(card.aliases, ["GRPO", "group relative policy optimization"]);
        assert_eq!(card.related, ["ppo", "dr_grpo", "kl_penalty"]);
        assert_eq!(card.family, "RL post-training");
        assert_eq!(card.sources, ["arxiv.2402.03300"]);
        assert!(card.body.contains("## Определение"));
        assert!(card.body.contains("обходится без критика"));
        assert!(!card.body.contains("---"));
    }

    #[test]
    fn parse_block_lists() {
        let text = r#"---
slug: ppo
type: algorithmic
level: α
formality: A
title: Proximal Policy Optimization
aliases:
  - PPO
  - proximal policy optimization
related:
  - trpo
family: RL post-training
sources: [arxiv.1707.06347]
---

## Определение
Актор-критик с клиппингом отношения вероятностей.
"#;
        let card = parse_card(text).unwrap();
        assert_eq!(card.slug, "ppo");
        assert_eq!(card.card_type, "algorithmic");
        assert_eq!(card.aliases, ["PPO", "proximal policy optimization"]);
        assert_eq!(card.related, ["trpo"]);
        assert_eq!(card.sources, ["arxiv.1707.06347"]);
        assert_eq!(card.family, "RL post-training");
        assert_eq!(card.level, "α");
    }

    #[test]
    fn parse_empty_and_missing_fields() {
        let text = "---\nslug: sparse\ntype: benchmark\naliases: []\nrelated:\n---\nтело\n";
        let card = parse_card(text).unwrap();
        assert_eq!(card.slug, "sparse");
        assert!(card.aliases.is_empty());
        assert!(card.related.is_empty());
        assert!(card.sources.is_empty());
        assert!(card.title.is_empty());
        assert!(card.family.is_empty());
        assert!(card.level.is_empty());
        assert_eq!(card.body, "тело");
    }

    #[test]
    fn parse_quoted_values() {
        let text = "---\nslug: quoted\ntype: theorem\ntitle: \"Quoted: Title\"\naliases: ['Single', \"Double\"]\nfamily: 'Семья'\n---\n\n## Определение\nтекст\n";
        let card = parse_card(text).unwrap();
        // кавычки сняты, двоеточие внутри значения сохранилось
        assert_eq!(card.title, "Quoted: Title");
        assert_eq!(card.aliases, ["Single", "Double"]);
        assert_eq!(card.family, "Семья");
    }

    #[test]
    fn parse_missing_frontmatter_is_error() {
        assert!(parse_card("").is_err());
        assert!(parse_card("## Определение\nпросто текст").is_err());
    }

    #[test]
    fn parse_unterminated_frontmatter_is_error() {
        let text = "---\nslug: cut\ntype: algorithmic\n";
        assert!(parse_card(text).is_err());
    }

    #[test]
    fn parse_missing_slug_is_error() {
        let text = "---\ntype: algorithmic\ntitle: Без slug\n---\nтело\n";
        assert!(parse_card(text).is_err());
        let empty_slug = "---\nslug:\ntype: x\n---\nтело\n";
        assert!(parse_card(empty_slug).is_err());
    }

    #[test]
    fn parse_ignores_unknown_fields() {
        let text = "---\nslug: tolerant\ntype: task\nreviewed: true\ntags:\n  - extra\n  - fields\ntitle: Толерантная карточка\n---\nтело\n";
        let card = parse_card(text).unwrap();
        assert_eq!(card.slug, "tolerant");
        assert_eq!(card.title, "Толерантная карточка");
        assert!(card.aliases.is_empty());
        assert!(card.related.is_empty());
        assert_eq!(card.body, "тело");
    }

    #[test]
    fn build_counts_types_and_skips_broken() {
        let lib = TestLib::new();
        lib.write_card("theorem/one.md", &minimal_card("one", "theorem"));
        lib.write_card("theorem/two.md", &minimal_card("two", "theorem"));
        lib.write_card("benchmark/three.md", &minimal_card("three", "benchmark"));
        lib.write_card("benchmark/broken.md", "без frontmatter вовсе");
        lib.write_card("notes.txt", "---\nslug: not_md\n---\n"); // не .md — игнорируется
        let index = lib.build();
        let (total, by_type) = index.stats();
        assert_eq!(total, 3);
        assert_eq!(by_type.get("theorem"), Some(&2));
        assert_eq!(by_type.get("benchmark"), Some(&1));
        assert_eq!(by_type.len(), 2);
        assert!(index.get("one").is_some());
        assert!(index.get("broken").is_none());
        assert!(index.get("not_md").is_none());
    }

    #[test]
    fn get_found_and_missing() {
        let lib = TestLib::new();
        lib.write_card("task/alpha.md", &minimal_card("alpha", "task"));
        let index = lib.build();
        let card = index.get("alpha").unwrap();
        assert_eq!(card.card_type, "task");
        assert!(card.body.contains("тестовый концепт"));
        assert!(index.get("ghost").is_none());
    }

    #[test]
    fn search_ranks_by_match_tier() {
        let (_lib, index) = search_fixture();
        let hits = index.search("grpo", 10);
        let slugs: Vec<&str> = hits.iter().map(|card| card.slug.as_str()).collect();
        // точный slug (100) → alias (80) → title (60) → body (10); промаха нет
        assert_eq!(slugs, ["grpo", "alias_hit", "title_hit", "body_hit"]);
    }

    #[test]
    fn search_is_case_insensitive() {
        let (_lib, index) = search_fixture();
        let hits = index.search("GrPo", 10);
        assert_eq!(hits.len(), 4);
        assert_eq!(hits.first().unwrap().slug, "grpo");
    }

    #[test]
    fn search_limit_and_empty_query() {
        let (_lib, index) = search_fixture();
        assert_eq!(index.search("grpo", 2).len(), 2);
        assert!(index.search("grpo", 0).is_empty());
        assert!(index.search("", 10).is_empty());
        assert!(index.search("   ", 10).is_empty());
        assert!(index.search("nonexistent-token", 10).is_empty());
    }

    #[test]
    fn explain_renders_key_parts() {
        let lib = TestLib::new();
        lib.write_card("algorithmic/grpo.md", FULL_CARD);
        let index = lib.build();
        let text = index.explain("grpo").unwrap();
        assert!(text.contains("# Group Relative Policy Optimization (grpo)"));
        assert!(text.contains("Тип: algorithmic"));
        assert!(text.contains("Уровень: β"));
        assert!(text.contains("Формальность: B"));
        assert!(text.contains("Семейство: RL post-training"));
        assert!(text.contains("## Определение"));
        assert!(text.contains("обходится без критика"));
        assert!(text.contains("## Мотивация"));
        assert!(text.contains("value-модели"));
        assert!(text.contains("Связанные: ppo, dr_grpo, kl_penalty"));
        // посторонние секции в компактный рендер не попадают
        assert!(!text.contains("Отличие от альтернативы"));
    }

    #[test]
    fn explain_limits_related_to_five() {
        let lib = TestLib::new();
        lib.write_card(
            "task/hub.md",
            "---\nslug: hub\ntype: task\ntitle: Hub\nrelated: [lnk01, lnk02, lnk03, lnk04, lnk05, lnk06, lnk07]\n---\nтело\n",
        );
        let index = lib.build();
        let text = index.explain("hub").unwrap();
        assert!(text.contains("lnk05"));
        assert!(!text.contains("lnk06"));
        assert!(!text.contains("lnk07"));
        let line = text
            .lines()
            .find(|line| line.starts_with("Связанные: "))
            .unwrap();
        assert_eq!(line, "Связанные: lnk01, lnk02, lnk03, lnk04, lnk05");
    }

    #[test]
    fn explain_unknown_slug_is_none() {
        let index = TestLib::new().build();
        assert!(index.explain("ghost").is_none());
    }

    #[test]
    fn related_bfs_depth_cycles_and_dangling() {
        let lib = TestLib::new();
        lib.write_card("task/a.md", "---\nslug: a\ntype: task\ntitle: A\nrelated: [b, c]\n---\nтело\n");
        lib.write_card("task/b.md", "---\nslug: b\ntype: task\ntitle: B\nrelated: [d]\n---\nтело\n");
        lib.write_card(
            "task/c.md",
            "---\nslug: c\ntype: task\ntitle: C\nrelated: [d, ghost]\n---\nтело\n",
        );
        lib.write_card("task/d.md", "---\nslug: d\ntype: task\ntitle: D\nrelated: [a]\n---\nтело\n");
        let index = lib.build();
        let slugs = |depth| -> Vec<String> {
            index
                .related("a", depth)
                .iter()
                .map(|card| card.slug.clone())
                .collect()
        };
        assert!(slugs(0).is_empty());
        assert_eq!(slugs(1), ["b", "c"]);
        assert_eq!(slugs(2), ["b", "c", "d"]);
        // цикл d -> a не зацикливает обход: «a» уже посещена, «ghost» нет в индексе
        assert_eq!(slugs(3), ["b", "c", "d"]);
        assert!(index.related("ghost", 2).is_empty());
    }

    #[test]
    fn duplicate_slug_first_wins() {
        let lib = TestLib::new();
        lib.write_card("a_type/dup.md", "---\nslug: dup\ntype: a_type\ntitle: Первая\n---\nтело\n");
        lib.write_card("b_type/dup.md", "---\nslug: dup\ntype: b_type\ntitle: Вторая\n---\nтело\n");
        let index = lib.build();
        let (total, by_type) = index.stats();
        assert_eq!(total, 1);
        // пути отсортированы: a_type/dup.md идёт раньше b_type/dup.md
        assert_eq!(index.get("dup").unwrap().title, "Первая");
        assert_eq!(by_type.get("a_type"), Some(&1));
        assert!(!by_type.contains_key("b_type"));
    }

    /// Мягкий тест на реальной библиотеке: при отсутствии каталога — skip.
    /// Чтобы не читать десятки тысяч файлов, индекс строится по самому
    /// маленькому подкаталогу, в котором нашлась хотя бы одна валидная карточка.
    #[test]
    fn real_library_soft() {
        let root = Path::new("/home/roman/library/concepts");
        if !root.is_dir() {
            eprintln!("SKIP: {root:?} отсутствует — тест пропущен");
            return;
        }
        let count_md = |dir: &Path| -> usize {
            fs::read_dir(dir)
                .map(|rd| {
                    rd.flatten()
                        .filter(|f| f.path().extension().is_some_and(|ext| ext == "md"))
                        .count()
                })
                .unwrap_or(0)
        };
        let mut dirs: Vec<(usize, PathBuf)> = fs::read_dir(root)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| {
                let path = entry.path();
                (count_md(&path), path)
            })
            .collect();
        dirs.sort();
        let mut built = None;
        for (_, dir) in dirs.into_iter().filter(|(count, _)| *count > 0).take(8) {
            let index = ConceptIndex::build(&dir);
            if index.stats().0 > 0 {
                built = Some((dir, index));
                break;
            }
        }
        let Some((dir, index)) = built else {
            eprintln!("SKIP: в маленьких подкаталогах {root:?} нет валидных карточек");
            return;
        };
        let (total, by_type) = index.stats();
        assert!(total > 0);
        assert_eq!(by_type.values().sum::<usize>(), total);
        let card = index.cards.first().unwrap();
        // get и explain работают на реальной карточке
        assert_eq!(index.get(&card.slug).unwrap().slug, card.slug);
        let rendered = index.explain(&card.slug).unwrap();
        assert!(rendered.contains(card.slug.as_str()));
        // точный slug всегда находится поиском с весом 100
        assert_eq!(index.search(&card.slug, 5).first().unwrap().slug, card.slug);
        // слово из заголовка (если заголовок непустой) тоже что-то находит
        if let Some(word) = card.title.split_whitespace().next() {
            assert!(!index.search(word, 5).is_empty());
        }
        eprintln!("реальная библиотека: {total} карточек в {}", dir.display());
    }
}
