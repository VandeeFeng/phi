#![doc = include_str!("../README.md")]

mod declarations;
mod runtime;
mod sandbox;
mod source;
mod types;

pub use declarations::{
    DEFAULT_SCHEMA_MAX_CHARS, MCP_TYPESCRIPT_PREAMBLE, render_declarations,
    render_tool_output_type, render_tool_signature, schema_to_type, to_codemode_identifier,
};
pub use sandbox::CodemodeSandbox;
pub use source::{
    CODEMODE_OPTIONS_PREFIX, CODEMODE_SOURCE_GRAMMAR, ParsedSource, SourceError, SourceOptions,
    parse_codemode_source,
};
pub use tokio_util::sync::CancellationToken;
pub use types::{
    ArgumentMode, CallRecord, CallStatus, CodemodeError, CodemodeTool, ErrorKind, ExecuteOptions,
    ExecutionOutcome, ExecutionResult, ExecutionTimeout, MAX_OUTPUT_CHARS, MAX_OUTPUT_ITEMS,
    MAX_STORE_TOTAL_CHARS, MAX_STORE_VALUE_CHARS, OutputItem, SandboxBuildError, SandboxLimits,
    SandboxOptions, SandboxSurface, StoreWrites, ToolContext, ToolDefinition, ToolResult,
};

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::{Value, json};
    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    #[test]
    fn sandbox_persists_compilation_across_processes() {
        const CACHE_ENV: &str = "PHI_CODEMODE_TEST_CACHE";
        const METERED_ENV: &str = "PHI_CODEMODE_TEST_METERED";
        if let Some(cache_dir) = std::env::var_os(CACHE_ENV) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let sandbox = CodemodeSandbox::new(SandboxOptions {
                    cache_dir: Some(cache_dir.into()),
                    limits: SandboxLimits {
                        fuel: (std::env::var(METERED_ENV).unwrap() == "metered")
                            .then_some(100_000_000),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .await
                .unwrap();
                let result = sandbox
                    .execute("return 2;", ExecuteOptions::default())
                    .await;
                assert_eq!(value(&result), Some(&json!(2)));
                sandbox.close().await;
            });
            return;
        }
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let cache_dir = directory.path().join("cache");
        let run = |mode: &str| {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::sandbox_persists_compilation_across_processes",
                ])
                .env(CACHE_ENV, &cache_dir)
                .env(METERED_ENV, mode)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        let artifacts = || {
            std::fs::read_dir(cache_dir.join("modules"))
                .unwrap()
                .flat_map(|entry| std::fs::read_dir(entry.unwrap().path()).unwrap())
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.is_file() && path.extension().is_none())
                .collect::<Vec<_>>()
        };
        run("unmetered");
        let paths = artifacts();
        assert_eq!(paths.len(), 1);
        let artifact = &paths[0];
        let modified = std::fs::metadata(artifact).unwrap().modified().unwrap();
        run("unmetered");
        assert_eq!(
            std::fs::metadata(artifact).unwrap().modified().unwrap(),
            modified
        );
        run("metered");
        assert_eq!(artifacts().len(), 2);
        const INVALID_CACHE: &[u8] = b"invalid cache data";
        std::fs::write(artifact, INVALID_CACHE).unwrap();
        run("unmetered");
        assert_ne!(std::fs::read(artifact).unwrap(), INVALID_CACHE);
        let repaired = std::fs::metadata(artifact).unwrap().modified().unwrap();
        run("unmetered");
        assert_eq!(
            std::fs::metadata(artifact).unwrap().modified().unwrap(),
            repaired
        );
    }

    #[tokio::test]
    async fn unavailable_cache_does_not_prevent_execution() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let cache_dir = directory.path().join("not-a-directory");
        std::fs::write(&cache_dir, b"not a cache").unwrap();
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            cache_dir: Some(cache_dir.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
        let result = sandbox
            .execute("return 2;", ExecuteOptions::default())
            .await;
        assert_eq!(value(&result), Some(&json!(2)));
        sandbox.close().await;
        assert_eq!(std::fs::read(cache_dir).unwrap(), b"not a cache");
    }

    #[tokio::test]
    async fn deletion_diff_is_bounded_without_store_growth() {
        let sandbox = CodemodeSandbox::new(SandboxOptions::default())
            .await
            .unwrap();
        let result = sandbox.execute(
            "for (let index = 0; index < 60000; index++) store(String(index), undefined); return 1;",
            ExecuteOptions::default(),
        ).await;
        assert!(
            matches!(result.outcome, ExecutionOutcome::Failure(_)),
            "{:?}",
            result.outcome
        );
        sandbox.close().await;
    }

    fn tool(name: &str) -> CodemodeTool {
        let owned = name.to_owned();
        CodemodeTool::new(
            ToolDefinition {
                name: name.into(),
                ..Default::default()
            },
            move |args, context| {
                let name = owned.clone();
                async move {
                    let args = args.unwrap_or(Value::Null);
                    match name.as_str() {
                        "fail" => Err("tool failed".into()),
                        "panic" => panic!("mock panic"),
                        "wait" => {
                            context.cancellation.cancelled().await;
                            Err("cancelled".into())
                        }
                        _ => {
                            tokio::time::sleep(Duration::from_millis(
                                args["ms"].as_u64().unwrap_or(0),
                            ))
                            .await;
                            Ok(args.get("value").cloned())
                        }
                    }
                }
            },
        )
    }

    async fn sandbox() -> CodemodeSandbox {
        CodemodeSandbox::new(SandboxOptions {
            tools: ["echo", "fail", "panic", "wait"]
                .into_iter()
                .map(tool)
                .collect(),
            ..Default::default()
        })
        .await
        .unwrap()
    }

    fn value(result: &ExecutionResult) -> Option<&Value> {
        match &result.outcome {
            ExecutionOutcome::Success { value, .. } => value.as_ref(),
            ExecutionOutcome::Failure(error) => panic!("unexpected failure: {error:?}"),
        }
    }

    fn error(result: &ExecutionResult) -> &CodemodeError {
        match &result.outcome {
            ExecutionOutcome::Failure(error) => error,
            outcome => panic!("unexpected success: {outcome:?}"),
        }
    }

    #[tokio::test]
    async fn async_tools_and_errors() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let parallel = CodemodeTool::new(
            ToolDefinition {
                name: "echo".into(),
                ..Default::default()
            },
            move |args, _| {
                let barrier = barrier.clone();
                async move {
                    barrier.wait().await;
                    Ok(args.and_then(|args| args.get("value").cloned()))
                }
            },
        );
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            tools: vec![parallel, tool("fail"), tool("panic")],
            timeout: Some(Duration::from_secs(2)),
            ..Default::default()
        })
        .await
        .unwrap();
        let result = sandbox
            .execute(
                r#"
            text('start');
            const values = await Promise.all([tools.echo({value:1}), tools.echo({value:2})]);
            try { await tools.fail({}); } catch(error) { values.push(error.message); }
            try { await tools.panic({}); } catch(error) { values.push(error.message); }
            console.log('done');
            return values;
        "#,
                ExecuteOptions::default(),
            )
            .await;
        assert_eq!(
            value(&result),
            Some(&json!([1, 2, "tool failed", "tool handler panicked"]))
        );
        assert_eq!(
            result.output,
            vec![
                OutputItem::Text {
                    text: "start".into(),
                    console: false
                },
                OutputItem::Text {
                    text: "done".into(),
                    console: true
                },
            ]
        );
        assert_eq!(
            result
                .calls
                .iter()
                .map(|call| (call.name.as_str(), call.status))
                .collect::<Vec<_>>(),
            [
                ("echo", CallStatus::Ok),
                ("echo", CallStatus::Ok),
                ("fail", CallStatus::Error),
                ("panic", CallStatus::Error)
            ]
        );
        sandbox.close().await;
    }

    #[tokio::test]
    async fn script_error_scope() {
        let sandbox = sandbox().await;
        let syntax = sandbox
            .execute("const = ;", ExecuteOptions::default())
            .await;
        assert_eq!(error(&syntax).kind, ErrorKind::Script);
        assert_eq!(error(&syntax).name.as_deref(), Some("SyntaxError"));
        let thrown = sandbox
            .execute(
                "text('before');\nstore('runs', 9); throw new Error('failed');",
                ExecuteOptions::default(),
            )
            .await;
        assert_eq!(error(&thrown).message, "failed");
        assert_eq!(thrown.output.len(), 1);
        assert!(
            error(&thrown)
                .stack
                .as_ref()
                .unwrap()
                .contains("codemode.js:2")
        );
        let serialization = sandbox
            .execute("store('runs', 9); return 1n;", ExecuteOptions::default())
            .await;
        assert_eq!(error(&serialization).kind, ErrorKind::Script);
    }

    #[tokio::test]
    async fn timeout_keeps_partial_output() {
        let sandbox = sandbox().await;
        for code in [
            "text('before'); while(true) {}",
            "text('before'); while(true) await null",
            "text('before'); await tools.wait({})",
        ] {
            let result = sandbox
                .execute(
                    code,
                    ExecuteOptions {
                        timeout: ExecutionTimeout::After(Duration::from_millis(100)),
                        ..Default::default()
                    },
                )
                .await;
            assert_eq!(error(&result).kind, ErrorKind::Timeout, "{result:?}");
            assert_eq!(result.output.len(), 1, "{code}");
        }
    }

    #[tokio::test]
    async fn stalled_promises_fail() {
        let sandbox = sandbox().await;
        let result = sandbox
            .execute("await new Promise(() => {})", ExecuteOptions::default())
            .await;
        assert_eq!(error(&result).kind, ErrorKind::Script);
        assert!(error(&result).message.contains("can never settle"));
    }

    #[tokio::test]
    async fn caught_exit_still_finishes() {
        let sandbox = sandbox().await;
        let result = sandbox
            .execute(
                "text('before'); store('key', 1); try { exit(); } catch(error) { while(true) {} }",
                ExecuteOptions {
                    timeout: ExecutionTimeout::After(Duration::from_millis(100)),
                    ..Default::default()
                },
            )
            .await;
        let ExecutionOutcome::Success {
            value,
            store_writes,
        } = &result.outcome
        else {
            panic!("{result:?}");
        };
        assert_eq!(*value, None);
        assert_eq!(store_writes.set, BTreeMap::from([("key".into(), json!(1))]));
        assert_eq!(result.output.len(), 1);
    }

    #[tokio::test]
    async fn globals_and_state_isolation() {
        let global = CodemodeTool::new(
            ToolDefinition {
                name: "models.list".into(),
                ..Default::default()
            },
            |args, _| async { Ok(args) },
        )
        .with_arguments(ArgumentMode::Spread);
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            globals: vec![global],
            ..Default::default()
        })
        .await
        .unwrap();
        let result = sandbox.execute(r#"
            globalThis.sentinel = 1;
            store('sentinel', 1);
            return {args: await models.list('a', 1), absent: ['fetch','require','process','setTimeout','WebAssembly'].map(name => typeof globalThis[name])};
        "#, ExecuteOptions::default()).await;
        assert_eq!(value(&result).unwrap()["args"], json!(["a", 1]));
        assert_eq!(
            value(&result).unwrap()["absent"],
            json!([
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                "undefined"
            ])
        );
        assert!(result.calls.is_empty());
        let fresh = sandbox
            .execute(
                "return [typeof sentinel, typeof load('sentinel')]",
                ExecuteOptions::default(),
            )
            .await;
        assert_eq!(value(&fresh), Some(&json!(["undefined", "undefined"])));
    }

    #[tokio::test]
    async fn host_only_surface_hides_codemode_and_nondeterminism() {
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            surface: SandboxSurface::HostOnly,
            globals: vec![CodemodeTool::new(
                ToolDefinition {
                    name: "echo".into(),
                    ..Default::default()
                },
                |args, _| async { Ok(args) },
            )],
            ..Default::default()
        })
        .await
        .unwrap();
        let result = sandbox.execute(r#"
            const absent = ['tools', 'console', 'text', 'image', 'exit', 'store', 'load', 'ALL_TOOLS', 'fetch', 'require', 'process'].map(name => typeof globalThis[name]);
            const blocked = [];
            for (const attempt of [() => Date.now(), () => Math.random(), () => new Date(), () => Date(), () => new (new Date(0).constructor)(), () => new (Date.prototype.constructor)()]) {
                try { attempt(); blocked.push(false); } catch (error) { blocked.push(error instanceof TypeError); }
            }
            return { absent, blocked, epoch: new Date(0).toISOString(), echo: await echo(7), parameters: [typeof tools, typeof console] };
        "#, ExecuteOptions::default()).await;
        let result = value(&result).unwrap();
        assert_eq!(result["absent"], json!(vec!["undefined"; 11]));
        assert_eq!(result["parameters"], json!(["undefined", "undefined"]));
        assert_eq!(result["blocked"], json!(vec![true; 6]));
        assert_eq!(result["epoch"], "1970-01-01T00:00:00.000Z");
        assert_eq!(result["echo"], 7);
        let stored = sandbox
            .execute(
                "return 1;",
                ExecuteOptions {
                    store: BTreeMap::from([("secret".into(), json!(1))]),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(error(&stored).kind, ErrorKind::Sandbox);
        sandbox.close().await;
        assert!(matches!(
            CodemodeSandbox::new(SandboxOptions {
                surface: SandboxSurface::HostOnly,
                tools: vec![tool("echo")],
                ..Default::default()
            })
            .await,
            Err(SandboxBuildError::InvalidOptions(_))
        ));
        let ordinary = self::sandbox().await;
        let result = ordinary
            .execute(
                "return [typeof tools, typeof store, typeof Date.now(), typeof Math.random()];",
                ExecuteOptions::default(),
            )
            .await;
        assert_eq!(
            value(&result),
            Some(&json!(["object", "function", "number", "number"]))
        );
        ordinary.close().await;
    }

    #[tokio::test]
    async fn image_output_contract() {
        let sandbox = sandbox().await;
        let result = sandbox
            .execute(
                r#"
            const images = [
                'data:image/jpeg;base64,iVBORw0K\r\nGgo=',
                {image_url: 'data:image/jpeg;base64,/9j/4A=='},
                {type: 'image', data: 'R0lGODlh', mimeType: 'image/png'},
                'data:image/png;base64,UklGRgAAAABXRUJQ',
            ];
            for (const input of images) image(input);
            const rejected = [];
            for (const input of [
                '', 'https://example.com/image.png', {image_url: 1},
                {type: 'text', text: 'not an image'},
                'data:image/png;base64,AA=A', 'data:image/png;base64,QUJD',
            ]) {
                try { image(input); rejected.push(false); }
                catch (error) { rejected.push(error instanceof TypeError); }
            }
            return rejected;
        "#,
                ExecuteOptions::default(),
            )
            .await;
        assert_eq!(value(&result), Some(&json!(vec![true; 6])));
        let expected = [
            ("image/png", "iVBORw0KGgo="),
            ("image/jpeg", "/9j/4A=="),
            ("image/gif", "R0lGODlh"),
            ("image/webp", "UklGRgAAAABXRUJQ"),
        ];
        assert_eq!(result.output.len(), expected.len());
        for (output, (expected_mime, expected_data)) in result.output.iter().zip(expected) {
            let OutputItem::Image { data, mime_type } = output else {
                panic!("unexpected output: {output:?}");
            };
            assert_eq!(mime_type, expected_mime);
            assert_eq!(data, expected_data);
        }
    }

    #[tokio::test]
    async fn memory_and_stack_limits() {
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            limits: SandboxLimits {
                heap_bytes: 4 * 1024 * 1024,
                memory_bytes: 8 * 1024 * 1024,
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
        let heap = sandbox.execute("try { const a=[]; while(true) a.push(new Array(10000).fill(1)); } catch(error) { return error.message; }", ExecuteOptions::default()).await;
        assert!(
            value(&heap)
                .unwrap()
                .as_str()
                .unwrap()
                .contains("out of memory")
        );
        let stack = sandbox
            .execute(
                "try { (function recurse(){recurse()})(); } catch(error) { return error.name; }",
                ExecuteOptions::default(),
            )
            .await;
        assert_eq!(value(&stack), Some(&json!("RangeError")));
        let linear = sandbox
            .execute(
                "try { new ArrayBuffer(16*1024*1024); } catch(error) { return error.message; }",
                ExecuteOptions::default(),
            )
            .await;
        assert!(
            value(&linear)
                .unwrap()
                .as_str()
                .unwrap()
                .contains("out of memory")
        );
    }

    #[tokio::test]
    async fn output_and_call_budgets() {
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            tools: vec![tool("echo")],
            limits: SandboxLimits {
                max_output_chars: 10,
                max_output_items: 2,
                max_calls: 2,
                max_pending_calls: 2,
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
        let output = sandbox
            .execute(
                "text('before'); try { text('12345678901'); } catch(error) {}",
                ExecuteOptions::default(),
            )
            .await;
        assert_eq!(error(&output).kind, ErrorKind::Sandbox);
        assert_eq!(output.output.len(), 1);
        let items = sandbox
            .execute(
                "text(''); console.log(''); text('');",
                ExecuteOptions::default(),
            )
            .await;
        assert_eq!(error(&items).kind, ErrorKind::Sandbox);
        assert_eq!(items.output.len(), 2);
        let guest_limit = sandbox
            .execute(
                &format!(
                    "try {{ text('x'.repeat({})); }} catch {{}} return 1;",
                    MAX_OUTPUT_CHARS + 1
                ),
                ExecuteOptions::default(),
            )
            .await;
        assert_eq!(error(&guest_limit).kind, ErrorKind::Script);
        assert_eq!(error(&guest_limit).name.as_deref(), Some("RangeError"));
        assert!(guest_limit.output.is_empty());
        let calls = sandbox
            .execute(
                "await tools.echo({}); await tools.echo({}); await tools.echo({});",
                ExecuteOptions::default(),
            )
            .await;
        assert_eq!(error(&calls).kind, ErrorKind::Sandbox);
        assert_eq!(calls.calls.len(), 2);
    }

    struct ActiveCall(Arc<AtomicUsize>);
    impl Drop for ActiveCall {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    async fn tracked_sandbox() -> (
        Arc<CodemodeSandbox>,
        Arc<AtomicUsize>,
        Arc<tokio::sync::Notify>,
    ) {
        let active = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let tool_active = active.clone();
        let tool_entered = entered.clone();
        let wait = CodemodeTool::new(
            ToolDefinition {
                name: "wait".into(),
                ..Default::default()
            },
            move |_, context| {
                let active = tool_active.clone();
                let entered = tool_entered.clone();
                async move {
                    active.fetch_add(1, Ordering::SeqCst);
                    let _active = ActiveCall(active);
                    entered.notify_one();
                    context.cancellation.cancelled().await;
                    Ok(None)
                }
            },
        );
        let sandbox = Arc::new(
            CodemodeSandbox::new(SandboxOptions {
                tools: vec![wait, tool("echo")],
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        (sandbox, active, entered)
    }

    #[tokio::test]
    async fn close_drains_nested_calls() {
        let (sandbox, active, entered) = tracked_sandbox().await;
        let running = sandbox.clone();
        let task = tokio::spawn(async move {
            running
                .execute("await tools.wait({})", ExecuteOptions::default())
                .await
        });
        entered.notified().await;
        sandbox.close().await;
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(error(&task.await.unwrap()).kind, ErrorKind::Aborted);
        assert_eq!(
            error(&sandbox.execute("return 1", ExecuteOptions::default()).await).kind,
            ErrorKind::Aborted
        );
    }

    #[tokio::test]
    async fn dropped_future_drains_calls() {
        let (sandbox, active, entered) = tracked_sandbox().await;
        let running = sandbox.clone();
        let task = tokio::spawn(async move {
            running
                .execute("await tools.wait({})", ExecuteOptions::default())
                .await
        });
        entered.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        sandbox.close().await;
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancellation_and_unawaited() {
        let sandbox = Arc::new(sandbox().await);
        let cancellation = CancellationToken::new();
        let options = ExecuteOptions {
            cancellation: cancellation.clone(),
            ..Default::default()
        };
        let running = sandbox.clone();
        let task = tokio::spawn(async move { running.execute("while(true) {}", options).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancellation.cancel();
        assert_eq!(error(&task.await.unwrap()).kind, ErrorKind::Aborted);
        let (tracked, active, _) = tracked_sandbox().await;
        let unawaited = tracked
            .execute(
                "tools.wait({}); await tools.echo({ms:20}); return 1",
                ExecuteOptions::default(),
            )
            .await;
        assert_eq!(value(&unawaited), Some(&json!(1)));
        assert_eq!(unawaited.calls[0].status, CallStatus::Cancelled);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn epoch_preserves_siblings() {
        let sandbox = sandbox().await;
        let (failed, sibling) = tokio::join!(
            sandbox.execute(
                "while(true) {}",
                ExecuteOptions {
                    timeout: ExecutionTimeout::After(Duration::from_millis(30)),
                    ..Default::default()
                }
            ),
            sandbox.execute(
                "const v = await tools.echo({value:7,ms:100}); return v",
                ExecuteOptions::default()
            ),
        );
        assert_eq!(error(&failed).kind, ErrorKind::Timeout);
        assert_eq!(value(&sibling), Some(&json!(7)));
        let startup = sandbox
            .execute(
                "while(true) {}",
                ExecuteOptions {
                    timeout: ExecutionTimeout::After(Duration::from_nanos(1)),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(error(&startup).kind, ErrorKind::Timeout);
    }

    #[tokio::test]
    async fn fuel_budget_is_enforced() {
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            limits: SandboxLimits {
                fuel: Some(100_000_000),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
        for code in ["while(true) {}", "while(true) await null"] {
            let result = sandbox
                .execute(
                    &format!("text('started'); {code}"),
                    ExecuteOptions::default(),
                )
                .await;
            assert_eq!(error(&result).kind, ErrorKind::Sandbox);
            assert!(error(&result).message.contains("fuel"));
            assert_eq!(result.output.len(), 1);
            assert_eq!(
                value(&sandbox.execute("return 1", ExecuteOptions::default()).await),
                Some(&json!(1))
            );
        }
    }

    #[tokio::test]
    async fn bridge_and_pending_budgets() {
        let large = CodemodeTool::new(
            ToolDefinition {
                name: "large".into(),
                ..Default::default()
            },
            |_, _| async { Ok(Some(json!("x".repeat(70_000)))) },
        );
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            tools: vec![large, tool("echo")],
            limits: SandboxLimits {
                max_source_bytes: 128,
                max_bridge_bytes: 65_536,
                max_pending_calls: 2,
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
        let source = sandbox
            .execute(&" ".repeat(129), ExecuteOptions::default())
            .await;
        assert!(error(&source).message.contains("source budget"));
        let large = sandbox
            .execute(
                "text('before'); return await tools.large({});",
                ExecuteOptions::default(),
            )
            .await;
        assert!(error(&large).message.contains("bridge budget"));
        assert_eq!(large.output.len(), 1);
        let aggregate = sandbox
            .execute(
                "store('key', 'x'.repeat(35000)); return 'x'.repeat(35000)",
                ExecuteOptions::default(),
            )
            .await;
        assert_eq!(error(&aggregate).kind, ErrorKind::Sandbox);
        assert!(error(&aggregate).message.contains("bridge budget"));
        let pending = sandbox
            .execute(
                "await Promise.all([tools.echo({}),tools.echo({}),tools.echo({})]);",
                ExecuteOptions::default(),
            )
            .await;
        assert!(error(&pending).message.contains("pending call budget"));
    }

    #[tokio::test]
    async fn bridge_string_contract() {
        let sandbox = sandbox().await;
        let expected = "\0\"\\\r\n\t𝄞中文";
        let result = sandbox
            .execute(
                &format!(
                    "const value = await tools.echo({}); text(value); console.log(value); store('value', value); return value;",
                    json!({"value": expected})
                ),
                ExecuteOptions::default(),
            )
            .await;
        assert_eq!(value(&result), Some(&json!(expected)));
        assert_eq!(
            result.output,
            vec![
                OutputItem::Text {
                    text: expected.into(),
                    console: false
                },
                OutputItem::Text {
                    text: expected.into(),
                    console: true
                },
            ]
        );
        let ExecutionOutcome::Success { store_writes, .. } = &result.outcome else {
            panic!("{result:?}");
        };
        assert_eq!(store_writes.set["value"], expected);
        for code in [
            r"text('\uD800')",
            r"console.log('\uD800')",
            r"return '\uD800'",
            r"await tools.echo({value:'\uD800'})",
            r"store('value', '\uD800')",
        ] {
            let result = sandbox
                .execute(
                    &format!("text('before'); {code}"),
                    ExecuteOptions::default(),
                )
                .await;
            assert_eq!(error(&result).kind, ErrorKind::Sandbox, "{code}");
            assert_eq!(result.output.len(), 1, "{code}");
        }
    }

    #[tokio::test]
    async fn invalid_inputs_fail_closed() {
        let invalid = CodemodeSandbox::new(SandboxOptions {
            tools: vec![tool("echo"), tool("echo")],
            ..Default::default()
        })
        .await;
        assert!(matches!(invalid, Err(SandboxBuildError::InvalidOptions(_))));
        for names in [
            vec!["tools.shadow"],
            vec!["store"],
            vec!["not-valid"],
            vec!["a.b.c"],
            vec!["models", "models.list"],
            vec!["models.list", "models.list"],
        ] {
            let invalid = CodemodeSandbox::new(SandboxOptions {
                globals: names.into_iter().map(tool).collect(),
                ..Default::default()
            })
            .await;
            assert!(matches!(invalid, Err(SandboxBuildError::InvalidOptions(_))));
        }
        let sandbox = sandbox().await;
        let oversized = ExecuteOptions {
            store: BTreeMap::from([("large".into(), json!("x".repeat(MAX_STORE_VALUE_CHARS)))]),
            ..Default::default()
        };
        let result = sandbox.execute("return 1", oversized).await;
        assert!(error(&result).message.contains("store value budget"));
        let overflow = sandbox
            .execute(
                "return 1",
                ExecuteOptions {
                    timeout: ExecutionTimeout::After(Duration::MAX),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(error(&overflow).kind, ErrorKind::Sandbox);
        let store = sandbox
            .execute(
                &format!(
                    r#"
                    const value = '\uD834\uDD1E'.repeat({});
                    store('key', value);
                    const errors = [];
                    try {{ store('key', value + 'x'); }} catch(error) {{ errors.push(error.name); }}
                    try {{
                        for (let index = 0; index < {}; index++) store(String(index), value);
                    }} catch(error) {{ errors.push(error.name); }}
                    return [errors, load('key') === value];
                "#,
                    MAX_STORE_VALUE_CHARS / 2 - 1,
                    MAX_STORE_TOTAL_CHARS / MAX_STORE_VALUE_CHARS
                ),
                ExecuteOptions::default(),
            )
            .await;
        assert_eq!(
            value(&store),
            Some(&json!([["RangeError", "RangeError"], true]))
        );
    }

    #[tokio::test]
    async fn bridge_and_builtin_lockdown() {
        let sandbox = sandbox().await;
        let result = sandbox.execute(r#"
            try { Object.defineProperty(Array.prototype,'toJSON',{value:()=>['forged']}); } catch(error) {}
            return {bridge:typeof bridge, api:typeof __api, value:await tools.echo({value:[1,2]})};
        "#, ExecuteOptions::default()).await;
        assert_eq!(
            value(&result),
            Some(&json!({"bridge":"undefined","api":"undefined","value":[1,2]}))
        );
    }

    #[test]
    fn runtime_shutdown_stops_worker() {
        for code in [
            "await tools.started({}); while(true) {}",
            "await tools.started({}); while(true) await null",
            "await tools.started({}); await tools.wait({})",
        ] {
            let (ready, started) = std::sync::mpsc::channel();
            let (done, stopped) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let entered = Arc::new(tokio::sync::Notify::new());
                let signal = entered.clone();
                let started_tool = CodemodeTool::new(
                    ToolDefinition {
                        name: "started".into(),
                        ..Default::default()
                    },
                    move |_, _| {
                        let signal = signal.clone();
                        async move {
                            signal.notify_one();
                            Ok(None)
                        }
                    },
                );
                let sandbox = Arc::new(
                    runtime
                        .block_on(CodemodeSandbox::new(SandboxOptions {
                            tools: vec![started_tool, tool("wait")],
                            timeout: None,
                            ..Default::default()
                        }))
                        .unwrap(),
                );
                runtime.block_on(async {
                    let running = sandbox.clone();
                    tokio::spawn(
                        async move { running.execute(code, ExecuteOptions::default()).await },
                    );
                    entered.notified().await;
                    tokio::time::sleep(Duration::from_millis(30)).await;
                });
                ready.send(()).unwrap();
                drop(runtime);
                done.send(()).unwrap();
            });
            started
                .recv_timeout(Duration::from_secs(30))
                .expect("runtime startup failed");
            stopped
                .recv_timeout(Duration::from_secs(2))
                .expect("runtime shutdown left a running WASM worker");
            worker.join().unwrap();
        }
    }

    #[tokio::test]
    async fn aliases_match_declarations() {
        let first = CodemodeTool::new(
            ToolDefinition {
                name: "a-b".into(),
                ..Default::default()
            },
            |_, _| async { Ok(Some(json!(1))) },
        );
        let second = CodemodeTool::new(
            ToolDefinition {
                name: "a+b".into(),
                ..Default::default()
            },
            |_, _| async { Ok(Some(json!(2))) },
        );
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            tools: vec![first, second],
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(sandbox.declarations().matches("a_b(").count(), 1);
        let result = sandbox.execute("return [await tools.a_b({}), await tools['a+b']({}), ALL_TOOLS.map(tool => tool.name)]", ExecuteOptions::default()).await;
        assert_eq!(value(&result), Some(&json!([1, 2, ["a_b"]])));
    }

    #[test]
    fn pinned_guest_digest() {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(include_bytes!("../vendor/quickjs-wasi/quickjs.wasm"));
        assert_eq!(
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            "79505123d7f65bd01202f41b40c2166952454950573402ef23b0bbf557775c82"
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(24))]
        #[test]
        fn bridge_store_round_trip(expected in prop_oneof![
            Just(None),
            Just(Some(Value::Null)),
            (proptest::collection::vec(any::<i32>(), 0..16), ".{0,64}")
                .prop_map(|(values, text)| Some(json!({"values": values, "text": text}))),
        ]) {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(async {
                let sandbox = sandbox().await;
                let args = expected.as_ref().map_or(json!({}), |value| json!({"value": value}));
                let result = sandbox.execute(
                    &format!(r#"
                        const value = await tools.echo({args});
                        store('value', undefined);
                        store('value', value);
                        store('runs', load('runs') + 1);
                        store('remove', undefined);
                        if (value !== undefined && value !== null) {{
                            value.values.push(0);
                            load('value').values.push(1);
                        }}
                        return load('value');
                    "#),
                    ExecuteOptions {
                        store: BTreeMap::from([
                            ("value".into(), json!("stale")),
                            ("runs".into(), json!(2)),
                            ("remove".into(), json!(true)),
                        ]),
                        ..Default::default()
                    },
                ).await;
                let ExecutionOutcome::Success { value, store_writes } = &result.outcome else {
                    panic!("{result:?}");
                };
                assert_eq!(value, &expected);
                assert_eq!(store_writes.set.get("value"), expected.as_ref());
                assert_eq!(store_writes.set["runs"], 3);
                assert!(store_writes.delete.contains(&"remove".into()));
                assert_eq!(store_writes.delete.contains(&"value".into()), expected.is_none());
                assert_eq!(store_writes.delete.len(), if expected.is_none() { 2 } else { 1 });
                assert_eq!(result.calls.len(), 1);
                assert_eq!(result.calls[0].status, CallStatus::Ok);
                sandbox.close().await;
            });
        }
    }
}
