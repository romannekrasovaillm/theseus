//! Скиллы (v0.3, урок всех трёх харнессов): discovery SKILL.md + загрузка в контекст по запросу.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct SkillSpec {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Сканирование каталогов: `<dir>/<name>/SKILL.md`, `<dir>/<name>.md`, а также
/// категории глубже (`<dir>/<категория>/<name>/SKILL.md`) — до 3 уровней
/// (структура библиотеки скиллов: категория → скилл). Дубликаты имён — первая
/// находка побеждает (в библиотеке есть осознанные версии-дубли).
pub fn discover(dirs: &[PathBuf]) -> Vec<SkillSpec> {
    let mut out = vec![];
    for dir in dirs {
        scan_dir(dir, 0, &mut out);
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert(s.name.clone()));
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn scan_dir(dir: &Path, depth: usize, out: &mut Vec<SkillSpec>) {
    if depth > 2 { return; }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            let candidate = p.join("SKILL.md");
            if candidate.exists() {
                if let Ok(text) = std::fs::read_to_string(&candidate) {
                    if let Some(spec) = parse_frontmatter(&text, &candidate) {
                        out.push(spec);
                    }
                }
            } else {
                scan_dir(&p, depth + 1, out);
            }
        } else if depth == 0 && p.extension().map(|x| x == "md").unwrap_or(false) {
            if let Ok(text) = std::fs::read_to_string(&p) {
                if let Some(spec) = parse_frontmatter(&text, &p) {
                    out.push(spec);
                }
            }
        }
    }
}

/// Поиск скиллов по имени и описанию (прогрессивное раскрытие: в промпте
/// дайджест первых 50, остальные находятся поиском — как у лидеров).
pub fn search<'a>(skills: &'a [SkillSpec], query: &str, limit: usize) -> Vec<&'a SkillSpec> {
    let q = query.trim().to_lowercase();
    // пустой запрос матчил бы всё (contains("") == true) — отсекаем явно
    if q.is_empty() { return Vec::new(); }
    let words: Vec<&str> = q.split_whitespace().collect();
    let mut scored: Vec<(u32, &SkillSpec)> = skills.iter().filter_map(|s| {
        let name = s.name.to_lowercase();
        let desc = s.description.to_lowercase();
        let score = if name == q { 100 }
        else if name.contains(&q) { 60 }
        else if desc.contains(&q) { 20 }
        else {
            let hits = words.iter().filter(|w| name.contains(**w) || desc.contains(**w)).count() as u32;
            if hits == 0 { return None; }
            5 * hits
        };
        Some((score, s))
    }).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.truncate(limit);
    scored.into_iter().map(|(_, s)| s).collect()
}

/// Минимальный YAML-frontmatter: ---\nname: X\ndescription: Y\n---
fn parse_frontmatter(text: &str, path: &Path) -> Option<SkillSpec> {
    let mut lines = text.lines();
    if lines.next()? != "---" { return None; }
    let mut name = None;
    let mut desc = String::new();
    let mut in_desc = false;
    for line in lines {
        if line == "---" { break; }
        if let Some(rest) = line.strip_prefix("name:") {
            name = Some(rest.trim().trim_matches('"').to_string());
            in_desc = false;
        } else if let Some(rest) = line.strip_prefix("description:") {
            let v = rest.trim().trim_start_matches('>').trim().trim_matches('"');
            desc = v.to_string();
            in_desc = true;
        } else if in_desc && (line.starts_with(' ') || line.starts_with('-')) {
            if !desc.is_empty() { desc.push(' '); }
            desc.push_str(line.trim());
        }
    }
    let name = name.or_else(|| {
        path.parent()?.file_name().map(|n| n.to_string_lossy().to_string())
            .or_else(|| path.file_stem().map(|n| n.to_string_lossy().to_string()))
    })?;
    Some(SkillSpec { name, description: desc, path: path.to_path_buf() })
}

pub fn load_body(spec: &SkillSpec) -> std::io::Result<String> {
    std::fs::read_to_string(&spec.path)
}

/// Строка для системного промпта: список доступных скиллов
pub fn surface_line(skills: &[SkillSpec]) -> Option<String> {
    if skills.is_empty() { return None; }
    let mut s = String::from("Available skills (call the `skill` tool with a name to load instructions):");
    for sk in skills.iter().take(50) {
        let d: String = sk.description.chars().take(80).collect();
        s.push_str(&format!("\n- {}: {}", sk.name, d));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_parse() {
        let dir = std::env::temp_dir().join("theseus_skilltest");
        let sub = dir.join("demo-skill");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("SKILL.md"),
            "---\nname: demo\ndescription: тестовый скилл\n---\n# Body\n").unwrap();
        let skills = discover(&[dir]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "demo");
        assert!(skills[0].description.contains("тестовый"));
    }

    /// Рекурсивная разведка: категория → скилл (структура библиотеки 0710_v1).
    #[test]
    fn discover_recurses_into_categories() {
        let dir = std::env::temp_dir().join("theseus_skillnest");
        let nested = dir.join("KAT").join("kat-tui-00");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("SKILL.md"),
            "---\nname: kat-tui-00\ndescription: DAG агентного RL\n---\n# Body\n").unwrap();
        let flat = dir.join("flat-skill");
        std::fs::create_dir_all(&flat).unwrap();
        std::fs::write(flat.join("SKILL.md"),
            "---\nname: flat\ndescription: плоский скилл\n---\n# Body\n").unwrap();
        let skills = discover(std::slice::from_ref(&dir));
        assert_eq!(skills.len(), 2, "нашлись оба: {skills:?}");
        assert!(skills.iter().any(|s| s.name == "kat-tui-00"));
        assert!(skills.iter().any(|s| s.name == "flat"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Дубликаты имён: первая находка побеждает.
    #[test]
    fn discover_dedupes_by_name() {
        let dir = std::env::temp_dir().join("theseus_skilldup");
        for variant in ["a", "b"] {
            let sub = dir.join(variant);
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("SKILL.md"),
                "---\nname: same\ndescription: вариант {variant}\n---\n# Body\n").unwrap();
        }
        let skills = discover(std::slice::from_ref(&dir));
        assert_eq!(skills.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_ranks_name_over_description() {
        let mk = |name: &str, desc: &str| SkillSpec {
            name: name.into(), description: desc.into(), path: PathBuf::from("/tmp/SKILL.md"),
        };
        let skills = vec![
            mk("grpo-training", "обучение моделей"),
            mk("other", "grpo метод в деталях"),
            mk("zzz-grpo", "что-то про grpo"),
        ];
        let hits = search(&skills, "grpo", 5);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].name, "grpo-training", "префикс имени выше: {hits:?}");
        // пустой запрос ничего не находит
        assert!(search(&skills, "", 5).is_empty());
        // слова запроса складываются
        let hits2 = search(&skills, "обучение моделей", 5);
        assert!(!hits2.is_empty());
    }
}
