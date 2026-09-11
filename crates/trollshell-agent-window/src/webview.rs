//! The embedded engine — **the one place this crate names `WebKitGTK`**.
//!
//! Spec #955 §7.1 decided the engine for v1 (`webkitgtk_6_0` through the
//! `webkit6` gtk-rs crate) and recorded Servo as a *re-check*, not a someday:
//! its embedding API is the unfinished part (servo/servo#27579 open since
//! 2020; the one GTK4 integration re-executes your binary in a subprocess
//! rather than rendering in-process; Verso archived 2025-10), and CSS Grid is
//! the known web-platform gap. The trigger for revisiting is concrete — a
//! maintained crate offering in-process `GtkGLArea` rendering with input wired
//! up.
//!
//! Until then a **cargo feature per engine would be a second code path nobody
//! compiles**, which is worse than a module boundary: everything outside this
//! file talks about "the page widget" and a URL, so the day there is a second
//! engine to try, this file is the whole surface it has to satisfy.

use adw::prelude::*;
use webkit::prelude::*;

use crate::tls::{CA_ENV, TlsPolicy};

/// Build the page view for `url`, under `policy`.
///
/// The network session is configured **before** the first load: the
/// TLS-errors policy is set explicitly (always
/// [`Fail`](webkit::TLSErrorsPolicy::Fail), see [`crate::tls`]) rather than
/// left at the default, so a reader of this file can see that nothing here
/// turns verification off.
#[must_use]
pub fn page(url: &str, policy: &TlsPolicy) -> gtk::Widget {
    let session = webkit::NetworkSession::default()
        .unwrap_or_else(|| webkit::NetworkSession::new(None, None));
    session.set_tls_errors_policy(policy.errors_policy());

    if let TlsPolicy::AllowCertificateForHost { pem, host } = policy {
        match gtk::gio::TlsCertificate::from_file(pem) {
            Ok(cert) => {
                session.allow_tls_certificate_for_host(&cert, host);
                tracing::info!(
                    pem = %pem.display(),
                    host,
                    "trusting the certificate {CA_ENV} names, for this host only"
                );
            }
            Err(e) => tracing::warn!(
                pem = %pem.display(),
                error = %e,
                "{CA_ENV} does not name a readable certificate; using the system trust store"
            ),
        }
    }

    let view = webkit::WebView::builder()
        .network_session(&session)
        .hexpand(true)
        .vexpand(true)
        .build();

    // Deliberately **not** "accept and reload": that would be trust on first
    // use with no human in it. Returning `false` lets `WebKit` render its own
    // failure page, and the log line names the way out.
    view.connect_load_failed_with_tls_errors(move |_, failing_uri, _cert, errors| {
        tracing::warn!(
            uri = failing_uri,
            ?errors,
            "TLS verification failed for the hive; add its trust-bundle.pem to the system store \
             (security.pki.certificateFiles) or point {CA_ENV} at it"
        );
        false
    });

    view.load_uri(url);
    view.upcast()
}
