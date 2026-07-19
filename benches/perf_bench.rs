//! Микробенчмарки Theseus — criterion (канон из rust-performance).
//! Запуск: `cargo bench`
//!
//! Бенчмарки: simhash64+hamming (compact_v2), levenshtein (models),
//! est_tokens (chars/4), сборка системного промпта.

use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use theseus::compact_v2::{hamming, simhash64};

// ---------------------------------------------------------------------------
// simhash64
// ---------------------------------------------------------------------------

fn bench_simhash(c: &mut Criterion) {
    let sizes = [1_024, 10_240, 102_400];
    let mut group = c.benchmark_group("simhash64");

    for size in sizes {
        let text: String = (0..size / 20)
            .map(|i| format!("строка {i:05}: филлер контекста для симуляции реальной нагрузки\n"))
            .collect();
        group.bench_with_input(BenchmarkId::new("bytes", size), &size, |b, _| {
            b.iter(|| simhash64(std::hint::black_box(&text)));
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// hamming — расстояние Хэмминга между simhash-подписями
// ---------------------------------------------------------------------------

fn bench_hamming(c: &mut Criterion) {
    let pairs: Vec<(u64, u64)> = (0..10_000)
        .map(|i| (i as u64 * 0x517cc1b727220a95, i as u64 ^ 0xdeadbeefcafebabe))
        .collect();
    let mut group = c.benchmark_group("hamming");
    group.bench_function("10k_pairs", |b| {
        b.iter(|| {
            for &(a, b_val) in &pairs {
                std::hint::black_box(hamming(a, b_val));
            }
        })
    });
    group.finish();
}

fn bench_hamming_identical(c: &mut Criterion) {
    c.bench_function("hamming_identical", |b| {
        b.iter(|| hamming(std::hint::black_box(0xdeadbeef), std::hint::black_box(0xdeadbeef)))
    });
}

// ---------------------------------------------------------------------------
// levenshtein (models) — подсказка ближайшей модели при опечатке
// ---------------------------------------------------------------------------

fn bench_levenshtein(c: &mut Criterion) {
    let pairs: Vec<(&str, &str)> = vec![
        ("deepseek-v4-pro", "deepseek-v4-pro"),
        ("deepseek-v4-pro", "deepseak-v4-pro"),
        ("deepseek-v4-pro", "gpt-4"),
        ("qwen-max", "qwen2.5-72b-instruct"),
        ("claude-sonnet-4-20250514", "cloud-sonnet-4-20250514"),
    ];
    c.bench_function("levenshtein_model_ids", |b| {
        b.iter(|| {
            for (a, b_val) in &pairs {
                std::hint::black_box(theseus::models::levenshtein(a, b_val));
            }
        })
    });
}

// ---------------------------------------------------------------------------
// est_tokens (agent) — оценка заполненности контекста (chars/4+1)
// ---------------------------------------------------------------------------

fn bench_est_tokens(c: &mut Criterion) {
    use theseus::api::Message;
    let sizes = [10, 100, 500];
    let mut group = c.benchmark_group("est_tokens");
    for n in sizes {
        let messages: Vec<Message> = (0..n)
            .map(|i| {
                let content = format!("сообщение {i:03}: некоторый текст для оценки токенов в контексте");
                if i % 3 == 0 {
                    Message::user(content)
                } else if i % 3 == 1 {
                    Message::assistant(Some(content), None)
                } else {
                    Message::system(content)
                }
            })
            .collect();
        group.bench_with_input(BenchmarkId::new("messages", n), &n, |b, _| {
            b.iter(|| {
                let chars: usize = std::hint::black_box(&messages).iter().map(|m| {
                    m.content.as_deref().unwrap_or("").len()
                        + m.tool_calls.as_ref()
                            .map(|v| serde_json::to_string(v).unwrap_or_default().len())
                            .unwrap_or(0)
                }).sum();
                std::hint::black_box(chars / 4 + 1)
            })
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// build_system_prompt (prompts) — сборка системного промпта
// ---------------------------------------------------------------------------

fn bench_system_prompt(c: &mut Criterion) {
    use theseus::prompts::{EnvContext, PromptBuilder, SkillDigest};

    let skills: Vec<SkillDigest> = (0..20)
        .map(|i| SkillDigest::new(
            format!("skill-{i}"),
            format!("описание скилла номер {i} для тестового бенчмарка"),
        ))
        .collect();

    let env = EnvContext {
        os: "linux".into(),
        shell: "/bin/bash".into(),
        cwd: "/tmp/theseus_bench".into(),
        date: "2026-07-18".into(),
        git_branch: None,
    };

    c.bench_function("build_system_prompt_20skills", |b| {
        b.iter(|| {
            PromptBuilder::new()
                .base("You are a test agent.")
                .env(env.clone())
                .agents_md("", "# Правила\nТестовый AGENTS.md")
                .agents_md_limit(32 * 1024)
                .skills(&skills)
                .goal(Some("бенчмарк-цель".into()))
                .plan_mode(true)
                .build()
        })
    });
}

criterion_group!(
    benches,
    bench_simhash,
    bench_hamming,
    bench_hamming_identical,
    bench_levenshtein,
    bench_est_tokens,
    bench_system_prompt,
);
criterion_main!(benches);
