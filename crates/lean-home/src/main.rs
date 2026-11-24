mod auth;
mod http;
mod services;
mod state;
mod ws;

#[cfg(test)]
mod e2e_tests;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

/// The HA version we are wire-compatible with; the frontend gates features on it.
pub const HA_VERSION: &str = "2026.11.2";

pub struct App {
    pub auth: auth::Auth,
    pub states: state::States,
    pub index_html: String,
    pub config_dir: PathBuf,
}

impl App {
    pub fn load(config_dir: &Path, frontend: &Path) -> Result<Self, String> {
        let app = Self {
            auth: auth::Auth::load(config_dir)?,
            states: state::States::default(),
            index_html: http::render_index(frontend)?,
            config_dir: config_dir.into(),
        };
        services::load_demo(&app.states);
        Ok(app)
    }

    /// `get_config`. shortcut: HA's defaults; read `.storage/core.config` when location/units matter.
    pub fn config_json(&self) -> Value {
        json!({
            "allowlist_external_dirs": [],
            "allowlist_external_urls": [],
            "components": ["homeassistant", "frontend", "websocket_api", "auth", "http", "lovelace", "light", "switch", "sensor", "persistent_notification"],
            "config_dir": self.config_dir.display().to_string(),
            "config_source": "default",
            "country": null,
            "currency": "EUR",
            "debug": false,
            "elevation": 0,
            "external_url": null,
            "internal_url": null,
            "language": "en",
            "latitude": 0,
            "location_name": "Home",
            "logging": {"log_file_disabled_reason": null},
            "longitude": 0,
            "radius": 100,
            "recovery_mode": false,
            "safe_mode": false,
            "state": "RUNNING",
            "time_zone": "UTC",
            "unit_system": {"length": "km", "accumulated_precipitation": "mm", "area": "m²", "mass": "g", "pressure": "Pa", "temperature": "°C", "volume": "L", "wind_speed": "m/s"},
            "version": HA_VERSION,
            "whitelist_external_dirs": [],
        })
    }
}

const USAGE: &str = "usage: lean-home [--config DIR] [--frontend DIR] [--listen ADDR]
  --config    HA config dir, already onboarded by HA (default: ./config)
  --frontend  hass_frontend package dir from home-assistant-frontend (default: ./hass_frontend)
  --listen    address to listen on (default: 0.0.0.0:8123)";

fn main() {
    let mut config = PathBuf::from("config");
    let mut frontend = PathBuf::from("hass_frontend");
    let mut listen: SocketAddr = ([0, 0, 0, 0], 8123).into();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut val = || {
            args.next()
                .unwrap_or_else(|| exit(&format!("{arg} needs a value")))
        };
        match arg.as_str() {
            "--config" => config = val().into(),
            "--frontend" => frontend = val().into(),
            "--listen" => {
                listen = val()
                    .parse()
                    .unwrap_or_else(|e| exit(&format!("--listen: {e}")))
            }
            _ => exit(USAGE),
        }
    }
    let app = Arc::new(App::load(&config, &frontend).unwrap_or_else(|e| exit(&e)));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .unwrap_or_else(|e| exit(&format!("{listen}: {e}")));
        eprintln!("lean-home listening on http://{listen}");
        let svc = http::router(app, &frontend).into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, svc)
            .with_graceful_shutdown(async {
                tokio::signal::ctrl_c().await.ok();
            })
            .await
            .unwrap_or_else(|e| exit(&e.to_string()));
    });
}

fn exit(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2)
}
