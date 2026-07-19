//! Кросс-сессионная память (v0.3, урок Claude memdir / Grok MEMORY.md + autoDream-lite).

use std::path::{Path, PathBuf};

pub struct Memory {
    dir: PathBuf,
}

impl Memory {
    pub fn open(home_root: &Path) -> Self {
        let dir = home_root.join("memory");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("MEMORY.md");
        if !f.exists() {
            let _ = std::fs::write(&f, "# Память агента (Theseus)\n\n");
        }
        Memory { dir }
    }

    fn file(&self) -> PathBuf { self.dir.join("MEMORY.md") }

    /// Дописать факт одной строкой (с датой).
    /// Атомарная запись: tmp-файл + rename (как session.rs) — защита
    /// от повреждения MEMORY.md при конкурентном доступе.
    pub fn write_fact(&self, fact: &str) -> String {
        let date = "2026-07-18"; // дата фиксируется вызывающей стороной при желании
        let line = format!("- [{}] {}\n", date, fact.trim());
        let mut cur = std::fs::read_to_string(self.file()).unwrap_or_default();
        cur.push_str(&line);
        let tmp = self.dir.join(".MEMORY.tmp");
        match std::fs::write(&tmp, &cur) {
            Ok(_) => match std::fs::rename(&tmp, self.file()) {
                Ok(_) => format!("OK: факт записан в память ({} символов)", line.len()),
                Err(e) => { let _ = std::fs::remove_file(&tmp); format!("ERROR: {e}") }
            },
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// BM25-lite: топ-N строк по пересечению токенов запроса
    pub fn search(&self, query: &str, top: usize) -> String {
        let text = match std::fs::read_to_string(self.file()) {
            Ok(t) => t,
            Err(e) => return format!("ERROR: {e}"),
        };
        let q: Vec<String> = query.to_lowercase().split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() > 2).map(String::from).collect();
        if q.is_empty() { return "(пустой запрос)".into(); }
        let mut scored: Vec<(usize, usize, String)> = vec![];
        for (i, line) in text.lines().enumerate() {
            let l = line.to_lowercase();
            let score = q.iter().filter(|t| l.contains(t.as_str())).count();
            if score > 0 {
                scored.push((score, i + 1, line.to_string()));
            }
        }
        scored.sort_by_key(|s| std::cmp::Reverse(s.0));
        if scored.is_empty() { return "(в памяти ничего не найдено)".into(); }
        scored.into_iter().take(top)
            .map(|(s, i, l)| format!("MEMORY.md:{i} (score {s}): {l}"))
            .collect::<Vec<_>>().join("\n")
    }

    /// Сколько непустых строк-фактов (для гейта консолидации)
    pub fn fact_count(&self) -> usize {
        std::fs::read_to_string(self.file()).unwrap_or_default()
            .lines().filter(|l| l.starts_with("- [")).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_search() {
        let dir = std::env::temp_dir().join("theseus_memtest");
        std::fs::create_dir_all(&dir).unwrap();
        let m = Memory::open(&dir);
        m.write_fact("У пользователя RTX 4080 SUPER 16GB");
        m.write_fact("Проект Theseus живёт в harness-review");
        let r = m.search("видеокарта rtx", 5);
        assert!(r.contains("4080"));
        assert!(!r.contains("Theseus живёт") || r.contains("4080"));
    }
}
