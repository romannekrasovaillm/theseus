//! Проверка обновлений версии харнесса: разбор списка релизов в формате
//! GitHub releases API и сравнение с текущей версией.
//!
//! Модуль сознательно **offline-дружелюбен**: сетевых вызовов здесь нет.
//! JSON со списком релизов доставляет вызывающая сторона (свежий ответ
//! API или закэшированная копия с прошлого сеанса — без разницы), а вся
//! логика «что новее и стоит ли сообщать пользователю» собрана здесь и
//! не паникует: любой некорректный ввод деградирует в пустой список или
//! [`UpdateVerdict::ParseError`].
//!
//! Формат входа — массив объектов GitHub releases API; используются поля
//! `tag_name`, `body`, `html_url`, `prerelease`, `draft`, остальные
//! игнорируются:
//!
//! ```json
//! [{"tag_name": "v0.3.0", "body": "...", "html_url": "https://...",
//!   "prerelease": false, "draft": false}]
//! ```
//!
//! Фильтрация двухуровневая:
//!
//! * при разборе ([`parse_releases_json`]) отбрасываются черновики
//!   (`draft: true`), записи с флагом `prerelease: true` и записи, чей
//!   `tag_name` не разбирается как [`Version`] (теги вида `nightly-...`);
//! * [`latest_stable`] дополнительно отсекает предрелизы по суффиксу
//!   версии (`0.4.0-rc.1`) — на случай, если флаг `prerelease` в JSON не
//!   выставлен, а тег предрелизный.
//!
//! [`ReleaseInfo`] хранит только версию, заметки и ссылку, поэтому флаги
//! после разбора не сохраняются: предрелизность восстанавливается из
//! самой версии ([`Version::is_prerelease`]), а черновики публичному
//! пользователю не видны в принципе.

use crate::semver::Version;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Публичная модель релиза
// ---------------------------------------------------------------------------

/// Информация о релизе харнесса, необходимая для проверки обновлений.
///
/// Собирается из записи GitHub releases API: `tag_name` → `version`,
/// `body` → `notes`, `html_url` → `url`. Флаги `draft`/`prerelease` в
/// структуре не сохраняются (см. документацию модуля): предрелизность
/// определяется по [`Version::is_prerelease`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// Версия релиза (тег без префикса `v`).
    pub version: Version,
    /// Заметки к релизу (`body`); пустая строка, если поля не было.
    pub notes: String,
    /// Ссылка на страницу релиза (`html_url`); пустая, если поля не было.
    pub url: String,
}

// ---------------------------------------------------------------------------
// Разбор JSON
// ---------------------------------------------------------------------------

/// Сырая запись GitHub releases API: все поля необязательные, лишние
/// поля JSON (`name`, `published_at`, `assets` и т.п.) serde игнорирует.
#[derive(Debug, Deserialize)]
struct RawRelease {
    /// Тег релиза (`v0.3.0`); без тега запись бесполезна — пропускаем.
    #[serde(default)]
    tag_name: Option<String>,
    /// Заметки к релизу; бывает `null` или отсутствует у старых записей.
    #[serde(default)]
    body: Option<String>,
    /// Ссылка на страницу релиза.
    #[serde(default)]
    html_url: Option<String>,
    /// Флаг предрелиза на стороне GitHub.
    #[serde(default)]
    prerelease: bool,
    /// Флаг черновика на стороне GitHub.
    #[serde(default)]
    draft: bool,
}

impl RawRelease {
    /// Превратить сырую запись в [`ReleaseInfo`]; `None`, если запись
    /// непригодна: черновик, помеченный предрелиз, отсутствующий или
    /// неразбираемый тег.
    fn into_release_info(self) -> Option<ReleaseInfo> {
        if self.draft || self.prerelease {
            return None;
        }
        let version = Version::parse(&self.tag_name?).ok()?;
        Some(ReleaseInfo {
            version,
            notes: self.body.unwrap_or_default(),
            url: self.html_url.unwrap_or_default(),
        })
    }
}

