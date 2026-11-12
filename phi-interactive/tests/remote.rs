#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);
const HTTP_STATUS_OK: &str = "200 OK";

struct GatewayProcess {
    root: PathBuf,
    child: Option<Child>,
}

impl GatewayProcess {
    fn new() -> Self {
        let root =
            PathBuf::from("/tmp").join(format!("phi-gateway-process-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("project")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let opener = root.join("bin/xdg-open");
        std::fs::write(&opener, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(opener, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { root, child: None }
    }

    fn command(&self) -> Command {
        self.command_for(env!("CARGO_BIN_EXE_phi"))
    }

    fn command_for(&self, executable: &str) -> Command {
        let mut command = Command::new(executable);
        command
            .current_dir(self.root.join("project"))
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("HOME", &self.root)
            .env("BROWSER", "/bin/true")
            .env("PHI_CONFIG_DIR", self.root.join("config"))
            .env("PHI_SESSION_DIR", self.root.join("sessions"))
            .env("PHI_AUTH_DIR", self.root.join("auth"))
            .env("PHI_OFFLINE", "1")
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("XDG_CACHE_HOME", self.root.join("xdg-cache"))
            .env("XDG_DATA_HOME", self.root.join("xdg-data"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn wait(&mut self) -> std::process::Output {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            if self.child.as_mut().unwrap().try_wait().unwrap().is_some() {
                return self.child.take().unwrap().wait_with_output().unwrap();
            }
            assert!(Instant::now() < deadline, "view process did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for GatewayProcess {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn provider_request(connection: &mut TcpStream) -> Value {
    connection.set_read_timeout(Some(PROCESS_TIMEOUT)).unwrap();
    let mut input = BufReader::new(connection);
    let mut length = 0;
    loop {
        let mut line = String::new();
        assert_ne!(input.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse::<usize>().unwrap();
        }
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[test]
fn web_and_host_tui_share_one_live_session_and_independent_lifetimes() {
    for decision in ["allow_once", "allow_always", "deny"] {
        let mut process = GatewayProcess::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let marker = process.root.join("permission-effect");
        let marker_command = format!("touch {}", marker.display());
        let tool_release = process.root.join("terminal-tool-release");
        let tool_rendered = process.root.join("terminal-tool-rendered");
        let provider = std::thread::spawn(move || {
            for step in 0..6 {
                let (mut connection, _) = listener.accept().unwrap();
                let request = provider_request(&mut connection);
                let (delta, reason) = if step == 4 {
                    assert!(request["messages"].to_string().contains("terminal-message"));
                    let command = format!(
                        "while ! test -f {}; do sleep 0.02; done; printf 'terminal-%s\\n' 'tool-output'",
                        tool_release.display()
                    );
                    (
                        json!({"tool_calls":[{"index":0,"id":"terminal-tool","type":"function","function":{"name":"Bash","arguments":json!({"command":command}).to_string()}}]}),
                        "tool_calls",
                    )
                } else if matches!(step, 0 | 2) {
                    assert!(
                        request["messages"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|message| message["content"]
                                .to_string()
                                .contains("first-message"))
                    );
                    (
                        json!({"tool_calls":[{"index":0,"id":"permission-tool","type":"function","function":{"name":"Bash","arguments":json!({"command":marker_command}).to_string()}}]}),
                        "tool_calls",
                    )
                } else {
                    let text = match step {
                        1 => "first-reply",
                        3 => "second-reply",
                        _ => "terminal-reply",
                    };
                    if step == 5 {
                        assert!(
                            request["messages"]
                                .to_string()
                                .contains("terminal-tool-output")
                        );
                        assert!(request["messages"].to_string().contains("second-reply"));
                        let deadline = Instant::now() + PROCESS_TIMEOUT;
                        while !tool_rendered.exists() {
                            assert!(
                                Instant::now() < deadline,
                                "TUI did not render the completed tool"
                            );
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                    if step == 3 {
                        assert!(
                            request["messages"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|message| message["content"]
                                    .to_string()
                                    .contains("first-reply"))
                        );
                    }
                    (json!({"content":text}), "stop")
                };
                let chunk = json!({"id":"gateway-completion","model":"test","choices":[{"delta":delta,"finish_reason":reason}]});
                let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                write!(connection, "HTTP/1.1 {HTTP_STATUS_OK}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let providers = [
            ("openai", "openai-responses"), ("anthropic", "anthropic-messages"),
            ("google", "gemini-generate-content"), ("deepseek", "deepseek-chat"),
            ("openai-codex", "openai-codex-responses"), ("openrouter", "openai-completions"),
            ("local", "openai-completions"),
        ].into_iter().map(|(name, api)| (name.to_owned(), json!({"api":api,"baseUrl":format!("http://{address}/v1"),"models":{"test":{"contextWindow":128000}}}))).collect::<serde_json::Map<_, _>>();
        std::fs::write(
            process.root.join("config/models.json"),
            json!({"providers":providers}).to_string(),
        )
        .unwrap();
        std::fs::write(process.root.join("config/settings.json"), r#"{"defaultProvider":"local","defaultModel":"test","memory":{"enabled":false},"sandbox":{"enabled":false},"extensions":["!*.lua"],"permission":{"mode":"allow","bash":"ask"}}"#).unwrap();
        process.child = Some(
            process
                .command()
                .args(["gateway", "--port", "0"])
                .spawn()
                .unwrap(),
        );
        let stdout = process.child.as_mut().unwrap().stdout.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                if let Some(url) = line.strip_prefix("Phi web: ") {
                    let _ = sender.send(url.to_owned());
                }
            }
        });
        let url = receiver
            .recv_timeout(PROCESS_TIMEOUT)
            .unwrap_or_else(|error| {
                let output = process.wait();
                panic!(
                    "Gateway did not start ({error}): {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            });
        let reused = process
            .command()
            .args(["--web", "--no-context-files"])
            .output()
            .unwrap();
        assert!(
            reused.status.success(),
            "{}",
            String::from_utf8_lossy(&reused.stderr)
        );
        assert!(String::from_utf8_lossy(&reused.stdout).contains(&url));
        let endpoint: Value = serde_json::from_slice(
            &std::fs::read(process.root.join("config/gateway/endpoint.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(endpoint["browserUrl"], url);
        assert_eq!(endpoint["localEndpoint"], url);
        assert!(!url.contains(['#', '?']));
        let rejected_reveal = process
            .command()
            .args(["gateway", "token"])
            .output()
            .unwrap();
        assert!(!rejected_reveal.status.success());
        let reveal = process
            .command_for("python3")
            .args(["-c", PAIRING_TOKEN, env!("CARGO_BIN_EXE_phi")])
            .output()
            .unwrap();
        assert!(
            reveal.status.success(),
            "{}",
            String::from_utf8_lossy(&reveal.stderr)
        );
        let entry_token = String::from_utf8(reveal.stdout).unwrap();
        let script = r#"
            import assert from 'node:assert/strict';
            import {webcrypto, createHash} from 'node:crypto';
            import {pathToFileURL} from 'node:url';
            const {proofPayload} = await import(pathToFileURL(process.argv[5]).href);
            const url = new URL(process.argv[1]);
            const decision = process.argv[2];
            const localCredential = process.argv[3];
            const entryToken = process.argv[4].trim();
            assert.equal(url.hash,'');
            assert.equal(url.search,'');
            assert.equal((await fetch(new URL('/api/sessions',url))).status,401);
            assert.equal((await fetch(new URL(`/api/sessions?token=${entryToken}`,url))).status,401);
            const key = await webcrypto.subtle.generateKey('Ed25519',true,['sign','verify']);
            const publicKey = [...new Uint8Array(await webcrypto.subtle.exportKey('raw',key.publicKey))];
            async function authenticate(kind, credential, requestId) {
              const socket = new WebSocket(new URL('/ws',url).href.replace('http:','ws:'),'phi.remote.v1');
              const frames = [];
              let receive;
              socket.addEventListener('message',event => {
                const frame = JSON.parse(event.data);
                if (receive) { const resolve = receive; receive = null; resolve(frame); }
                else frames.push(frame);
              });
              const next = () => frames.length ? Promise.resolve(frames.shift()) : new Promise(resolve => { receive = resolve; });
              const send = (id,method,params) => socket.send(JSON.stringify({type:'req',id,method,params}));
              const hello = await next();
              assert.equal(hello.event,'connect.challenge');
              send('1','connect',{minProtocol:1,maxProtocol:1,auth:{kind,credential,publicKey,...(requestId ? {requestId} : {})}});
              const challenge = await next();
              assert.equal(challenge.payload.status,'proofRequired');
              const signature = [...new Uint8Array(await webcrypto.subtle.sign('Ed25519',key.privateKey,
                proofPayload(challenge.payload.challenge,createHash('sha256').update(credential).digest('hex'))))];
              send('2','connect.prove',{challengeId:challenge.payload.challenge.challengeId,signature});
              const result = await next();
              assert.equal(result.ok,true);
              if (kind !== 'device') socket.close();
              return {payload:result.payload,socket,next,send};
            }
            const {payload:pending} = await authenticate('enroll',entryToken);
            assert.equal(pending.status,'pending');
            const approval = await fetch(new URL(`/api/gateway/devices/${pending.requestId}/approve`,url),{
              method:'POST',headers:{Authorization:`Bearer ${localCredential}`,Origin:url.origin,'Content-Type':'application/json'},body:'{}'});
            assert.equal(approval.status,200);
            const {payload:claimed} = await authenticate('claim',entryToken,pending.requestId);
            assert.equal(claimed.status,'ready');
            const token = claimed.deviceCredential;
            assert.ok(token);
            const observation = await authenticate('device',token);
            assert.equal(observation.payload.status,'ready');
            assert.equal((await fetch(new URL('/api/gateway/devices',url),{headers:{Authorization:`Bearer ${token}`,Origin:url.origin}})).status,403);
            const headers = {Authorization: `Bearer ${token}`, Origin: url.origin, 'Content-Type': 'application/json'};
            const sessions = await (await fetch(new URL('/api/sessions', url), {headers})).json();
            assert.equal(sessions.length,1);
            const sessionId = sessions[0].id;
            async function call(path, body) {
              path = `/api/sessions/${sessionId}/${path.slice('/api/'.length)}`;
              const response = await fetch(new URL(path, url), {method: body ? 'POST' : 'GET',
                headers: {Authorization: `Bearer ${token}`, Origin: url.origin, 'Content-Type': 'application/json'},
                body: body ? JSON.stringify(body) : undefined, signal:AbortSignal.timeout(5000)});
              return {status:response.status, value:await response.json()};
            }
            async function until(predicate) {
              const deadline = Date.now() + 12000;
              while (Date.now() < deadline) {
                const {status,value} = await call('/api/view');
                assert.equal(status,200);
                if (predicate(value)) return value;
                await new Promise(resolve => setTimeout(resolve,30));
              }
              throw new Error('view observation timed out');
            }
            const initial = await call('/api/view');
            assert.equal(initial.value.agents.length,1);
            assert.equal(initial.value.agents[0].id,sessionId);
            assert.equal(initial.value.sessionId,sessionId);
            await call('/api/snapshot');
            observation.send('3','session.watch',{sessionId});
            const watched = await observation.next();
            assert.equal(watched.ok,true);
            const toolEvents = [];
            const observing = (async () => {
              for (;;) {
                const frame = await observation.next();
                if (frame.event === 'session.event' && frame.payload.event.kind === 'phi') {
                  assert.equal(frame.payload.sessionId,sessionId);
                  assert.equal(frame.payload.watchId,'3');
                  const event = frame.payload.event.event;
                  if (event.type.startsWith('tool_call_')) toolEvents.push(event);
                }
                if (toolEvents.filter(event => event.type === 'tool_call_finished').length === 2) return;
              }
            })();
            assert.equal((await call('/api/message',{message:'first-message'})).status,200);
            const blocked = await until(view => view.permissions.length > 0);
            const question = blocked.permissions[0];
            assert.equal(question.tool,'bash');
            assert.equal(question.anchorId,blocked.sessionId);
            assert.ok(question.canAllow);
            assert.ok(question.input.includes('permission-effect'));
            const answer = {agentId:question.agentId,requestId:question.requestId,decision};
            assert.equal((await call('/api/permission',{...answer,agentId:'foreign'})).status,503);
            const replies = await Promise.all([call('/api/permission',answer),call('/api/permission',answer)]);
            assert.deepEqual(replies.map(reply => reply.status).sort(),[200,503]);
            assert.equal(replies.find(reply => reply.status === 200).value.persistenceWarning,null);
            assert.equal((await call('/api/permission',answer)).status,503);
            const first = await until(view => view.conversation.messages.some(message => message.text.includes('first-reply')));
            assert.equal(first.permissions.length,0);
            assert.equal((await call('/api/message',{agentId:first.sessionId,message:'second-message'})).status,200);
            if (decision !== 'allow_always') {
              const repeated = await until(view => view.permissions.length > 0);
              const next = repeated.permissions[0];
              assert.notEqual(next.requestId,question.requestId);
              assert.equal(next.input,question.input);
              assert.equal((await call('/api/permission',{agentId:next.agentId,requestId:next.requestId,decision})).status,200);
            }
            const second = await until(view => view.conversation.messages.some(message => message.text.includes('second-reply')));
            assert.equal(second.permissions.length,0);
            assert.equal(second.sessionId,first.sessionId);
            assert.equal(second.agents.length,1);
            await Promise.race([observing,new Promise((_,reject) => setTimeout(() => reject(new Error('tool events timed out')),5000).unref())]);
            assert.deepEqual(toolEvents.map(event => event.type),['tool_call_started','tool_call_finished','tool_call_started','tool_call_finished']);
            for (const event of toolEvents.filter(event => event.type === 'tool_call_finished')) {
              assert.equal(event.is_error,decision === 'deny');
            }
            assert.ok(second.conversation.messages.some(message => message.role === 'tool' && message.text.includes(decision === 'deny' ? 'failed' : 'finished')));
            observation.socket.close();
            console.log(second.sessionId);
        "#;
        let browser = Command::new("node")
            .args([
                "--experimental-transform-types",
                "--input-type=module",
                "-e",
                script,
                &url,
                decision,
                endpoint["localCredential"].as_str().unwrap(),
                entry_token.trim(),
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../phi-remote/plugins/web/remote/protocol.ts"
                ),
            ])
            .output()
            .unwrap();
        assert!(
            browser.status.success(),
            "{}",
            String::from_utf8_lossy(&browser.stderr)
        );
        assert_eq!(marker.exists(), decision != "deny");
        let session_id = String::from_utf8(browser.stdout).unwrap();
        let terminal = process
            .command_for("python3")
            .env("TERM", "xterm-256color")
            .args([
                "-c",
                TERMINAL_CLIENT,
                env!("CARGO_BIN_EXE_phi"),
                session_id.trim(),
                &url,
                endpoint["localCredential"].as_str().unwrap(),
                process.root.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            terminal.status.success(),
            "{}",
            String::from_utf8_lossy(&terminal.stderr)
        );
        let pid = process.child.as_ref().unwrap().id().to_string();
        assert!(
            Command::new("kill")
                .args([
                    if decision == "allow_once" {
                        "-INT"
                    } else {
                        "-TERM"
                    },
                    &pid
                ])
                .status()
                .unwrap()
                .success()
        );
        let output = process.wait();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        provider.join().unwrap();
        let mut files = Vec::new();
        collect_files(&process.root.join("sessions"), &mut files);
        let sessions = files
            .iter()
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .collect::<Vec<_>>();
        assert_eq!(sessions.len(), 1);
        let content = std::fs::read_to_string(sessions[0]).unwrap();
        let permissions = content
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|entry| entry["type"] == "permission_snapshot")
            .last()
            .unwrap();
        let expected = if decision == "allow_always" {
            json!([{"tool":"bash","pattern":format!("touch {}", marker.display())}])
        } else {
            json!([])
        };
        assert_eq!(permissions["entries"], expected);
        for text in [
            "first-message",
            "first-reply",
            "second-message",
            "second-reply",
            "terminal-message",
            "terminal-reply",
        ] {
            assert!(content.contains(text), "missing persisted {text}");
        }
        let address = url
            .strip_prefix("http://")
            .unwrap()
            .split('/')
            .next()
            .unwrap();
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        while TcpStream::connect(address).is_ok() {
            assert!(
                Instant::now() < deadline,
                "view service survived process exit"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

const PAIRING_TOKEN: &str = r#"
import errno, os, pty, re, select, signal, sys, time
pid, terminal = pty.fork()
if pid == 0:
    os.execv(sys.argv[1], [sys.argv[1], 'gateway', 'token'])
output = b''
exited = False
try:
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if select.select([terminal], [], [], 0.05)[0]:
            try:
                chunk = os.read(terminal, 4096)
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
            if not chunk:
                break
            output += chunk
    while time.monotonic() < deadline:
        waited, status = os.waitpid(pid, os.WNOHANG)
        if waited:
            exited = True
            break
        time.sleep(0.01)
    assert exited and os.waitstatus_to_exitcode(status) == 0, output.decode(errors='replace')
    token = re.search(rb'(?m)^([0-9a-f]{64})\r?$', output)
    assert token, 'pairing token not displayed'
    print(token.group(1).decode())
finally:
    if not exited:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
    os.close(terminal)
"#;

const TERMINAL_CLIENT: &str = r#"
import errno, fcntl, json, os, pty, re, select, signal, struct, sys, termios, time, urllib.parse, urllib.request
pid, terminal = pty.fork()
if pid == 0:
    os.execv(sys.argv[1], [sys.argv[1], '--gateway', '--session', sys.argv[2]])
fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))
output = b''
sent = False
quitting = False
exited = False
next_quit_key = 0
CONTROL_KEY_INTERVAL = 0.1
EXPECTED_TOOL_RESULTS = 3
url = urllib.parse.urlsplit(sys.argv[3])
assert not url.fragment and not url.query
token = sys.argv[4]
tool_started = False
next_expand_key = 0
tool_finished = False
def get(path):
    request = urllib.request.Request(f'{url.scheme}://{url.netloc}{path}', headers={'Authorization': f'Bearer {token}'})
    return json.load(urllib.request.urlopen(request, timeout=3))
try:
    deadline = time.monotonic() + 18
    while time.monotonic() < deadline:
        if select.select([terminal], [], [], 0.05)[0]:
            try:
                chunk = os.read(terminal, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
            if not chunk:
                break
            output += chunk
            if b'\x1b[6n' in chunk:
                os.write(terminal, b'\x1b[1;1R')
        if not sent and b'second-reply' in output:
            os.write(terminal, b'terminal-message\r')
            sent = True
        if sent and not tool_finished:
            view = get(f'/api/sessions/{sys.argv[2]}/view')
            for question in view['permissions']:
                request = urllib.request.Request(
                    f'{url.scheme}://{url.netloc}/api/sessions/{sys.argv[2]}/permission',
                    data=json.dumps({'agentId': question['agentId'], 'requestId': question['requestId'], 'decision': 'allow_once'}).encode(),
                    headers={'Authorization': f'Bearer {token}', 'Content-Type': 'application/json', 'Origin': f'{url.scheme}://{url.netloc}'}, method='POST')
                urllib.request.urlopen(request, timeout=3).close()
            if not tool_started and b'terminal-tool-release' in output:
                tool_started = True
                open(os.path.join(sys.argv[5], 'terminal-tool-release'), 'w').close()
            completed_tools = [message for message in view['conversation']['messages'] if message['role'] == 'tool']
            if tool_started and len(completed_tools) == EXPECTED_TOOL_RESULTS and time.monotonic() >= next_expand_key:
                os.write(terminal, b'\x0f')
                next_expand_key = time.monotonic() + CONTROL_KEY_INTERVAL
            if b'terminal-tool-output' in output:
                assert tool_started, 'tool result appeared before its running display'
                assert b'terminal-reply' not in output, 'tool result must render before assistant completion'
                tool_finished = True
                open(os.path.join(sys.argv[5], 'terminal-tool-rendered'), 'w').close()
        if not quitting and b'terminal-reply' in output:
            quitting = get(f'/api/sessions/{sys.argv[2]}/snapshot')['execution']['status'] == 'idle'
        if quitting and time.monotonic() >= next_quit_key:
            os.write(terminal, b'\x03')
            next_quit_key = time.monotonic() + CONTROL_KEY_INTERVAL
        waited, status = os.waitpid(pid, os.WNOHANG)
        if waited:
            exited = True
            assert os.waitstatus_to_exitcode(status) == 0, output[-4000:]
            break
    exit_deadline = time.monotonic() + 2
    while not exited and time.monotonic() < exit_deadline:
        waited, status = os.waitpid(pid, os.WNOHANG)
        exited = bool(waited)
        if exited:
            assert os.waitstatus_to_exitcode(status) == 0, output[-4000:]
        else:
            time.sleep(0.01)
    rendered = re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', output)
    rendered = re.sub(rb'[^\x00-\x7f]{3} Cooking\.\.\.\s*', b'', rendered)
    assert sent and tool_started and tool_finished and quitting and exited, (sent, tool_started, tool_finished, quitting, exited, rendered[-16000:])
    sessions = get('/api/sessions')
    assert len(sessions) == 1 and sessions[0]['id'] == sys.argv[2], sessions
    view = get(f'/api/sessions/{sys.argv[2]}/view')
    conversation = json.dumps(view['conversation'])
    assert 'terminal-message' in conversation and 'terminal-reply' in conversation, view
finally:
    if not exited:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
    os.close(terminal)
"#;

fn collect_files(directory: &std::path::Path, files: &mut Vec<PathBuf>) {
    if !directory.exists() {
        return;
    }
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_files(&path, files);
        } else {
            files.push(path);
        }
    }
}
