use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, ensure};
use phi_remote::{Gateway, RemoteClient, deployment as gateway};
use phi_sdk::agent_manager::AgentManager;

use crate::runtime::preparation::SessionWorkflows;

pub(crate) fn gateway_directory() -> PathBuf {
    std::env::var_os("PHI_GATEWAY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::config::settings::phi_path().join("gateway"))
}

pub(crate) struct GatewayEntry {
    directory: PathBuf,
    endpoint: gateway::Endpoint,
    service: Option<Gateway>,
    _registration: Option<gateway::Registration>,
}

pub(crate) async fn existing() -> anyhow::Result<Option<GatewayEntry>> {
    existing_in(&gateway_directory()).await
}

async fn existing_in(directory: &Path) -> anyhow::Result<Option<GatewayEntry>> {
    let Some(endpoint) = gateway::discover(directory)? else {
        return Ok(None);
    };
    if !endpoint.is_available().await? {
        return Ok(None);
    }
    Ok(Some(GatewayEntry {
        directory: directory.to_owned(),
        endpoint,
        service: None,
        _registration: None,
    }))
}

impl GatewayEntry {
    async fn start(
        manager: &AgentManager,
        workflows: &SessionWorkflows,
        persist: bool,
        port: Option<u16>,
    ) -> anyhow::Result<Self> {
        let directory = gateway_directory();
        if let Some(view) = existing_in(&directory).await? {
            return Ok(view);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let (service, registration, endpoint) = loop {
            match gateway::start(
                &directory,
                manager.clone(),
                workflows.remote_host(persist),
                web_directory().join("dist/public"),
                port,
            )
            .await
            {
                Ok(started) => break started,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) =>
                {
                    if let Some(service) = existing_in(&directory).await? {
                        return Ok(service);
                    }
                    ensure!(
                        tokio::time::Instant::now() < deadline,
                        "another Phi process owns an unresponsive gateway"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => return Err(error),
            }
        };
        Ok(Self {
            directory,
            endpoint,
            service: Some(service),
            _registration: Some(registration),
        })
    }

    pub(crate) fn url(&self) -> &str {
        self.endpoint.browser_url()
    }

    pub(crate) fn client(&self) -> anyhow::Result<RemoteClient> {
        self.endpoint.client()
    }

    pub(crate) async fn close(mut self) -> anyhow::Result<()> {
        if let Some(service) = self.service.take() {
            service.close().await?;
        }
        Ok(())
    }
}

pub(crate) async fn select_session(
    client: &RemoteClient,
    id: Option<&str>,
    working_dir: &Path,
) -> anyhow::Result<phi_remote::RemoteSession> {
    let sessions = client.sessions().await?;
    match id {
        Some(id) => sessions
            .into_iter()
            .find(|session| session.id == id)
            .context("Session is not hosted by this Gateway"),
        None => match sessions.into_iter().next() {
            Some(session) => Ok(session),
            None => client.create(working_dir).await,
        },
    }
}

pub(crate) async fn open_browser(url: String) {
    if !matches!(
        tokio::task::spawn_blocking(move || open::that(url)).await,
        Ok(Ok(()))
    ) {
        eprintln!("Could not open a browser; open the URL above manually.");
    }
}

fn sole_candidate<'a>(
    ids: impl Iterator<Item = &'a str>,
    empty_message: &str,
    multiple_message: &str,
) -> anyhow::Result<&'a str> {
    let ids: Vec<_> = ids.collect();
    match ids.as_slice() {
        [id] => Ok(id),
        [] => anyhow::bail!("{empty_message}"),
        _ => anyhow::bail!("{multiple_message}\n{}", ids.join("\n")),
    }
}

pub(crate) async fn manage(action: &super::GatewayCommand) -> anyhow::Result<()> {
    if matches!(action, super::GatewayCommand::Token) {
        use std::io::IsTerminal;
        ensure!(
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            "pairing token reveal requires an interactive terminal; redirection is not allowed"
        );
        println!("{}", gateway::entry_token(&gateway_directory())?);
        return Ok(());
    }
    let Some(service) = existing().await? else {
        println!("Gateway is not running.");
        return Ok(());
    };
    let client = service.client()?;
    match action {
        super::GatewayCommand::Token => unreachable!(),
        super::GatewayCommand::Devices { action } => match action {
            super::DeviceCommand::List => {
                let devices = client.devices().await?;
                println!("Pending requests:");
                for request in devices.pending {
                    println!(
                        "{}  device={}  expires={}",
                        request.request_id, request.device_id, request.expires_at
                    );
                }
                println!("Approved devices:");
                for device in devices.approved {
                    let expiry = device
                        .expires_at
                        .map_or_else(|| "never".into(), |deadline| deadline.to_string());
                    println!(
                        "{}  approved={}  expires={expiry}",
                        device.device_id, device.approved_at
                    );
                }
            }
            super::DeviceCommand::Approve { request_id } => {
                let devices;
                let request_id = match request_id.as_deref() {
                    Some(request_id) => request_id,
                    None => {
                        devices = client.devices().await?;
                        sole_candidate(
                            devices
                                .pending
                                .iter()
                                .map(|request| request.request_id.as_str()),
                            "No pending pairing requests. Submit a request in the browser first.",
                            "Multiple pending pairing requests. Run phi gateway devices approve <request-id>:",
                        )?
                    }
                };
                client.approve_device(request_id).await?;
                println!("Pairing request approved.");
            }
            super::DeviceCommand::Reject { request_id } => {
                client.reject_device(request_id).await?;
                println!("Pairing request rejected.");
            }
            super::DeviceCommand::Revoke { device_id } => {
                let devices;
                let device_id = match device_id.as_deref() {
                    Some(device_id) => device_id,
                    None => {
                        devices = client.devices().await?;
                        sole_candidate(
                            devices
                                .approved
                                .iter()
                                .map(|device| device.device_id.as_str()),
                            "No approved devices.",
                            "Multiple approved devices. Run phi gateway devices revoke <device-id>:",
                        )?
                    }
                };
                client.revoke_device(device_id).await?;
                println!("Device revoked.");
            }
        },
        super::GatewayCommand::Status => println!(
            "Gateway is running ({} active Sessions).",
            client.sessions().await?.len()
        ),
        super::GatewayCommand::Stop => {
            client.stop().await?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
            loop {
                if service.endpoint.retired(&service.directory)? {
                    break;
                }
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "gateway shutdown timed out"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            println!("Gateway stopped.");
        }
    }
    Ok(())
}

pub(crate) async fn prepare(
    manager: AgentManager,
    workflows: SessionWorkflows,
    cli: &super::Cli,
) -> anyhow::Result<GatewayEntry> {
    let port = match cli.command {
        Some(super::Command::Gateway { port, .. }) => port,
        _ => None,
    };
    GatewayEntry::start(&manager, &workflows, !cli.no_session, port).await
}

pub(crate) async fn run(service: &mut GatewayEntry) -> anyhow::Result<()> {
    println!("Phi web: {}", service.url());
    let Some(gateway) = &mut service.service else {
        return Ok(());
    };
    println!("Keep this process running. Press Ctrl+C to stop and save Sessions.");
    tokio::select! {
        result = shutdown_signal() => {
            result?;
            gateway.request_shutdown();
        },
        result = gateway.wait() => {
            result?;
            ensure!(gateway.is_stopping(), "phi-remote gateway disconnected");
        },
    }
    Ok(())
}

const SETUP_COMMANDS: [&[&str]; 2] = [&["install", "--frozen-lockfile"], &["build"]];

fn web_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../phi-remote/plugins/web")
}

pub(crate) fn needs_setup() -> bool {
    !web_directory().join("dist/public/app.js").is_file()
}

pub(crate) fn setup_web() -> anyhow::Result<()> {
    let directory = web_directory();
    confirm_setup(
        &directory,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
        |arguments| {
            let status = std::process::Command::new("pnpm")
                .args(arguments)
                .current_dir(&directory)
                .status()
                .context(
                    "cannot run pnpm; install Node.js 22 or newer and pnpm, then retry phi --web",
                )?;
            ensure!(
                status.success(),
                "pnpm {} failed: {status}",
                arguments.join(" ")
            );
            Ok(())
        },
    )
}

fn confirm_setup(
    directory: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
    mut run: impl FnMut(&[&str]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    writeln!(
        output,
        "\nWeb assets need a first-time build. Setup runs outside the interactive UI."
    )?;
    writeln!(output, "Directory: {}", directory.display())?;
    writeln!(
        output,
        "Building browser assets requires Node.js and pnpm; the running gateway does not require Node.js."
    )?;
    writeln!(
        output,
        "This downloads npm dependencies, runs approved install scripts and builds dist/:"
    )?;
    for arguments in SETUP_COMMANDS {
        writeln!(output, "  pnpm {}", arguments.join(" "))?;
    }
    write!(output, "Continue? [y/N] ")?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        writeln!(output, "Setup cancelled. No commands were run.")?;
        return Ok(());
    }
    for arguments in SETUP_COMMANDS {
        writeln!(output, "\n$ pnpm {}", arguments.join(" "))?;
        output.flush()?;
        run(arguments)?;
    }
    writeln!(
        output,
        "\nWeb is built. Run phi --web again to open the Gateway page."
    )?;
    Ok(())
}

async fn shutdown_signal() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config { failure_persistence: None, ..Default::default() })]

        #[test]
        fn automatic_selection_never_picks_from_ambiguous_candidates(
            ids in proptest::collection::vec("[a-z0-9-]{1,32}", 0..16),
        ) {
            let selected = sole_candidate(ids.iter().map(String::as_str), "No candidates.", "Choose an ID:");
            match ids.as_slice() {
                [id] => proptest::prop_assert_eq!(selected.unwrap(), id),
                [] => proptest::prop_assert_eq!(selected.unwrap_err().to_string(), "No candidates."),
                _ => {
                    let error = selected.unwrap_err().to_string();
                    proptest::prop_assert!(error.starts_with("Choose an ID:\n"));
                    proptest::prop_assert_eq!(error.lines().skip(1).collect::<Vec<_>>(), ids);
                }
            }
        }
    }

    #[test]
    fn setup_requires_explicit_confirmation() {
        for answer in ["", "\n", "n\n", "N\n", "no\n", "maybe\n"] {
            let mut output = Vec::new();
            confirm_setup(
                Path::new("/tmp/phi-web-setup"),
                &mut answer.as_bytes(),
                &mut output,
                |_| panic!("unconfirmed setup must not run commands"),
            )
            .unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("Continue? [y/N]"));
            assert!(output.contains("Setup cancelled"));
            assert!(!output.contains("$ pnpm"));
        }
    }

    #[test]
    fn confirmed_setup_runs_install_then_build_without_resuming_ui() {
        for answer in ["y\n", "Y\n", "yes\n", " YES \n"] {
            let mut output = Vec::new();
            let mut commands = Vec::new();
            confirm_setup(
                Path::new("/tmp/phi-web-setup"),
                &mut answer.as_bytes(),
                &mut output,
                |arguments| {
                    commands.push(
                        arguments
                            .iter()
                            .map(|value| value.to_string())
                            .collect::<Vec<_>>(),
                    );
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(
                commands,
                [vec!["install", "--frozen-lockfile"], vec!["build"]]
            );
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("Run phi --web again"));
        }
    }

    #[test]
    fn setup_stops_on_failure_and_does_not_report_success() {
        for failed_step in 0..SETUP_COMMANDS.len() {
            let mut output = Vec::new();
            let mut calls = 0;
            let error = confirm_setup(
                Path::new("/tmp/phi-web-setup"),
                &mut "y\n".as_bytes(),
                &mut output,
                |_| {
                    let step = calls;
                    calls += 1;
                    anyhow::ensure!(step != failed_step, "setup command failed");
                    Ok(())
                },
            )
            .unwrap_err();
            assert_eq!(calls, failed_step + 1);
            assert!(error.to_string().contains("setup command failed"));
            assert!(!String::from_utf8(output).unwrap().contains("Web is built"));
        }
    }
}
