//! The `trollshell-agent-window` binary: parse the command line, register the
//! per-agent application, and build the window when `GApplication` says to.
//!
//! The window itself and everything it shows live in the library crate — see
//! its module docs for the design, `cli::app_id` for why the application id
//! carries the agent, and `window.rs` for the chrome.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use trollshell_agent_window::cli;
use trollshell_agent_window::window::Window;

/// Default `tracing` level when `RUST_LOG` is unset — `INFO`, matching the
/// shell (#746) and the control center (#780). `fmt::init()`'s own fallback is
/// `ERROR`, and nothing on the launch path sets `RUST_LOG`, so without this
/// every diagnostic here would be discarded on a normal launch.
const DEFAULT_LOG_LEVEL: tracing_subscriber::filter::LevelFilter =
    tracing_subscriber::filter::LevelFilter::INFO;

/// Exit code for a command line this window cannot use.
const EXIT_USAGE: u8 = 2;

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(DEFAULT_LOG_LEVEL.into())
                .from_env_lossy(),
        )
        .init();

    // `args_os` + `to_string_lossy`, not `args`: the latter **panics** on a
    // non-UTF-8 argument, which is the one input this binary works hardest to
    // answer with a usage line (#1130 L5). The `command_line` arm below
    // already did it this way; these two now agree.
    let command_line: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    // Parsed **here**, before the application exists, because the app id is
    // derived from `--agent` and `Application::new` aborts on an invalid one.
    // `get(1..)` rather than `[1..]`: an argv with no argv[0] at all is
    // reachable through a hand-rolled `execv`, and panicking on it would be
    // the same failure as above with a different cause.
    let args = match cli::parse(command_line.get(1..).unwrap_or_default()) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("trollshell-agent-window: {e}\n{}", cli::USAGE);
            return glib::ExitCode::from(EXIT_USAGE);
        }
    };

    // The window's own runtime: it links no `hytte-reactive`, so there is no
    // process-wide one to borrow. Two worker threads is one socket poll and
    // room for the write that interrupts it.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("trollshell-agent-window: could not start a runtime: {e}");
            return glib::ExitCode::FAILURE;
        }
    };

    let app = adw::Application::builder()
        .application_id(cli::app_id(&args.agent))
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    let state: Rc<RefCell<Option<Rc<Window>>>> = Rc::new(RefCell::new(None));
    let handle = runtime.handle().clone();
    app.connect_command_line(move |app, cmd| {
        let args: Vec<String> = cmd
            .arguments()
            .iter()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let args = match cli::parse(&args) {
            Ok(args) => args,
            Err(e) => {
                // The *remote* process learns this through the exit code this
                // returns; `print_literal`, which would put the sentence on
                // its own stderr, is gated on glib's `v2_80` feature and
                // enabling that would widen the whole workspace's glib floor
                // for one diagnostic. The sentence goes to the journal
                // instead, where this window's other diagnostics already are.
                tracing::warn!(error = %e, "{}", cli::USAGE);
                return glib::ExitCode::from(EXIT_USAGE);
            }
        };

        let mut slot = state.borrow_mut();
        let window = slot.get_or_insert_with(|| Window::build(app, &args.agent, &handle));
        window.show_tab(args.tab);
        window.present();
        glib::ExitCode::SUCCESS
    });

    app.run_with_args(&command_line)
}
