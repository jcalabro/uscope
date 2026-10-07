//! The browser debugger, `uscope web`.
//!
//! One server debugs one program at a time and serves it to every tab that
//! joins: the page, `/api/login`, which turns a join link's token into a
//! cookie, and `/api/ws`, the WebSocket each tab talks over. Like the CLI
//! and the DAP adapter, it is a client of [`uscope::DebuggerHandle`].

mod assets;
mod auth;
mod connection;
mod describe;
mod inspect;
mod lowlevel;
mod picker;
mod protocol;
mod session;
pub mod terminal;
mod values;

use std::ffi::OsString;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use axum::Router;
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use uscope::{CoreDumpOptions, ProcessId};

use auth::{Origins, Tokens};
use protocol::Role;
use session::{LaunchSpec, Session, Start};

/// The port tried first, so links stay the same from run to run.
const DEFAULT_PORT: u16 = 7341;

/// The largest message a tab may send.
const MAX_MESSAGE: usize = 1024 * 1024;

/// Serves a browser debugger.
#[derive(clap::Args)]
#[command(after_help = "\
Common forms:
  uscope web                       choose what to debug in the page
  uscope web EXECUTABLE [-- ARGS]  load a program, which runs at the first continue
  uscope web --attach PID
  uscope web --core CORE [EXECUTABLE]")]
pub struct WebArgs {
    /// Executable to launch, or the one that wrote --core.
    #[arg(value_name = "EXECUTABLE")]
    executable: Option<PathBuf>,

    /// Attach to a running process.
    #[arg(short = 'p', long, value_name = "PID", conflicts_with_all = ["core", "executable"])]
    attach: Option<u64>,

    /// Open a core dump.
    #[arg(long, value_name = "CORE")]
    core: Option<PathBuf>,

    /// Start the program at once instead of at the first continue.
    #[arg(long, conflicts_with_all = ["attach", "core"])]
    run: bool,

    /// Stop at the program's first instruction when it starts.
    #[arg(long, conflicts_with_all = ["attach", "core"])]
    stop_at_entry: bool,

    /// Run the launched program in DIR.
    #[arg(long, value_name = "DIR", conflicts_with_all = ["attach", "core"])]
    cwd: Option<PathBuf>,

    /// Set NAME to VALUE in the launched program's environment. May be repeated.
    #[arg(
        long = "env",
        value_name = "NAME=VALUE",
        value_parser = crate::parse_environment_variable,
        conflicts_with_all = ["attach", "core"]
    )]
    environment: Vec<(OsString, OsString)>,

    /// Listen on 127.0.0.1:PORT. By default 7341, or any free port if it is taken.
    #[arg(long, value_name = "PORT", conflicts_with = "listen")]
    port: Option<u16>,

    /// Listen on ADDRESS instead, such as 0.0.0.0:7341. Anyone who can
    /// reach it and has a link can use the session; prefer an SSH tunnel.
    #[arg(long, value_name = "ADDRESS")]
    listen: Option<SocketAddr>,

    /// Also accept pages served from ORIGIN, such as the development
    /// server's `http://127.0.0.1:5173`. May be repeated.
    #[arg(long = "allow-origin", value_name = "ORIGIN", hide = true)]
    allow_origins: Vec<String>,

    /// Arguments passed to the launched program.
    #[arg(last = true, value_name = "ARGS", conflicts_with_all = ["attach", "core"])]
    arguments: Vec<OsString>,
}

struct App {
    session: Arc<Session>,
    origins: Origins,
    port: u16,
}

impl App {
    /// The role this request's cookie grants, if it came from this server's
    /// own pages.
    fn authorize(&self, headers: &HeaderMap) -> Option<Role> {
        if !self.is_own_page(headers) {
            return None;
        }
        let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
        self.session
            .tokens()
            .role(auth::cookie(cookies, self.port)?)
    }

    fn is_own_page(&self, headers: &HeaderMap) -> bool {
        let text = |name| {
            headers
                .get(name)
                .and_then(|value: &HeaderValue| value.to_str().ok())
        };
        self.origins
            .allows(text(header::HOST), text(header::ORIGIN))
    }

    fn known_host(&self, headers: &HeaderMap) -> bool {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok());
        // An Origin of the host itself always passes for a known host.
        host.is_some_and(|host| {
            self.origins
                .allows(Some(host), Some(&format!("http://{host}")))
        })
    }
}

