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
    /// потоковая задача (spawn_fn): результата — строка из замыкания, child нет
    pub threaded: bool,
}

pub struct BgRegistry {
    tasks: BTreeMap<u64, BgTask>,
    next: u64,
    /// разделяемый счётчик работающих задач (индикатор «фон: N» в TUI)
    counter: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

impl Default for BgRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BgRegistry {
    pub fn new() -> Self {
        BgRegistry { tasks: BTreeMap::new(), next: 0, counter: None }
    }

    /// Подключить разделяемый счётчик работающих задач (Controls.bg_running).
    pub fn set_counter(&mut self, counter: Arc<std::sync::atomic::AtomicUsize>) {
        self.counter = Some(counter);
    }

    fn counter_inc(&self) {
        if let Some(c) = &self.counter {
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
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

        self.counter_inc();
        self.tasks.insert(id, BgTask {
            id, command: command.to_string(), started: Instant::now(),
            out, child: Some(child), done, threaded: false,
        });
        Ok(id)
    }

    /// Фоновая ПОТОКОВАЯ задача (не процесс): замыкание производит строку-
    /// результат (фоновые субагенты и peer-вызовы, v0.6.6 — Тесей продолжает
    /// работу, не дожидаясь субагента). Процессного child нет: stop её не
    /// убивает — время жизни ограничивают собственные бюджеты/таймауты задачи.
    pub fn spawn_fn(&mut self, label: String, f: impl FnOnce() -> String + Send + 'static) -> u64 {
        let out = Arc::new(Mutex::new(String::new()));
        let done = Arc::new(Mutex::new(None));
        self.next += 1;
        let id = self.next;
        let out2 = out.clone();
        let done2 = done.clone();
        let counter2 = self.counter.clone();
        self.counter_inc();
        std::thread::spawn(move || {
            let res = f();
            *out2.lock().unwrap() = res;
            *done2.lock().unwrap() = Some(0);
            if let Some(c) = counter2 {
                let _ = c.fetch_update(std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |v| Some(v.saturating_sub(1)));
            }
        });
        self.tasks.insert(id, BgTask {
            id, command: label, started: Instant::now(),
            out, child: None, done, threaded: true,
        });
        id
    }

    pub fn output(&mut self, id: u64) -> String {
        if !self.tasks.contains_key(&id) {
            return format!("ERROR: задача {id} не найдена");
        }
        self.refresh(id);
        let t = &self.tasks[&id];
        let status = match *t.done.lock().unwrap() {
            Some(code) => format!("завершена (exit {code:?})"),
            // анти-flail (живой кейс 21.07: модель опросила task_output 7 раз
            // подряд за 5с и бросила ждать) — направляем на другую работу
            None => "выполняется — НЕ опрашивайте подряд: продолжайте другую \
                     работу и заберите результат позже".to_string(),
        };
        let out = t.out.lock().unwrap().clone();
        // хвост: для процессов — последние ~400 символов лога, для потоковых
        // (субагенты/пиры) — до ~2000: там результат целиком, он и есть payload
        let tail_chars = if t.threaded { 2000 } else { 400 };
        let tail = if out.len() > 4096 * 2 {
            out.chars().skip(out.chars().count() - tail_chars).collect::<String>()
        } else { out };
        format!("[bg {}] {} | {:.0}s | {}\n{}", id, t.command, t.started.elapsed().as_secs_f32(), status, tail)
    }

    /// Ленивое обновление статуса процессной задачи (try_wait) + декремент
    /// счётчика при обнаружении завершения. Потоковые помечают себя сами.
    fn refresh(&mut self, id: u64) {
        let counter = self.counter.clone();
        let Some(t) = self.tasks.get_mut(&id) else { return };
        if t.done.lock().unwrap().is_none() {
            if let Some(ch) = t.child.as_mut() {
                if let Ok(Some(status)) = ch.try_wait() {
                    *t.done.lock().unwrap() = status.code();
                    if let Some(c) = &counter {
                        let _ = c.fetch_update(std::sync::atomic::Ordering::Relaxed,
                            std::sync::atomic::Ordering::Relaxed,
                            |v| Some(v.saturating_sub(1)));
                    }
                    t.child = None;
                }
            }
        }
    }

    /// Завершена ли задача: Some(true/false) по флагу done (с ленивым
    /// обновлением для процессных), None — задача не найдена. Для swarm_wait.
    pub fn is_done(&mut self, id: u64) -> Option<bool> {
        self.refresh(id);
        self.tasks.get(&id).map(|t| t.done.lock().unwrap().is_some())
    }

    pub fn stop(&mut self, id: u64) -> String {
        let counter = self.counter.clone();
        let Some(t) = self.tasks.get_mut(&id) else {
            return format!("ERROR: задача {id} не найдена");
        };
        if t.threaded {
            return format!(
                "[bg {id}] потоковая задача (субагент/peer) не останавливается — \
                 у неё свой бюджет/таймаут; дождитесь завершения");
        }
        if let Some(ch) = t.child.as_mut() {
            let _ = ch.kill();
            let _ = ch.wait();
            *t.done.lock().unwrap() = Some(-9);
            t.child = None;
            if let Some(c) = &counter {
                let _ = c.fetch_update(std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |v| Some(v.saturating_sub(1)));
            }
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

    /// Счётчик работающих задач (индикатор «фон: N» в TUI): +1 на старте
    /// потоковой задачи, −1 на завершении; процессная — декремент при
    /// обнаружении выхода в output().
    #[test]
    fn counter_tracks_running_tasks() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut reg = BgRegistry::new();
        reg.set_counter(counter.clone());
        let id = reg.spawn_fn("subagent — тест".into(), || {
            std::thread::sleep(std::time::Duration::from_millis(80));
            "ok".to_string()
        });
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1,
            "после старта — одна работающая");
        for _ in 0..60 {
            if counter.load(std::sync::atomic::Ordering::Relaxed) == 0 { break; }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0,
            "после завершения — ноль");
        let _ = reg.output(id);
        // процессная задача: спавн + обнаружение завершения
        let id2 = reg.spawn("true", &std::env::temp_dir()).expect("spawn bash");
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);
        for _ in 0..60 {
            if counter.load(std::sync::atomic::Ordering::Relaxed) == 0 { break; }
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = reg.output(id2);
        }
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    /// Потоковые фоновые задачи (v0.6.6 — фоновые субагенты/пиры): замыкание
    /// производит результат, статус идёт «выполняется» → «завершена», stop
    /// честно отказывает (не процесс).
    #[test]
    fn spawn_fn_lifecycle() {
        let mut reg = BgRegistry::new();
        let id = reg.spawn_fn("subagent explore — тест".into(), || {
            std::thread::sleep(std::time::Duration::from_millis(80));
            "ответ субагента".to_string()
        });
        let running = reg.output(id);
        assert!(running.contains("subagent explore"), "{running}");
        // дождаться завершения потока
        for _ in 0..50 {
            if reg.output(id).contains("завершена") { break; }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let done = reg.output(id);
        assert!(done.contains("завершена"), "{done}");
        assert!(done.contains("ответ субагента"), "результат потерян: {done}");
        // stop на потоковой — честный отказ, а не «уже завершена»
        let stop = reg.stop(id);
        assert!(stop.contains("не останавливается"), "{stop}");
    }
}
