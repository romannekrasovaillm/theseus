//! Фоновые задачи (v0.3, урок всех трёх: bash is_background + task_output/task_stop).

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct BgTask {
    pub id: u64,
    pub command: String,
    pub started: Instant,
    pub out: Arc<Mutex<String>>,
    child: Option<Child>,
    pub done: Arc<Mutex<Option<i32>>>,
}

pub struct BgRegistry {
    tasks: BTreeMap<u64, BgTask>,
    next: u64,
}

impl Default for BgRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BgRegistry {
    pub fn new() -> Self {
        BgRegistry { tasks: BTreeMap::new(), next: 0 }
    }

    pub fn spawn(&mut self, command: &str, cwd: &PathBuf) -> Result<u64> {
        let mut child = Command::new("bash")
            .arg("-lc").arg(command)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let out = Arc::new(Mutex::new(String::new()));
        let done = Arc::new(Mutex::new(None));
        self.next += 1;
        let id = self.next;

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let out2 = out.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                out2.lock().unwrap().push_str(&line);
                out2.lock().unwrap().push('\n');
            }
        });
        let out3 = out.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                out3.lock().unwrap().push_str(&line);
                out3.lock().unwrap().push('\n');
            }
        });

        self.tasks.insert(id, BgTask {
            id, command: command.to_string(), started: Instant::now(),
            out, child: Some(child), done,
        });
        Ok(id)
    }

    pub fn output(&mut self, id: u64) -> String {
        let Some(t) = self.tasks.get_mut(&id) else {
            return format!("ERROR: задача {id} не найдена");
        };
        // ленивое обновление статуса
        if t.done.lock().unwrap().is_none() {
            if let Some(ch) = t.child.as_mut() {
                if let Ok(Some(status)) = ch.try_wait() {
                    *t.done.lock().unwrap() = status.code();
                    t.child = None;
                }
            }
        }
        let status = match *t.done.lock().unwrap() {
            Some(code) => format!("завершена (exit {code:?})"),
            None => "выполняется".to_string(),
        };
        let out = t.out.lock().unwrap().clone();
        let tail = if out.len() > 4096 {
            out.chars().skip(out.chars().count() - 400).collect::<String>()
        } else { out };
        format!("[bg {}] {} | {:.0}s | {}\n{}", id, t.command, t.started.elapsed().as_secs_f32(), status, tail)
    }

    pub fn stop(&mut self, id: u64) -> String {
        let Some(t) = self.tasks.get_mut(&id) else {
            return format!("ERROR: задача {id} не найдена");
        };
        if let Some(ch) = t.child.as_mut() {
            let _ = ch.kill();
            let _ = ch.wait();
            *t.done.lock().unwrap() = Some(-9);
            t.child = None;
            format!("[bg {id}] остановлена")
        } else {
            format!("[bg {id}] уже завершена")
        }
    }

    pub fn list(&mut self) -> String {
        if self.tasks.is_empty() { return "(нет фоновых задач)".into(); }
        let mut lines = vec![];
        // ключи копируем: output() ниже берёт &mut self, итератор по tasks держать нельзя
        let ids: Vec<u64> = self.tasks.keys().copied().collect();
        for id in ids {
            lines.push(self.output(id).lines().next().unwrap_or("").to_string());
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Регрессия (REVIEW_REPORT_V3 #1.1): id несуществующей задачи обязан
    /// интерполироваться в текст ошибки, а не оставаться «{id}» литералом.
    #[test]
    fn missing_task_id_is_interpolated() {
        let mut reg = BgRegistry::default();
        let out = reg.output(42);
        assert!(out.contains("42"), "id не подставлен: {out}");
        assert!(!out.contains("{id}"), "литерал-плейсхолдер остался: {out}");
        let stop = reg.stop(43);
        assert!(stop.contains("43"), "id не подставлен: {stop}");
        assert!(!stop.contains("{id}"), "литерал-плейсхолдер остался: {stop}");
    }
}
