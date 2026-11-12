use std::{
    hint::black_box,
    path::PathBuf,
    time::{Duration, Instant},
};

use futures::future::join_all;
use phi_codemode::{
    CodemodeSandbox, CodemodeTool, ExecuteOptions, ExecutionOutcome, SandboxLimits, SandboxOptions,
    ToolDefinition,
};
use serde_json::{Value, json};

const DEFAULT_SAMPLES: usize = 20;
const WARMUP_RUNS: usize = 3;
const WAITING_SCRIPTS: usize = 16;
const TOOL_WAIT: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
enum Metering {
    Unmetered,
    Metered,
}

impl Metering {
    fn name(self) -> &'static str {
        match self {
            Self::Unmetered => "unmetered",
            Self::Metered => "metered",
        }
    }

    fn fuel(self) -> Option<u64> {
        match self {
            Self::Unmetered => None,
            Self::Metered => Some(u64::MAX),
        }
    }
}

struct Workload {
    name: &'static str,
    code: &'static str,
    expected: Value,
    concurrency: usize,
}

async fn run(sandbox: &CodemodeSandbox, workload: &Workload) -> Duration {
    let started = Instant::now();
    let executions = (0..workload.concurrency)
        .map(|_| sandbox.execute(workload.code, ExecuteOptions::default()));
    for result in join_all(executions).await {
        match result.outcome {
            ExecutionOutcome::Success { value, .. } => {
                assert_eq!(
                    value.as_ref(),
                    Some(&workload.expected),
                    "{}",
                    workload.name
                );
                black_box(value);
                black_box(result.output);
            }
            ExecutionOutcome::Failure(error) => panic!("{}: {error:?}", workload.name),
        }
    }
    started.elapsed()
}

async fn benchmark() {
    let mut samples = DEFAULT_SAMPLES;
    let mut filter = String::new();
    let mut cache_dir = None;
    let mut tool_wait = TOOL_WAIT;
    let mut modes = vec![Metering::Unmetered, Metering::Metered];
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--samples" => samples = arguments.next().expect("sample count").parse().unwrap(),
            "--filter" => filter = arguments.next().expect("workload filter"),
            "--cache-dir" => {
                cache_dir = Some(PathBuf::from(arguments.next().expect("cache directory")))
            }
            "--wait-ms" => {
                tool_wait = Duration::from_millis(
                    arguments.next().expect("wait duration").parse().unwrap(),
                );
            }
            "--mode" => {
                modes = vec![match arguments.next().as_deref() {
                    Some("unmetered") => Metering::Unmetered,
                    Some("metered") => Metering::Metered,
                    _ => panic!("mode must be unmetered or metered"),
                }];
            }
            "--bench" => (),
            _ => panic!("unknown argument: {argument}"),
        }
    }
    assert!(samples > 0, "sample count must be positive");
    let workloads = [
        Workload {
            name: "fresh_vm",
            code: "return 1",
            expected: json!(1),
            concurrency: 1,
        },
        Workload {
            name: "arithmetic",
            code: "let sum = 0; for (let i = 0; i < 100000; i++) sum += i; return sum;",
            expected: json!(4_999_950_000_u64),
            concurrency: 1,
        },
        Workload {
            name: "serial_100",
            code: "let value; for (let i = 0; i < 100; i++) value = await tools.echo(i); return value;",
            expected: json!(99),
            concurrency: 1,
        },
        Workload {
            name: "parallel_32",
            code: "return (await Promise.all(Array.from({length:32}, (_, i) => tools.echo(i)))).length;",
            expected: json!(32),
            concurrency: 1,
        },
        Workload {
            name: "escaped_256k",
            code: "const value = '\"\\\\\\n\\t'.repeat(65536); return (await tools.echo(value)).length;",
            expected: json!(262_144),
            concurrency: 1,
        },
        Workload {
            name: "output_256k",
            code: "text('\"\\\\\\n\\t'.repeat(65536)); return 1;",
            expected: json!(1),
            concurrency: 1,
        },
        Workload {
            name: "waiting_16",
            code: "await tools.wait(); return 1;",
            expected: json!(1),
            concurrency: WAITING_SCRIPTS,
        },
    ];
    assert!(
        workloads
            .iter()
            .any(|workload| workload.name.contains(&filter)),
        "unknown workload filter"
    );
    println!("mode,workload,samples,median_ms,p95_ms");
    for mode in modes {
        let tools = vec![
            CodemodeTool::new(
                ToolDefinition {
                    name: "echo".into(),
                    ..Default::default()
                },
                |args, _| async move { Ok(args) },
            ),
            CodemodeTool::new(
                ToolDefinition {
                    name: "wait".into(),
                    ..Default::default()
                },
                move |_, _| async move {
                    tokio::time::sleep(tool_wait).await;
                    Ok(None)
                },
            ),
        ];
        let started = Instant::now();
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            cache_dir: cache_dir.clone(),
            tools,
            limits: SandboxLimits {
                fuel: mode.fuel(),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
        eprintln!(
            "{} construction: {:.3} ms",
            mode.name(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        for workload in workloads
            .iter()
            .filter(|workload| workload.name.contains(&filter))
        {
            for _ in 0..WARMUP_RUNS {
                run(&sandbox, workload).await;
            }
            let mut timings = Vec::with_capacity(samples);
            for _ in 0..samples {
                timings.push(run(&sandbox, workload).await);
            }
            timings.sort_unstable();
            let median = timings[samples / 2].as_secs_f64() * 1000.0;
            let percentile = timings[(samples * 95).div_ceil(100) - 1].as_secs_f64() * 1000.0;
            println!(
                "{},{},{samples},{median:.3},{percentile:.3}",
                mode.name(),
                workload.name
            );
        }
        sandbox.close().await;
    }
}

fn main() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(benchmark());
}
