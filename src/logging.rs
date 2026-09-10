use std::path::Path;

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

pub const DEFAULT_FILTER: &str = "warn,openuuyc=debug,webrtc=error,webrtc_sctp=error,webrtc_ice=error,webrtc_ice::agent::punch=info,webrtc_ice::agent::agent_internal=info,webrtc_ice::agent::agent_selector=info,webrtc_mdns=error";

pub struct LoggingGuard {
    _file_guard: WorkerGuard,
}

struct ProcessEventFormat(tracing_subscriber::fmt::format::Format);

impl<S, N> tracing_subscriber::fmt::format::FormatEvent<S, N> for ProcessEventFormat
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::format::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        context: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        // The GUI and its viewer intentionally share a diagnostic log. Thread
        // IDs alone are process-local and cannot distinguish their owners.
        write!(writer, "pid={} ", std::process::id())?;
        self.0.format_event(context, writer, event)
    }
}

pub fn init(level: &str, path: &Path) -> Result<LoggingGuard> {
    let file_name = path
        .file_name()
        .context("log path must include a file name")?;
    let directory = path.parent().filter(|value| !value.as_os_str().is_empty());
    if let Some(directory) = directory {
        std::fs::create_dir_all(directory).context("create log directory")?;
    }
    let file = tracing_appender::rolling::never(directory.unwrap_or(Path::new(".")), file_name);
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = EnvFilter::try_new(level).context("invalid log filter")?;
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .event_format(ProcessEventFormat(
                    tracing_subscriber::fmt::format()
                        .with_target(true)
                        .with_thread_ids(true),
                )),
        )
        .try_init()
        .context("initialize logging")?;
    Ok(LoggingGuard { _file_guard: guard })
}
