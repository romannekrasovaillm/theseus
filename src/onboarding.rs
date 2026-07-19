//! Онбординг первого запуска (уроки codex onboarding и claude /init): оценка готовности
//! окружения (конфиг, API-ключ, workspace, скиллы), приветственный текст со статусами
//! ✅/❌ и следующими шагами, чек-лист готовности, маркер первого запуска и стартовые
//! промпты для ML-инженерии. Модуль самодостаточен: std + toml (разбор `api_key`).

use std::path::{Path, PathBuf};

/// Имя env-переменной с API-ключом (совпадает с конвенцией config.rs).
const ENV_API_KEY: &str = "DEEPSEEK_API_KEY";

/// Маркер-файл первого запуска (относительно домашнего каталога пользователя).
const MARKER_RELATIVE_PATH: &str = ".theseus/onboarded.marker";

/// Снимок готовности харнесса к работе, собранный при старте.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OnboardingState {
    /// Найден ли хотя бы один конфиг: `~/.config/theseus/config.toml` или `./.theseus/config.toml`.
    pub config_exists: bool,
    /// Задан ли API-ключ: env `DEEPSEEK_API_KEY` или непустой `api_key` в конфиге.
    pub key_set: bool,
    /// Существует ли рабочий каталог и доступен ли он на чтение.
    pub workspace_ok: bool,
    /// Число скиллов в каталогах по умолчанию (`./.theseus/skills`, `~/.theseus/skills`).
    pub skills_found: usize,
}

impl OnboardingState {
    /// `true`, когда все четыре проверки пройдены и харнесс готов к работе.
    pub fn is_ready(&self) -> bool {
        self.config_exists && self.key_set && self.workspace_ok && self.skills_found > 0
    }
}

/// Один пункт чек-листа готовности.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecklistItem {
    /// Машинное имя проверки: `config`, `api_key`, `workspace` или `skills`.
    pub item: String,
    /// Пройдена ли проверка.
    pub ok: bool,
    /// Что сделать, чтобы закрыть пункт. Пустая строка, когда `ok == true`.
    pub hint: String,
}

/// Оценить готовность окружения: читает env `DEEPSEEK_API_KEY` и файловую систему.
///
/// Чистая разведка перед показом онбординга: ничего не создаёт и не изменяет.
pub fn assess(home_dir: &Path, workspace: &Path) -> OnboardingState {
    let env_key = std::env::var(ENV_API_KEY).ok();
    assess_with_env(home_dir, workspace, env_key.as_deref())
}

/// Приветственный текст первого запуска на русском: приветствие, статусы ✅/❌
/// по каждой проверке и пронумерованные следующие шаги для незакрытых пунктов.
/// Когда всё готово — вместо шагов показываем стартовые промпты.
pub fn welcome_text(state: &OnboardingState) -> String {
    let mut out = String::from(
        "Добро пожаловать в Theseus — агентный харнесс!\n\nСтатус окружения:\n",
    );
    out.push_str(&status_line(state.config_exists, "конфиг theseus (config.toml)"));
    out.push_str(&status_line(
        state.key_set,
        "API-ключ (DEEPSEEK_API_KEY или api_key в config.toml)",
    ));
    out.push_str(&status_line(state.workspace_ok, "рабочий каталог проекта"));
    let skills_icon = if state.skills_found > 0 { "✅" } else { "❌" };
    out.push_str(&format!("{skills_icon} скиллы: обнаружено {}\n", state.skills_found));

    if state.is_ready() {
        out.push_str("\nВсё готово к работе! С чего начать:\n");
        for (i, prompt) in suggested_starter_prompts().iter().enumerate() {
            out.push_str(&format!("{}. {prompt}\n", i + 1));
        }
    } else {
        out.push_str("\nСледующие шаги:\n");
        let mut step = 0;
        for item in readiness_checklist(state).iter().filter(|it| !it.ok) {
            step += 1;
            out.push_str(&format!("{step}. {}\n", item.hint));
        }
    }
    out
}

