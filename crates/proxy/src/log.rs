//! Logging setup: one compact, colour-coded line per event.
//!
//! The previous `pretty()` formatter printed every event as a multi-line
//! block separated by blank lines, which made a busy proxy log unreadable.
//! This uses a custom single-line formatter instead:
//!
//! ```text
//! 17:04:12.381  INFO  deepseek_rs::proxy  request in  thread=thread_ab12… model=deepseek-chat
//! 17:04:13.902  WARN  deepseek_rs::pool   retrying in 1000ms  account=…
//! ```
//!
//! Colours distinguish the parts at a glance: dim timestamp, level in its own
//! colour (red/yellow/green/blue/magenta), cyan target, plain message, dim
//! `key=value` fields.
//!
//! Two profiles:
//!
//! - **debug** (`DEEPSEEK_LOG=debug`, or a `RUST_LOG` override): `debug` level
//!   and call sites.
//! - **prod** (default): `info` level, no call sites.
//!
//! `RUST_LOG` always wins over `DEEPSEEK_LOG`, so a one-off
//! `RUST_LOG=proxy::pool=trace deepseek-rs serve` still works.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::{
    fmt::{
        format::{FormatEvent, FormatFields, Writer},
        FmtContext,
    },
    layer::SubscriberExt,
    registry::LookupSpan,
    util::SubscriberInitExt,
    EnvFilter,
};

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const CYAN: &str = "\x1b[36m";

/// Verbosity profile. `DEEPSEEK_LOG=debug|info|warn|error|off` selects it;
/// anything else falls back to `prod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Prod,
    Debug,
}

impl Profile {
    pub fn from_env() -> Self {
        match std::env::var("DEEPSEEK_LOG")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "debug" | "trace" | "dev" => Profile::Debug,
            _ => Profile::Prod,
        }
    }

    fn default_filter(self) -> &'static str {
        match self {
            // The bin target is `deepseek-rs`, so the crate module path is
            // `deepseek_rs` — filtering on the package name `proxy` matches nothing.
            // `hyper`/`reqwest` are noisy at debug; keep them at warn.
            Profile::Debug => {
                "deepseek_rs=debug,client=debug,pow=debug,protocol=debug,hyper=warn,reqwest=warn"
            }
            Profile::Prod => "deepseek_rs=info,client=info,hyper=warn,reqwest=warn",
        }
    }
}

/// Single-line, colour-coded event formatter.
struct Compact {
    ansi: bool,
}

fn level_style(level: Level) -> (&'static str, &'static str) {
    match level {
        Level::ERROR => ("ERROR", "\x1b[31;1m"),
        Level::WARN => ("WARN ", "\x1b[33;1m"),
        Level::INFO => ("INFO ", "\x1b[32;1m"),
        Level::DEBUG => ("DEBUG", "\x1b[34;1m"),
        Level::TRACE => ("TRACE", "\x1b[35;1m"),
    }
}

impl<S, N> FormatEvent<S, N> for Compact
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();

        // Wall clock, UTC, millisecond precision.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
        let ms = now.subsec_millis();
        if self.ansi {
            write!(writer, "{DIM}{h:02}:{m:02}:{s:02}.{ms:03}{RESET} ")?;
        } else {
            write!(writer, "{h:02}:{m:02}:{s:02}.{ms:03} ")?;
        }

        // Level, then the module/target that emitted it.
        let (label, color) = level_style(*meta.level());
        if self.ansi {
            write!(writer, "{color}{label}{RESET} ")?;
            write!(writer, "{BOLD}{CYAN}{}{RESET}  ", meta.target())?;
        } else {
            write!(writer, "{label} {}  ", meta.target())?;
        }

        // Fields. The message is the `message` field; keeping the default
        // field formatter means `key=value` pairs stay greppable.
        if self.ansi {
            write!(writer, "{DIM}")?;
        }
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        if self.ansi {
            write!(writer, "{RESET}")?;
        }

        writeln!(writer)
    }
}

/// Install the global subscriber. Safe to call once from `main`.
pub fn init(profile: Profile) {
    // NOTE: an empty `RUST_LOG` is accepted by `EnvFilter` as "allow nothing",
    // so treat blank as unset. `try_from_default_env` alone silently disables
    // all logging when the variable exists but is empty.
    let filter = std::env::var("RUST_LOG")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .and_then(|v| EnvFilter::try_new(v).ok())
        .or_else(|| EnvFilter::try_new(profile.default_filter()).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));

    // Colour whenever a human is likely watching: a TTY, or an explicit opt-in.
    // `NO_COLOR` (https://no-color.org) always wins.
    let ansi = match std::env::var("NO_COLOR") {
        Ok(v) if !v.trim().is_empty() => false,
        _ => match std::env::var("DEEPSEEK_LOG_ANSI") {
            Ok(v) => matches!(v.trim(), "1" | "true" | "yes" | "on"),
            Err(_) => is_tty(),
        },
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(Compact { ansi })
                .with_ansi(ansi)
                .with_writer(std::io::stdout),
        )
        .init();
}

fn is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

/// Render an error and every `source()` beneath it, joined with ": ".
///
/// `reqwest`'s own `Display` is only "error sending request for url (...)";
/// the real cause (an expired certificate, a refused connection, a DNS
/// failure) sits further down the chain. Logging just the top line hid a TLS
/// failure that was only visible in the foreground terminal.
pub fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        let text = s.to_string();
        if !out.ends_with(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        cur = s.source();
    }
    out
}

#[cfg(test)]
mod error_chain_tests {
    use super::error_chain;
    use std::fmt;

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);
    impl fmt::Display for Layer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }
    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|l| l as _)
        }
    }

    #[test]
    fn walks_the_whole_source_chain() {
        let e = Layer("error sending request", Some(Box::new(Layer("tls", Some(Box::new(Layer("certificate expired", None)))))));
        assert_eq!(error_chain(&e), "error sending request: tls: certificate expired");
    }

    #[test]
    fn single_error_is_unchanged() {
        assert_eq!(error_chain(&Layer("boom", None)), "boom");
    }
}
