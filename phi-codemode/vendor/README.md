# Vendored sources

## quickjs-wasi

- Source: https://github.com/vercel-labs/quickjs-wasi, `quickjs-wasi@3.6.2`.
- The WASM is built from pinned sources with `../build-quickjs.sh`.
- Package license: `quickjs-wasi/LICENSE`.
- QuickJS-NG 0.17.0 license: `quickjs-wasi/LICENSE-quickjs`.

### Rebuild

```sh
./phi-codemode/build-quickjs.sh
# Build and replace the vendored WASM:
./phi-codemode/build-quickjs.sh --install
# Optional output path; must not already exist:
./phi-codemode/build-quickjs.sh --output /tmp/quickjs-rebuilt.wasm
```

Requires Bash, GNU make/coreutils, curl, tar and glibc-based Linux x86_64 or
ARM64, with about 2 GiB free under `/tmp/`. The script downloads checksum-pinned
sources and tools, verifies that two clean builds produce identical bytes, and
exports the artifact. Only `--install` replaces the vendored WASM; `--output`
refuses existing files. Cargo does not invoke the script.

After updating pinned inputs, run with `--install`, update the digest test and
licenses, then run `cargo test -p phi-codemode`.

## pi-codemode

- Source: https://github.com/earendil-works/pi/tree/42a3497d03ad17e308a2299fa824727894f2c0ec/packages/codemode
- License: `LICENSE-pi`.
- `src/prelude.js` is the evaluated `PRELUDE_SOURCE` from
  `src/runtime/prelude-source.ts`, with an attribution comment added.
- Original evaluated prelude SHA-256:
  `224cd74082a03a57105af78e1fe205bda692f096252ff4202121016dd78f6228`.
- `src/mcp-types.d.ts` comes from `MCP_TYPESCRIPT_PREAMBLE` in
  `src/declarations.ts`, followed by one newline.
- Source parsing, declaration rendering and the Rust guest adapter follow the
  same package's contracts. The host owns interruption, budgets and persistence.