/// Чек-лист готовности в фиксированном порядке: `config` → `api_key` → `workspace` → `skills`.
/// Подсказка (`hint`) заполнена только у непройденных пунктов.
pub fn readiness_checklist(state: &OnboardingState) -> Vec<ChecklistItem> {
    let make = |item: &str, ok: bool, hint: &str| ChecklistItem {
        item: item.to_string(),
        ok,
        hint: if ok { String::new() } else { hint.to_string() },
    };
    vec![
        make(
            "config",
            state.config_exists,
            "создайте конфиг: `theseus init` запишет ~/.config/theseus/config.toml \
             (или заведите ./.theseus/config.toml в проекте)",
        ),
        make(
            "api_key",
            state.key_set,
            "задайте ключ: export DEEPSEEK_API_KEY=… или строка `api_key = \"…\"` в config.toml",
        ),
        make(
            "workspace",
            state.workspace_ok,
            "запустите theseus из существующего каталога проекта (нужен доступ на чтение)",
        ),
        make(
            "skills",
            state.skills_found > 0,
            "добавьте скиллы в .theseus/skills: каталог `<имя>/SKILL.md` или файл `<имя>.md`",
        ),
    ]
}

/// Детект первого запуска по маркер-файлу `~/.theseus/onboarded.marker`.
///
/// Маркер отсутствует → создаём его (вместе с каталогом) и возвращаем `Ok(true)` —
/// это первый запуск; маркер уже есть → `Ok(false)`. Ошибки ФС пробрасываются наверх.
pub fn first_run_marker(home_dir: &Path) -> std::io::Result<bool> {
    let marker = home_dir.join(MARKER_RELATIVE_PATH);
    if marker.exists() {
        return Ok(false);
    }
    if let Some(dir) = marker.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    std::fs::write(&marker, format!("theseus first-run marker\nunix_secs: {secs}\n"))?;
    Ok(true)
}

/// Стартовые промпты для ML-инженерии (3–4 шт.): типовые сценарии для первого диалога.
pub fn suggested_starter_prompts() -> Vec<&'static str> {
    vec![
        "Собери датасет: прочитай все parquet-файлы в ./data, посчитай распределение длин \
         текстов и запиши сводку в report.md",
        "Запусти train.py, дождись конца обучения и построй кривые loss по логам из ./runs",
        "Сравни два чекпоинта из ./ckpts по eval-метрикам и объясни, какой лучше и почему",
        "Профилируй шаг обучения: найди, где теряется время в даталоадере, и предложи фикс",
    ]
}

// ---------- внутренние помощники ----------

/// `assess` с явной подстановкой env-ключа: детерминированные тесты и вызовы
/// из контекстов, где ключ получен иначе (keyring, прокси).
fn assess_with_env(home_dir: &Path, workspace: &Path, env_key: Option<&str>) -> OnboardingState {
    let configs = config_paths(home_dir, workspace);
    let key_from_env = env_key.is_some_and(|k| !k.trim().is_empty());
    let key_from_config = configs
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .any(|text| config_has_key(&text));
    OnboardingState {
        config_exists: configs.iter().any(|p| p.is_file()),
        key_set: key_from_env || key_from_config,
        workspace_ok: workspace.is_dir() && std::fs::read_dir(workspace).is_ok(),
        skills_found: count_skills(&skill_dirs(home_dir, workspace)),
    }
}

/// Строка статуса с иконкой ✅/❌.
fn status_line(ok: bool, label: &str) -> String {
    let icon = if ok { "✅" } else { "❌" };
    format!("{icon} {label}\n")
}

/// Пути к конфигам theseus: пользовательский и проектный (конвенция config.rs).
fn config_paths(home_dir: &Path, workspace: &Path) -> [PathBuf; 2] {
    [
        home_dir.join(".config/theseus/config.toml"),
        workspace.join(".theseus/config.toml"),
    ]
}