/// Разобрать JSON строго: ошибка, если вход структурно бит (не массив
/// объектов релизов). Непригодные отдельные записи ошибкой не считаются —
/// они пропускаются.
///
/// Внутренняя fallible-версия: публичная [`parse_releases_json`] прячет
/// ошибку за пустым списком, а [`check_against`] использует её, чтобы
/// отличать битый JSON от честно пустого списка релизов.
fn try_parse_releases(json: &str) -> Result<Vec<ReleaseInfo>, serde_json::Error> {
    let raw: Vec<RawRelease> = serde_json::from_str(json)?;
    Ok(raw.into_iter().filter_map(RawRelease::into_release_info).collect())
}

/// Разобрать ответ GitHub releases API в список пригодных релизов.
///
/// Никогда не паникует и не возвращает ошибку: структурно битый JSON даёт
/// пустой список, а непригодные записи (черновики, помеченные предрелизы,
/// неразбираемые или отсутствующие теги) молча пропускаются — для
/// UI-путей, где «нет данных» и «кэш побит» выглядят одинаково. Отличить
/// эти случаи позволяет [`check_against`].
///
/// Порядок записей совпадает с порядком в JSON (обычно GitHub отдаёт от
/// новых к старым, но [`latest_stable`] на позицию не полагается).
#[must_use]
pub fn parse_releases_json(json: &str) -> Vec<ReleaseInfo> {
    try_parse_releases(json).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Выбор последнего стабильного релиза
// ---------------------------------------------------------------------------

/// Найти последний стабильный релиз: максимальную версию без
/// предрелизного суффикса.
///
/// Черновики и помеченные предрелизы сюда обычно не доходят (отфильтрованы
/// при разборе), но функция самодостаточна: предрелизы вида `0.4.0-rc.1`
/// отсекаются по [`Version::is_prerelease`], а максимум берётся по
/// semver-порядку, а не по позиции в списке. Пустой список и список
/// «только предрелизы» дают `None`.
#[must_use]
pub fn latest_stable(releases: &[ReleaseInfo]) -> Option<&ReleaseInfo> {
    releases
        .iter()
        .filter(|r| !r.version.is_prerelease())
        .max_by(|a, b| a.version.cmp(&b.version))
}

// ---------------------------------------------------------------------------
// Сравнение и сообщение
// ---------------------------------------------------------------------------

/// `true`, если `latest` строго новее `current` по semver-порядку.
///
/// Стабильный релиз новее своего предрелиза (`0.3.1` > `0.3.1-beta.1`),
/// поэтому пользователю предрелиза о выходе стабильной версии сообщим.
#[must_use]
pub fn is_newer(current: &Version, latest: &Version) -> bool {
    latest > current
}

/// Построить однострочное сообщение о доступном обновлении.
///
/// Возвращает `Some` вида «доступна версия X (у вас Y): url», только если
/// [`is_newer`] подтверждает, что `latest` действительно новее; иначе
/// `None` — чтобы вызывающая сторона не могла случайно показать сообщение
/// о «даунгрейде».
#[must_use]
pub fn update_message(current: &Version, latest: &ReleaseInfo) -> Option<String> {
    is_newer(current, &latest.version).then(|| {
        format!(
            "доступна версия {} (у вас {current}): {}",
            latest.version, latest.url
        )
    })
}

// ---------------------------------------------------------------------------
// Итог проверки
// ---------------------------------------------------------------------------

/// Результат проверки обновлений против списка релизов.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateVerdict {
    /// Текущая версия не старше последнего стабильного релиза (или
    /// пригодных релизов нет вовсе) — беспокоить пользователя нечем.
    UpToDate,
    /// Найден более новый стабильный релиз.
    NewerAvailable {
        /// Информация о новейшем стабильном релизе.
        info: ReleaseInfo,
    },
    /// Некорректный вход: не разобралась строка текущей версии или JSON
    /// релизов структурно бит (не массив объектов). Непригодные отдельные
    /// записи ошибкой не считаются — они пропускаются.
    ParseError,
}

