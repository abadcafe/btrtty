use anyhow::Result;

fn main() -> Result<()> {
    platform::run()
}

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{Context, Result};
    use btrtty::app::{self, AppConfig};
    use btrtty::bluetooth::{BluetoothMainThread, BtCommand, INPUT_QUEUE_CAPACITY};
    use btrtty::cli::Cli;
    use btrtty::egress::OutputRate;
    use btrtty::instance::InstanceLock;
    use btrtty::session::SessionConfig;
    use btrtty::shutdown;
    use clap::Parser;
    use objc2::MainThreadMarker;
    use tokio::runtime::Builder;
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;
    use tracing::error;
    use tracing_subscriber::EnvFilter;

    pub fn run() -> Result<()> {
        init_logging();
        let cli = Cli::parse();
        start(cli)
    }

    fn start(args: Cli) -> Result<()> {
        let _instance_lock = InstanceLock::acquire_default()?;
        let mtm = MainThreadMarker::new().context("btrtty must start on the macOS main thread")?;
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .context("create two-worker Tokio runtime")?;
        let signal_waiter = {
            let _runtime_guard = runtime.enter();
            shutdown::SignalWaiter::install()?
        };
        let app_config = AppConfig {
            session: SessionConfig::new(args.command, args.rows, args.cols)?,
            output_rate: OutputRate::new(args.max_output_rate)?,
        };
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let (input_sender, input_receiver) = mpsc::channel(INPUT_QUEUE_CAPACITY);
        let native = BluetoothMainThread::start(event_sender, input_sender, mtm)?;
        let bluetooth = native.command_queue();
        let channel_id = native.channel_id();
        let shutdown_token = CancellationToken::new();
        let signal_token = shutdown_token.clone();
        let native_stop = shutdown_token.clone();

        runtime.spawn(async move {
            if let Err(error) = signal_waiter.wait(signal_token.clone()).await {
                error!(%error, "signal handler failed");
                signal_token.cancel();
            }
        });

        let app_future = app::start(
            event_receiver,
            input_receiver,
            bluetooth.clone(),
            app_config,
            shutdown_token,
        )?;
        let app_task = runtime.spawn(app_future);
        let emergency_stop = bluetooth.clone();
        let (outcome_sender, outcome_receiver) = oneshot::channel();
        runtime.spawn(async move {
            let result = app_task
                .await
                .context("Tokio application task panicked")
                .and_then(|result| result);
            if let Err(error) = &result {
                error!(%error, "application engine failed; requesting native shutdown");
                if let Err(shutdown_error) = emergency_stop.submit(BtCommand::Shutdown) {
                    error!(%shutdown_error, "failed to request native shutdown");
                }
            }
            let _ = outcome_sender.send(result);
        });

        println!("Registered Bluetooth Serial Port");
        println!("Bluetooth address: {}", native.local_address());
        println!(
            "SDP record handle: {:#010X}",
            native.service_record_handle()
        );
        println!("RFCOMM channel: {channel_id}");
        println!("Waiting for paired client...");
        let native_result = native.run();
        native_stop.cancel();
        let app_result = runtime
            .block_on(outcome_receiver)
            .context("application outcome monitor stopped")?;
        match (native_result, app_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(native), Ok(())) => Err(native.context("native Bluetooth loop failed")),
            (Ok(()), Err(app)) => Err(app.context("application engine failed")),
            (Err(native), Err(app)) => Err(anyhow::anyhow!(
                "native Bluetooth loop failed: {native:#}; application engine failed: {app:#}"
            )),
        }
    }

    fn init_logging() {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("btrtty=info")),
            )
            .with_target(false)
            .compact()
            .init();
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use anyhow::{Result, bail};

    pub fn run() -> Result<()> {
        bail!("the btrtty server runs on macOS")
    }
}