/// Каталоги поиска скиллов по умолчанию (конвенция skills.rs).
fn skill_dirs(home_dir: &Path, workspace: &Path) -> [PathBuf; 2] {
    [
        workspace.join(".theseus/skills"),
        home_dir.join(".theseus/skills"),
    ]
}

/// Есть ли непустой `api_key` в TOML-тексте конфига. Битый TOML трактуем как «ключа нет».
fn config_has_key(text: &str) -> bool {
    let Ok(value) = text.parse::<toml::Value>() else {
        return false;
    };
    value
        .get("api_key")
        .and_then(|k| k.as_str())
        .is_some_and(|s| !s.trim().is_empty())
}

/// Быстрый подсчёт скиллов без разбора frontmatter (онбординг не валидирует содержимое):
/// считаем каталоги `<dir>/<имя>/SKILL.md` и плоские файлы `<dir>/<имя>.md`.
fn count_skills(dirs: &[PathBuf]) -> usize {
    let mut found = 0;
    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_skill = if path.is_dir() {
                path.join("SKILL.md").is_file()
            } else {
                path.extension().is_some_and(|ext| ext == "md")
            };
            if is_skill {
                found += 1;
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Конфиг без ключа.
    const CONFIG_NO_KEY: &str = "model = \"deepseek-chat\"\n";
    /// Конфиг с ключом.
    const CONFIG_WITH_KEY: &str = "model = \"deepseek-chat\"\napi_key = \"sk-test-123\"\n";

    /// Уникальный временный каталог на тест (параллельный прогон безопасен).
    fn tempdir(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "theseus_onboarding_test_{}_{tag}_{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Записать пользовательский конфиг `home/.config/theseus/config.toml`.
    fn write_user_config(home: &Path, body: &str) {
        let path = home.join(".config/theseus/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// Готовая фикстура: home + ws (оба существуют), без конфига и скиллов.
    fn fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = tempdir(tag);
        let home = base.join("home");
        let ws = base.join("ws");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        (base, home, ws)
    }

    #[test]
    fn assess_covers_all_flag_combinations() {
        // Все 8 комбинаций config_exists × key_set × workspace_ok.
        for has_config in [true, false] {
            for has_key in [true, false] {
                for ws_ok in [true, false] {
                    let base = tempdir("combo");
                    let home = base.join("home");
                    std::fs::create_dir_all(&home).unwrap();
                    let workspace = base.join("ws");
                    if ws_ok {
                        std::fs::create_dir_all(&workspace).unwrap();
                    }
                    // Ключ берём из конфига, если конфиг есть; иначе — из env.
                    let env_key = if !has_config && has_key { Some("sk-env") } else { None };
                    if has_config && has_key {
                        write_user_config(&home, CONFIG_WITH_KEY);
                    } else if has_config {
                        write_user_config(&home, CONFIG_NO_KEY);
                    }
                    let state = assess_with_env(&home, &workspace, env_key);
                    let ctx = format!("has_config={has_config} has_key={has_key} ws_ok={ws_ok}");
                    assert_eq!(state.config_exists, has_config, "config_exists, {ctx}");
                    assert_eq!(state.key_set, has_key, "key_set, {ctx}");
                    assert_eq!(state.workspace_ok, ws_ok, "workspace_ok, {ctx}");
                    assert_eq!(state.skills_found, 0, "skills_found, {ctx}");
                    // Без скиллов (skills_found == 0) готовности нет ни в одной комбинации.
                    assert!(!state.is_ready(), "ready, {ctx}");
                    std::fs::remove_dir_all(&base).unwrap();
                }
            }
        }
    }

    #[test]
    fn assess_counts_skills_in_default_dirs() {
        let (base, home, ws) = fixture("skills");
        // Workspace-скиллы: каталог с SKILL.md + плоский .md; .txt — шум.
        let ws_skills = ws.join(".theseus/skills");
        std::fs::create_dir_all(ws_skills.join("ml-debug")).unwrap();
        std::fs::write(ws_skills.join("ml-debug").join("SKILL.md"), "---\nname: ml-debug\n---\n").unwrap();
        std::fs::write(ws_skills.join("notes.md"), "# заметки\n").unwrap();
        std::fs::write(ws_skills.join("ignore.txt"), "шум\n").unwrap();
        // Home-скиллы: каталог без SKILL.md не считается, с SKILL.md — считается.
        let home_skills = home.join(".theseus/skills");
        std::fs::create_dir_all(home_skills.join("empty-dir")).unwrap();
        std::fs::create_dir_all(home_skills.join("grpo-tips")).unwrap();
        std::fs::write(home_skills.join("grpo-tips").join("SKILL.md"), "---\nname: grpo-tips\n---\n").unwrap();

        let state = assess_with_env(&home, &ws, None);
        assert_eq!(state.skills_found, 3);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn assess_rejects_file_as_workspace() {
        let (base, home, _ws) = fixture("wsfile");
        let file_ws = base.join("not-a-dir");
        std::fs::write(&file_ws, "я файл, а не каталог").unwrap();
        let state = assess_with_env(&home, &file_ws, None);
        assert!(!state.workspace_ok);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn assess_ignores_blank_env_key() {
        let (base, home, ws) = fixture("blankenv");
        assert!(!assess_with_env(&home, &ws, Some("")).key_set);
        assert!(!assess_with_env(&home, &ws, Some("   ")).key_set);
        assert!(assess_with_env(&home, &ws, Some("sk-real")).key_set);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn assess_ignores_empty_or_broken_config_key() {
        let (base, home, ws) = fixture("badkey");
        // Пустая строка-ключ — не ключ.
        write_user_config(&home, "api_key = \"\"\n");
        let state = assess_with_env(&home, &ws, None);
        assert!(state.config_exists);
        assert!(!state.key_set);
        // Битый TOML: файл конфига есть, но ключа из него не извлечь.
        write_user_config(&home, "api_key = [это не строка\n");
        let state = assess_with_env(&home, &ws, None);
        assert!(state.config_exists);
        assert!(!state.key_set);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn assess_reads_key_from_workspace_config() {
        let (base, home, ws) = fixture("wskey");
        let cfg = ws.join(".theseus/config.toml");
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        std::fs::write(&cfg, CONFIG_WITH_KEY).unwrap();
        let state = assess_with_env(&home, &ws, None);
        assert!(state.config_exists);
        assert!(state.key_set);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn assess_public_wrapper_matches_injected_env() {
        let (base, home, ws) = fixture("wrapper");
        let env_now = std::env::var(ENV_API_KEY).ok();
        assert_eq!(
            assess(&home, &ws),
            assess_with_env(&home, &ws, env_now.as_deref())
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn welcome_contains_hints_when_key_missing() {
        let state = OnboardingState {
            config_exists: true,
            key_set: false,
            workspace_ok: true,
            skills_found: 2,
        };
        let text = welcome_text(&state);
        assert!(text.contains("Добро пожаловать"));
        assert!(text.contains('❌'), "нет статуса ❌:\n{text}");
        assert!(text.contains("DEEPSEEK_API_KEY"), "нет подсказки про ключ:\n{text}");
        assert!(text.contains("Следующие шаги"));
        assert!(text.contains("1. "), "шаги должны быть пронумерованы:\n{text}");
    }

    #[test]
    fn welcome_ready_state_has_no_failures() {
        let state = OnboardingState {
            config_exists: true,
            key_set: true,
            workspace_ok: true,
            skills_found: 4,
        };
        let text = welcome_text(&state);
        assert!(!text.contains('❌'), "готовое состояние без ❌:\n{text}");
        assert!(text.contains("Всё готово"));
        assert!(text.contains("✅"));
        // В готовом состоянии показываем первый стартовый промпт.
        assert!(text.contains(suggested_starter_prompts()[0]));
    }

    #[test]
    fn welcome_shows_skills_count() {
        let state = OnboardingState {
            skills_found: 7,
            ..Default::default()
        };
        assert!(welcome_text(&state).contains("обнаружено 7"));
    }

    #[test]
    fn checklist_order_and_hint_contract() {
        let items = readiness_checklist(&OnboardingState::default());
        let names: Vec<&str> = items.iter().map(|it| it.item.as_str()).collect();
        assert_eq!(names, ["config", "api_key", "workspace", "skills"]);
        for it in &items {
            assert!(!it.ok);
            assert!(!it.hint.is_empty(), "у пункта {} нужна подсказка", it.item);
        }
        // Подсказки конкретны: называют файл, переменную окружения, каталог.
        assert!(items[0].hint.contains("config.toml"));
        assert!(items[1].hint.contains("DEEPSEEK_API_KEY"));
        assert!(items[2].hint.contains("каталог"));
        assert!(items[3].hint.contains("SKILL.md"));
    }

    #[test]
    fn checklist_all_ok_has_empty_hints() {
        let state = OnboardingState {
            config_exists: true,
            key_set: true,
            workspace_ok: true,
            skills_found: 1,
        };
        let items = readiness_checklist(&state);
        assert!(items.iter().all(|it| it.ok));
        assert!(items.iter().all(|it| it.hint.is_empty()));
    }

    #[test]
    fn is_ready_requires_everything() {
        assert!(!OnboardingState::default().is_ready());
        // Ноль скиллов — ещё не готовы, даже когда остальное ✅.
        let almost = OnboardingState {
            config_exists: true,
            key_set: true,
            workspace_ok: true,
            skills_found: 0,
        };
        assert!(!almost.is_ready());
        let ready = OnboardingState {
            skills_found: 1,
            ..almost
        };
        assert!(ready.is_ready());
    }

    #[test]
    fn first_run_true_then_false() {
        let (base, home, _ws) = fixture("firstrun");
        assert!(first_run_marker(&home).unwrap(), "первый вызов — первый запуск");
        let marker = home.join(MARKER_RELATIVE_PATH);
        assert!(marker.is_file());
        assert!(!std::fs::read_to_string(&marker).unwrap().is_empty());
        assert!(!first_run_marker(&home).unwrap(), "второй вызов — уже не первый");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn first_run_respects_preexisting_marker() {
        let (base, home, _ws) = fixture("premarker");
        let marker = home.join(MARKER_RELATIVE_PATH);
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, "я был здесь").unwrap();
        assert!(!first_run_marker(&home).unwrap());
        // Существующий маркер не перезаписывается.
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "я был здесь");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn first_run_creates_missing_home_dirs() {
        let base = tempdir("nohome");
        let home = base.join("deep").join("nested").join("home");
        assert!(first_run_marker(&home).unwrap());
        assert!(home.join(MARKER_RELATIVE_PATH).is_file());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn starter_prompts_are_non_empty_unique_and_ml_flavored() {
        let prompts = suggested_starter_prompts();
        assert!(
            (3..=4).contains(&prompts.len()),
            "ожидается 3–4 промпта, получено {}",
            prompts.len()
        );
        for (i, p) in prompts.iter().enumerate() {
            assert!(!p.trim().is_empty());
            assert!(!prompts[..i].contains(p), "дубликат промпта: {p}");
        }
        let ml_words = ["обучен", "метрик", "датасет", "чекпоинт", "модел"];
        assert!(
            prompts.iter().any(|p| ml_words.iter().any(|w| p.contains(w))),
            "промпты должны быть про ML: {prompts:?}"
        );
    }
}
