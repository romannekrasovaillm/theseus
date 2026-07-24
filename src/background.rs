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
    child: Option<Arc<Mutex<Child>>>,
    pub done: Arc<Mutex<Option<i32>>>,
    /// потоковая задача (spawn_fn): результата — строка из замыкания, child нет
    pub threaded: bool,
}

/// Атомарная финализация задачи: выставить код завершения, если его ещё нет.
/// Возвращает true только ПЕРВОМУ финализатору (watcher/refresh/stop) —
/// именно он декрементит счётчик и помечает снимок (без двойного учёта).
fn finalize(done: &Arc<Mutex<Option<i32>>>, code: Option<i32>) -> bool {
    let mut d = done.lock().unwrap();
    if d.is_some() {
        return false;
    }
    *d = code;
    true
}

/// Снимок одной фоновой задачи для TUI (живая панель «фон» и уведомления
/// о завершении, v0.7): id, короткий тип, метка, момент старта, флаг done.
#[derive(Debug, Clone)]
pub struct BgTaskInfo {
    /// Идентификатор задачи (из BgRegistry).
    pub id: u64,
    /// Короткий тип для панели: «explore», «peer kimi», «bash».
    pub kind: String,
    /// Полная метка (команда или «subagent X — промпт»).
    pub label: String,
    /// Момент запуска (для таймеров панели).
    pub started: Instant,
    /// Завершена ли задача.
    pub done: bool,
}

/// Короткий тип задачи из метки: «subagent explore — …» → «explore»,
/// «peer kimi — …» → «peer kimi», прочее (bash-команда) → «bash».
/// Чистая функция — для тестов.
pub fn short_kind(label: &str) -> String {
    if let Some(rest) = label.strip_prefix("subagent ") {
        return rest.split([' ', '—']).next().unwrap_or("subagent").to_string();
    }
    if let Some(rest) = label.strip_prefix("peer ") {
        let name = rest.split([' ', '—']).next().unwrap_or("?");
        return format!("peer {name}");
    }
    "bash".to_string()
}

pub struct BgRegistry {
    tasks: BTreeMap<u64, BgTask>,
    next: u64,
    /// разделяемый счётчик работающих задач (индикатор «фон: N» в TUI)
    counter: Option<Arc<std::sync::atomic::AtomicUsize>>,
    /// разделяемый снимок задач для живой панели TUI и уведомлений
    snapshot: Option<Arc<Mutex<Vec<BgTaskInfo>>>>,
}

impl Default for BgRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BgRegistry {
    pub fn new() -> Self {
        BgRegistry { tasks: BTreeMap::new(), next: 0, counter: None, snapshot: None }
    }

    /// Подключить разделяемый счётчик работающих задач (Controls.bg_running).
    pub fn set_counter(&mut self, counter: Arc<std::sync::atomic::AtomicUsize>) {
        self.counter = Some(counter);
    }

    /// Подключить разделяемый снимок задач (Controls.bg_snapshot) — живая
    /// панель «фон» и уведомления о завершении в TUI.
    pub fn set_snapshot(&mut self, snapshot: Arc<Mutex<Vec<BgTaskInfo>>>) {
        self.snapshot = Some(snapshot);
    }