/// Проверить обновление «в один вызов»: разобрать текущую версию и JSON
/// релизов, найти последний стабильный и сравнить.
///
/// * текущая версия не разбирается или JSON структурно бит →
///   [`UpdateVerdict::ParseError`];
/// * последний стабильный релиз новее текущей версии →
///   [`UpdateVerdict::NewerAvailable`];
/// * иначе (в том числе текущая версия новее всех релизов или список
///   релизов пуст) → [`UpdateVerdict::UpToDate`].
#[must_use]
pub fn check_against(current_str: &str, releases_json: &str) -> UpdateVerdict {
    let Ok(current) = Version::parse(current_str) else {
        return UpdateVerdict::ParseError;
    };
    let Ok(releases) = try_parse_releases(releases_json) else {
        return UpdateVerdict::ParseError;
    };
    match latest_stable(&releases) {
        Some(info) if is_newer(&current, &info.version) => UpdateVerdict::NewerAvailable {
            info: info.clone(),
        },
        _ => UpdateVerdict::UpToDate,
    }
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Короткий конструктор версии для тестов: разбор или паника.
    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    /// Собрать релиз вручную, без JSON.
    fn rel(version: &str, notes: &str, url: &str) -> ReleaseInfo {
        ReleaseInfo {
            version: v(version),
            notes: notes.to_string(),
            url: url.to_string(),
        }
    }

    /// Фикстура в формате GitHub releases API: стабильные релизы,
    /// предрелиз с флагом, бета без флага, черновик, запись с
    /// неразбираемым тегом, запись без `tag_name`, запись без
    /// необязательных полей и запись с лишним полем.
    const FIXTURE: &str = r#"[
        {
            "tag_name": "v0.4.0-rc.1",
            "body": "Кандидат в релизы 0.4.0",
            "html_url": "https://github.com/example/theseus/releases/tag/v0.4.0-rc.1",
            "prerelease": true,
            "draft": false
        },
        {
            "tag_name": "v0.3.2-beta.1",
            "body": "Бета без флага prerelease",
            "html_url": "https://github.com/example/theseus/releases/tag/v0.3.2-beta.1",
            "prerelease": false,
            "draft": false
        },
        {
            "tag_name": "v0.3.1",
            "body": "Исправления: враппинг в TUI, гонка в планировщике",
            "html_url": "https://github.com/example/theseus/releases/tag/v0.3.1",
            "prerelease": false,
            "draft": false,
            "published_at": "2026-07-10T12:00:00Z"
        },
        {
            "tag_name": "v0.3.0",
            "body": "Планировщик фоновых задач",
            "html_url": "https://github.com/example/theseus/releases/tag/v0.3.0",
            "prerelease": false,
            "draft": false
        },
        {
            "tag_name": "v0.3.5-wip",
            "body": "Черновик заметок",
            "html_url": "https://github.com/example/theseus/releases/tag/v0.3.5-wip",
            "prerelease": false,
            "draft": true
        },
        {
            "tag_name": "nightly-2026-07",
            "body": "Ночная сборка без semver-тега",
            "html_url": "https://github.com/example/theseus/releases/tag/nightly",
            "prerelease": false,
            "draft": false
        },
        {
            "tag_name": "v0.2.0"
        },
        {
            "body": "Запись вообще без tag_name",
            "html_url": "https://github.com/example/theseus/releases/tag/untagged",
            "prerelease": false,
            "draft": false
        }
    ]"#;

    /// Разбор фикстуры: остаются только пригодные записи, порядок
    /// сохраняется; черновик, помеченный предрелиз, неразбираемый тег и
    /// запись без тега отброшены.
    #[test]
    fn parse_fixture_keeps_only_usable_releases() {
        let versions: Vec<Version> = parse_releases_json(FIXTURE)
            .into_iter()
            .map(|r| r.version)
            .collect();
        assert_eq!(
            versions,
            [
                v("0.3.2-beta.1"),
                v("0.3.1"),
                v("0.3.0"),
                v("0.2.0")
            ]
        );
    }

    /// Поля переносятся один в один; у записи без `body`/`html_url`
    /// заметки и ссылка — пустые строки, а не ошибка.
    #[test]
    fn parse_fixture_maps_fields_and_tolerates_missing() {
        let releases = parse_releases_json(FIXTURE);
        let r031 = releases
            .iter()
            .find(|r| r.version == v("0.3.1"))
            .unwrap();
        assert_eq!(
            r031.notes,
            "Исправления: враппинг в TUI, гонка в планировщике"
        );
        assert_eq!(
            r031.url,
            "https://github.com/example/theseus/releases/tag/v0.3.1"
        );

        let r020 = releases
            .iter()
            .find(|r| r.version == v("0.2.0"))
            .unwrap();
        assert!(r020.notes.is_empty());
        assert!(r020.url.is_empty());
    }

    /// Флаг `prerelease: true` отбрасывает запись даже со стабильным на
    /// вид тегом; флаг `draft: true` — всегда. Предрелиз по суффиксу
    /// версии без флага разбором сохраняется (его отфильтрует
    /// `latest_stable`).
    #[test]
    fn parse_filters_draft_and_flagged_prerelease() {
        let json = r#"[
            {"tag_name": "v1.0.0", "prerelease": true, "draft": false},
            {"tag_name": "v1.1.0", "prerelease": false, "draft": true},
            {"tag_name": "v1.2.0-rc.1", "prerelease": false, "draft": false},
            {"tag_name": "v0.9.0", "prerelease": false, "draft": false}
        ]"#;
        let versions: Vec<Version> = parse_releases_json(json)
            .into_iter()
            .map(|r| r.version)
            .collect();
        assert_eq!(versions, [v("1.2.0-rc.1"), v("0.9.0")]);
    }

    /// Пустой массив — честный пустой список, без ошибки.
    #[test]
    fn parse_empty_array_is_empty_list() {
        assert!(parse_releases_json("[]").is_empty());
        assert!(parse_releases_json("  [  ]  ").is_empty());
    }

    /// Структурно битый вход инфаллибильный разбор прячет за пустым
    /// списком: обрезанный JSON, пустая строка, не-массив, массив
    /// не-объектов.
    #[test]
    fn parse_broken_json_is_empty_list() {
        for json in ["", "{не-json", "null", "{}", "[1, 2, 3]", "[\"x\"]"] {
            assert!(
                parse_releases_json(json).is_empty(),
                "вход: «{json}»"
            );
        }
    }

    /// `latest_stable` берёт максимум по semver-порядку, а не по позиции
    /// в списке, и игнорирует предрелизы — даже если предрелиз новее
    /// всех стабильных.
    #[test]
    fn latest_stable_picks_max_by_version_not_position() {
        let releases = vec![
            rel("0.2.0", "старое", "u/0.2.0"),
            rel("0.3.1", "новейшее стабильное", "u/0.3.1"),
            rel("0.3.2-beta.1", "предрелиз новее всех", "u/0.3.2-beta.1"),
            rel("0.3.0", "среднее", "u/0.3.0"),
        ];
        let latest = latest_stable(&releases).unwrap();
        assert_eq!(latest.version, v("0.3.1"));
        assert_eq!(latest.notes, "новейшее стабильное");
        assert_eq!(latest.url, "u/0.3.1");
    }

    /// На фикстуре `latest_stable` выбирает `0.3.1`: `0.4.0-rc.1` отброшен
    /// при разборе по флагу, `0.3.2-beta.1` — здесь по суффиксу версии.
    #[test]
    fn latest_stable_from_fixture() {
        let releases = parse_releases_json(FIXTURE);
        let latest = latest_stable(&releases).unwrap();
        assert_eq!(latest.version, v("0.3.1"));
        assert!(!latest.notes.is_empty());
    }

    /// Пустой список и список «только предрелизы» стабильного релиза
    /// не содержат.
    #[test]
    fn latest_stable_none_on_empty_or_only_prereleases() {
        assert!(latest_stable(&[]).is_none());
        let pre_only = vec![rel("0.1.0-rc.1", "", "u/a"), rel("0.2.0-alpha", "", "u/b")];
        assert!(latest_stable(&pre_only).is_none());
    }

    /// Таблица истинности `is_newer`: строгое «новее», включая порядок
    /// предрелизов (стабильный релиз новее своего предрелиза).
    #[test]
    fn is_newer_truth_table() {
        let newer: &[(&str, &str)] = &[
            ("0.2.0", "0.3.0"),
            ("0.3.0", "0.3.1"),
            ("1.9.9", "2.0.0"),
            ("0.3.1-beta.1", "0.3.1"),
        ];
        for (current, latest) in newer {
            assert!(
                is_newer(&v(current), &v(latest)),
                "{current} → {latest} должно быть «новее»"
            );
        }

        let not_newer: &[(&str, &str)] = &[
            ("0.3.0", "0.3.0"), // равные — не новее
            ("0.3.1", "0.3.0"), // текущая новее
            ("2.0.0", "1.9.9"),
            ("0.3.1", "0.3.1-beta.1"), // предрелиз младше релиза
        ];
        for (current, latest) in not_newer {
            assert!(
                !is_newer(&v(current), &v(latest)),
                "{current} → {latest} не должно быть «новее»"
            );
        }
    }

    /// Точный формат сообщения: «доступна версия X (у вас Y): url».
    #[test]
    fn update_message_format_is_exact() {
        let latest = rel(
            "0.3.1",
            "заметки в сообщение не входят",
            "https://github.com/example/theseus/releases/tag/v0.3.1",
        );
        let msg = update_message(&v("0.2.0"), &latest).unwrap();
        assert_eq!(
            msg,
            "доступна версия 0.3.1 (у вас 0.2.0): \
             https://github.com/example/theseus/releases/tag/v0.3.1"
        );
    }

    /// Сообщения нет, если новее не стало: равные версии и «даунгрейд».
    #[test]
    fn update_message_none_when_not_newer() {
        let latest = rel("0.3.1", "", "u/0.3.1");
        assert!(update_message(&v("0.3.1"), &latest).is_none());
        assert!(update_message(&v("0.3.2"), &latest).is_none());
    }

    /// Полный путь: на фикстуре пользователю `0.2.0` сообщаем о `0.3.1`
    /// (а не о `0.4.0-rc.1` и не о черновике `0.3.5-wip`).
    #[test]
    fn check_against_reports_newer_available() {
        let verdict = check_against("0.2.0", FIXTURE);
        let expected = UpdateVerdict::NewerAvailable {
            info: rel(
                "0.3.1",
                "Исправления: враппинг в TUI, гонка в планировщике",
                "https://github.com/example/theseus/releases/tag/v0.3.1",
            ),
        };
        assert_eq!(verdict, expected);
        // Префикс v и пробелы в строке текущей версии допустимы.
        assert_eq!(check_against(" v0.2.0 ", FIXTURE), expected);
    }

    /// Текущая версия равна последнему стабильному — обновления нет.
    #[test]
    fn check_against_equal_is_up_to_date() {
        assert_eq!(check_against("0.3.1", FIXTURE), UpdateVerdict::UpToDate);
    }

    /// Текущая версия НОВЕЕ всех релизов в списке (например, локальная
    /// сборка из main) — тоже `UpToDate`, сообщения о «даунгрейде» нет.
    #[test]
    fn check_against_current_newer_is_up_to_date() {
        assert_eq!(check_against("0.9.9", FIXTURE), UpdateVerdict::UpToDate);
        assert_eq!(check_against("1.0.0", FIXTURE), UpdateVerdict::UpToDate);
    }

    /// Пользователь предрелиза узнаёт о выходе стабильной версии:
    /// `0.3.1-rc.1` < `0.3.1`.
    #[test]
    fn check_against_prerelease_user_notified_about_stable() {
        let verdict = check_against("0.3.1-rc.1", FIXTURE);
        assert!(matches!(
            verdict,
            UpdateVerdict::NewerAvailable { ref info } if info.version == v("0.3.1")
        ));
    }

    /// Битый JSON — `ParseError`, а не молчаливый `UpToDate`: обрезанный
    /// ввод, не-массив, массив не-объектов.
    #[test]
    fn check_against_broken_json_is_parse_error() {
        for json in ["", "{не-json", "{}", "[1, 2, 3]", "null"] {
            assert_eq!(
                check_against("0.2.0", json),
                UpdateVerdict::ParseError,
                "вход: «{json}»"
            );
        }
    }

    /// Неразбираемая строка текущей версии — `ParseError` (даже при
    /// валидном JSON релизов).
    #[test]
    fn check_against_broken_current_version_is_parse_error() {
        for current in ["", "  ", "абв", "1.2.3.4", "1.2.3+build"] {
            assert_eq!(
                check_against(current, FIXTURE),
                UpdateVerdict::ParseError,
                "вход: «{current}»"
            );
        }
    }

    /// Валидный, но пустой список релизов — `UpToDate`: проверка прошла,
    /// обновляться некуда.
    #[test]
    fn check_against_empty_releases_is_up_to_date() {
        assert_eq!(check_against("0.1.0", "[]"), UpdateVerdict::UpToDate);
    }
}
