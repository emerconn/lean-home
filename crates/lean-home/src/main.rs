mod auth;
mod condition;
mod config;
mod http;
mod mqtt;
mod registry;
mod script;
mod services;
mod state;
mod template;
mod trace;
mod ws;

#[cfg(test)]
mod e2e_tests;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde_json::{Value, json};

/// The HA version we are wire-compatible with; the frontend gates features on it.
pub const HA_VERSION: &str = "2026.11.2";

pub struct App {
    pub auth: auth::Auth,
    pub states: Arc<state::States>,
    pub index_html: String,
    pub config_dir: PathBuf,
    pub registry: registry::Registry,
    pub mqtt: OnceLock<Arc<mqtt::Mqtt>>,
    pub config: config::Config,
    pub scripts: script::Scripts,
}

impl App {
    pub fn load(config_dir: &Path, frontend: &Path, demo: bool) -> Result<Self, String> {
        let config = config::load(config_dir)?;
        template::set_time_zone(config.core.tz.clone());
        eprintln!(
            "config: {} automations, {} scripts",
            config.automations.len(),
            config.scripts.len()
        );
        let registry = registry::Registry::load(config_dir)?;
        let states: Arc<state::States> = Arc::default();
        let scripts = script::Scripts::load(&config.scripts, &registry, &states);
        let app = Self {
            config,
            auth: auth::Auth::load(config_dir)?,
            states,
            index_html: http::render_index(frontend)?,
            config_dir: config_dir.into(),
            registry,
            mqtt: OnceLock::new(),
            scripts,
        };
        if demo {
            services::load_demo(&app.states);
        }
        Ok(app)
    }

    /// Loaded components, as HA lists them; the frontend shows features based on this.
    fn components(&self) -> Vec<&'static str> {
        let mut c = vec![
            "homeassistant",
            "frontend",
            "websocket_api",
            "auth",
            "http",
            "lovelace",
            "light",
            "switch",
            "sensor",
            "persistent_notification",
        ];
        if self.mqtt.get().is_some() {
            c.extend(["mqtt", "binary_sensor"]);
        }
        c
    }

    /// `get_config`
    pub fn config_json(&self) -> Value {
        let c = &self.config.core;
        json!({
            "allowlist_external_dirs": [],
            "allowlist_external_urls": [],
            "components": self.components(),
            "config_dir": self.config_dir.display().to_string(),
            "config_source": c.config_source,
            "country": c.country,
            "currency": c.currency,
            "debug": false,
            "elevation": c.elevation,
            "external_url": c.external_url,
            "internal_url": c.internal_url,
            "language": c.language,
            "latitude": c.latitude,
            "location_name": c.location_name,
            "logging": {"log_file_disabled_reason": null},
            "longitude": c.longitude,
            "radius": c.radius,
            "recovery_mode": false,
            "safe_mode": false,
            "state": "RUNNING",
            "time_zone": c.time_zone,
            "unit_system": c.units_json(),
            "version": HA_VERSION,
            "whitelist_external_dirs": [],
        })
    }
}

const USAGE: &str = "usage: lean-home [--config DIR] [--frontend DIR] [--listen ADDR] [--demo]
  --config    HA config dir, already onboarded by HA (default: ./config)
  --frontend  hass_frontend package dir from home-assistant-frontend (default: ./hass_frontend)
  --listen    address to listen on (default: 0.0.0.0:8123)
  --demo      add demo entities (light.kitchen, switch.fan, sensor.temperature)
MQTT starts when the config dir has an enabled mqtt config entry, as set up by HA.";

fn main() {
    let mut config = PathBuf::from("config");
    let mut frontend = PathBuf::from("hass_frontend");
    let mut listen: SocketAddr = ([0, 0, 0, 0], 8123).into();
    let mut demo = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut val = || {
            args.next()
                .unwrap_or_else(|| exit(&format!("{arg} needs a value")))
        };
        match arg.as_str() {
            "--config" => config = val().into(),
            "--frontend" => frontend = val().into(),
            "--demo" => demo = true,
            "--listen" => {
                listen = val()
                    .parse()
                    .unwrap_or_else(|e| exit(&format!("--listen: {e}")))
            }
            _ => exit(USAGE),
        }
    }
    let app = Arc::new(App::load(&config, &frontend, demo).unwrap_or_else(|e| exit(&e)));
    let mqtt = mqtt::settings(&config).unwrap_or_else(|e| exit(&e));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async move {
        if let Some(settings) = mqtt {
            let _ = app.mqtt.set(mqtt::Mqtt::start(app.clone(), settings));
        }
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .unwrap_or_else(|e| exit(&format!("{listen}: {e}")));
        eprintln!("lean-home listening on http://{listen}");
        let svc = http::router(app.clone(), &frontend)
            .into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, svc)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .unwrap_or_else(|e| exit(&e.to_string()));
        if let Some(m) = app.mqtt.get() {
            m.stop().await;
        }
    });
}

/// Ctrl-C, or SIGTERM from `docker stop` / systemd.
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

fn exit(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2)
}

#[cfg(test)]
#[test]
fn ha_version_is_the_compat_pin() {
    let pins = include_str!("../../../compat/versions.env");
    assert!(
        pins.lines()
            .any(|l| l == format!("HA_VERSION={HA_VERSION}")),
        "HA_VERSION in main.rs differs from compat/versions.env"
    );
}