    fn counter_inc(&self) {
        if let Some(c) = &self.counter {
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Добавить задачу в снимок (вызывается при спавне).
    fn snap_push(&self, info: BgTaskInfo) {
        if let Some(s) = &self.snapshot {
            s.lock().unwrap().push(info);
        }
    }

    /// Пометить задачу завершённой в снимке (статические точки завершения).
    fn snap_done(&self, id: u64) {
        if let Some(s) = &self.snapshot {
            if let Some(info) = s.lock().unwrap().iter_mut().find(|i| i.id == id) {
                info.done = true;
            }
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

        // watcher: сам помечает завершение процесса (v0.7 — иначе счётчик и
        // снимок ждали, пока агент спросит task_output, и уведомления в TUI
        // не приходили на процессных задачах). Опрос try_wait короткими
        // захватами мьютекса — stop() может вклиниться и убить процесс.
        let child = Arc::new(Mutex::new(child));
        {
            let child2 = child.clone();
            let done2 = done.clone();
            let counter2 = self.counter.clone();
            let snapshot2 = self.snapshot.clone();
            std::thread::spawn(move || {
                loop {
                    let status = {
                        let mut ch = child2.lock().unwrap();
                        ch.try_wait()
                    };
                    match status {
                        Ok(Some(st)) => {
                            if finalize(&done2, st.code()) {
                                if let Some(s) = &snapshot2 {
                                    if let Some(info) = s.lock().unwrap().iter_mut().find(|i| i.id == id) {
                                        info.done = true;
                                    }
                                }
                                if let Some(c) = &counter2 {
                                    let _ = c.fetch_update(std::sync::atomic::Ordering::Relaxed,
                                        std::sync::atomic::Ordering::Relaxed,
                                        |v| Some(v.saturating_sub(1)));
                                }
                            }
                            break;
                        }
                        Ok(None) => std::thread::sleep(std::time::Duration::from_millis(300)),
                        Err(_) => break,
                    }
                }
            });
        }

        self.counter_inc();
        self.tasks.insert(id, BgTask {
            id, command: command.to_string(), started: Instant::now(),
            out, child: Some(child), done, threaded: false,
        });
        self.snap_push(BgTaskInfo {
            id, kind: "bash".to_string(), label: command.to_string(),
            started: Instant::now(), done: false,
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
        let snapshot2 = self.snapshot.clone();
        self.counter_inc();
        std::thread::spawn(move || {
            let res = f();
            *out2.lock().unwrap() = res;
            *done2.lock().unwrap() = Some(0);
            if let Some(s) = &snapshot2 {
                if let Some(info) = s.lock().unwrap().iter_mut().find(|i| i.id == id) {
                    info.done = true;
                }
            }
            if let Some(c) = counter2 {
                let _ = c.fetch_update(std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |v| Some(v.saturating_sub(1)));
            }
        });
        self.snap_push(BgTaskInfo {
            id, kind: short_kind(&label), label: label.clone(),
            started: Instant::now(), done: false,
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
    /// Финализация атомарна: гонку с watcher'ом выигрывает первый (v0.7).
    fn refresh(&mut self, id: u64) {
        let counter = self.counter.clone();
        let mut finalized = false;
        if let Some(t) = self.tasks.get_mut(&id) {
            if let Some(ch) = t.child.as_mut() {
                let status = ch.lock().unwrap().try_wait();
                if let Ok(Some(st)) = status {
                    if finalize(&t.done, st.code()) {
                        t.child = None;
                        finalized = true;
                    }
                }
            }
        }
        if finalized {
            if let Some(c) = &counter {
                let _ = c.fetch_update(std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |v| Some(v.saturating_sub(1)));
            }
            self.snap_done(id);
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
        let mut killed = false;
        if let Some(ch) = t.child.as_mut() {
            {
                let mut guard = ch.lock().unwrap();
                let _ = guard.kill();
                let _ = guard.wait();
            }
            if finalize(&t.done, Some(-9)) {
                t.child = None;
                killed = true;
            }
        }
        if killed {
            if let Some(c) = &counter {
                let _ = c.fetch_update(std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |v| Some(v.saturating_sub(1)));
            }
            self.snap_done(id);
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

    /// Watcher процессных задач (v0.7): завершение помечается в снимке и
    /// счётчике БЕЗ какого-либо опроса output()/is_done — иначе уведомления
    /// TUI не приходили на bash-задачах (живой кейс: sleep в фоне «висел»).
    #[test]
    fn process_task_finalizes_without_polling() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let snap = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut reg = BgRegistry::new();
        reg.set_counter(counter.clone());
        reg.set_snapshot(snap.clone());
        reg.spawn("true", &std::env::temp_dir()).expect("spawn");
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);
        // никаких output()/is_done — только ждём watcher
        for _ in 0..80 {
            let done = snap.lock().unwrap().first().map(|i| i.done).unwrap_or(false);
            if done { break; }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(snap.lock().unwrap()[0].done, "watcher обязан пометить done сам");
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0,
            "и декрементить счётчик без опроса");
    }

    /// Снимок задач для живой панели TUI (v0.7): push при спавне с коротким
    /// типом, пометка done по завершении потоковой задачи.
    #[test]
    fn snapshot_tracks_spawn_and_completion() {
        let snap = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut reg = BgRegistry::new();
        reg.set_snapshot(snap.clone());
        let id = reg.spawn_fn("subagent explore — что-то".into(), || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            "ok".to_string()
        });
        {
            let items = snap.lock().unwrap();
            assert_eq!(items.len(), 1, "одна задача в снимке");
            assert_eq!(items[0].id, id);
            assert_eq!(items[0].kind, "explore");
            assert!(!items[0].done, "на старте работает");
        }
        for _ in 0..60 {
            if snap.lock().unwrap()[0].done { break; }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(snap.lock().unwrap()[0].done, "после завершения — done");
    }

    /// Короткий тип из метки (панель в шапке): subagent/peer/bash.
    #[test]
    fn short_kind_maps_labels() {
        assert_eq!(short_kind("subagent explore — что делает x"), "explore");
        assert_eq!(short_kind("subagent test_runner — прогон"), "test_runner");
        assert_eq!(short_kind("peer kimi — обзор"), "peer kimi");
        assert_eq!(short_kind("cargo test"), "bash");
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