/// Runs the server until it is interrupted.
pub async fn run(args: &WebArgs) -> Result<()> {
    // Ctrl+C is caught from here on. One that comes while the program starts
    // ends the server once it has, so nothing is left half launched.
    let terminated = crate::dap::termination();
    let start = start_from(args);
    let listener = bind(args).await?;
    let address = listener.local_addr().context("the listening address")?;
    let origins = Origins::new(address, args.allow_origins.clone());
    let cwd = std::env::current_dir().context("the working directory")?;
    let tokens = Tokens::mint().context("failed to mint access tokens")?;
    let session = Session::new(cwd, origins.primary().to_owned(), tokens);
    let app = Arc::new(App {
        session: Arc::clone(&session),
        origins,
        port: address.port(),
    });

    // Before any debugger exists: see `terminal`.
    let link = session.join_link(Role::Control, "/");
    let keys = if terminal::interactive() {
        match terminal::start_opener() {
            Ok(opener) => Some(terminal::Keys::listen(link.clone(), opener)?),
            Err(error) => {
                eprintln!("warning: pressing o cannot open a browser: {error}");
                None
            }
        }
    } else {
        None
    };
    {
        let mut stdout = std::io::stdout().lock();
        writeln!(
            stdout,
            "uscope web: serving http://{}",
            app.origins.primary()
        )?;
        writeln!(stdout, "open {link}")?;
        writeln!(stdout, "anyone with this link can control the program")?;
        if keys.is_some() {
            writeln!(stdout, "press o to open it in your browser")?;
        }
        stdout.flush()?;
    }

    if let Some(start) = start {
        // The page shows a failure to start, as the terminal does.
        if let Err(failure) = session.start(start, false).await {
            eprintln!("error: {}", failure.body().message);
        }
    }

    let router = Router::new()
        .route("/api/ws", get(socket))
        .route("/api/login", post(login))
        .route("/api/check", post(check))
        .fallback(get(page))
        .with_state(app);
    let served = tokio::select! {
        served = axum::serve(listener, router) => served.context("the server failed"),
        () = terminated => Ok(()),
    };
    session.shutdown().await;
    drop(keys);
    served
}

fn start_from(args: &WebArgs) -> Option<Start> {
    if let Some(pid) = args.attach {
        return Some(Start::Attach(ProcessId::new(pid)));
    }
    if let Some(core) = &args.core {
        let mut options = CoreDumpOptions::new(core.clone());
        options.executable.clone_from(&args.executable);
        return Some(Start::Core(options));
    }
    let program = args.executable.clone()?;
    Some(Start::Launch {
        spec: LaunchSpec {
            program,
            arguments: args.arguments.clone(),
            environment: args.environment.clone(),
            cwd: args.cwd.clone(),
            stop_at_entry: args.stop_at_entry,
        },
        run: args.run,
    })
}

async fn bind(args: &WebArgs) -> Result<tokio::net::TcpListener> {
    if let Some(address) = args.listen {
        return tokio::net::TcpListener::bind(address)
            .await
            .with_context(|| format!("cannot listen on {address}"));
    }
    let loopback = |port| SocketAddr::from(([127, 0, 0, 1], port));
    if let Some(port) = args.port {
        return tokio::net::TcpListener::bind(loopback(port))
            .await
            .with_context(|| format!("cannot listen on port {port}"));
    }
    match tokio::net::TcpListener::bind(loopback(DEFAULT_PORT)).await {
        Ok(listener) => Ok(listener),
        Err(_) => tokio::net::TcpListener::bind(loopback(0))
            .await
            .context("cannot listen on any port"),
    }
}

async fn socket(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(role) = app.authorize(&headers) else {
        return (StatusCode::FORBIDDEN, "join with a link first").into_response();
    };
    let session = Arc::clone(&app.session);
    upgrade
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| connection::serve(socket, session, role))
}

/// Trades a join link's token for the cookie that carries it.
async fn login(State(app): State<Arc<App>>, headers: HeaderMap, body: String) -> Response {
    if !app.is_own_page(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if app.session.tokens().role(body.trim()).is_none() {
        return (StatusCode::FORBIDDEN, "this link is not for this server").into_response();
    }
    let cookie = auth::set_cookie(app.port, body.trim());
    HeaderValue::from_str(&cookie).map_or_else(
        |_| StatusCode::BAD_REQUEST.into_response(),
        |cookie| (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response(),
    )
}

/// Says whether this browser's cookie grants access, which a refused
/// WebSocket cannot tell the page.
async fn check(State(app): State<Arc<App>>, headers: HeaderMap) -> StatusCode {
    if app.authorize(&headers).is_some() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::FORBIDDEN
    }
}

async fn page(State(app): State<Arc<App>>, headers: HeaderMap, uri: Uri) -> Response {
    if !app.known_host(&headers) {
        return (StatusCode::MISDIRECTED_REQUEST, "unknown host").into_response();
    }
    if uri.path().starts_with("/api/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(asset) = assets::find(uri.path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cache = if asset.immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (header::CONTENT_TYPE, asset.media_type),
            (header::CACHE_CONTROL, cache),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
                 frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
            ),
        ],
        asset.bytes.into_owned(),
    )
        .into_response()
}
