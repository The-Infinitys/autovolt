use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use toml;
use zbus::{Connection, Proxy, Variant, interface, object_server::SignalEmitter};

// ==========================================
// CONFIGURATION & CLI (clap)
// ==========================================
const CONFIG_PATH: &str = "/etc/autovolt.toml";
const BIN_DEST_PATH: &str = "/usr/local/bin/autovolt";
const SERVICE_PATH: &str = "/etc/systemd/system/autovolt.service";
const DBUS_CONF_PATH: &str = "/etc/dbus-1/system.d/net.hadess.PowerProfiles.conf";

#[derive(Parser, Debug)]
#[command(
    name = "autovolt",
    version,
    about = "Dynamic MMIO Power Control & Fake Power Profiles Daemon for Linux"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to custom configuration TOML file
    #[arg(short, long, default_value = CONFIG_PATH)]
    config: PathBuf,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Install binary, systemd service, and D-Bus policy
    Install,
    /// Remove all installed system files and services
    Uninstall,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Config {
    limit_perf_w: u64,
    limit_balanced_w: u64,
    limit_saver_w: u64,
    msr_limit_path: String,
    mmio_limit_path: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            limit_perf_w: 35,
            limit_balanced_w: 20,
            limit_saver_w: 10,
            msr_limit_path: "/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw".into(),
            mmio_limit_path: "/sys/class/powercap/intel-rapl-mmio:0/constraint_0_power_limit_uw"
                .into(),
        }
    }
}

fn load_config(path: &Path) -> Config {
    if path.exists() {
        if let Ok(contents) = fs::read_to_string(path) {
            if let Ok(cfg) = toml::from_str(&contents) {
                return cfg;
            }
        }
    } else {
        let default_cfg = Config::default();
        if let Ok(toml_str) = toml::to_string_pretty(&default_cfg) {
            let _ = fs::write(path, toml_str);
        }
        return default_cfg;
    }
    Config::default()
}

// ==========================================
// INSTALL / UNINSTALL LOGIC
// ==========================================
fn generate_systemd_service(bin_path: &str) -> String {
    format!(
        r#"[Unit]
Description=autovolt Dynamic MMIO Power Control & Fake Power Profiles Daemon
After=dbus.service
Before=powerdevil.service

[Service]
Type=simple
ExecStart={}
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
"#,
        bin_path
    )
}

const DBUS_POLICY_CONTENT: &str = r#"<!DOCTYPE busconfig PUBLIC
 "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
 "http://freedesktop.org">
<busconfig>
  <policy user="root">
    <allow own="net.hadess.PowerProfiles"/>
    <allow send_destination="net.hadess.PowerProfiles"/>
    <allow receive_sender="net.hadess.PowerProfiles"/>
  </policy>
  <policy context="default">
    <allow send_destination="net.hadess.PowerProfiles"/>
    <allow receive_sender="net.hadess.PowerProfiles"/>
  </policy>
</busconfig>
"#;

fn install_system() -> Result<(), Box<dyn std::error::Error>> {
    println!("[autovolt] Installing system files...");

    let current_exe = env::current_exe()?;
    let current_exe_str = current_exe.to_str().ok_or("Invalid executable path")?;

    if current_exe != Path::new(BIN_DEST_PATH) {
        let _ = Command::new("systemctl")
            .args(["stop", "autovolt.service"])
            .status();
        fs::copy(&current_exe, BIN_DEST_PATH)?;
        println!(
            "  -> Copied binary from {} to {}",
            current_exe_str, BIN_DEST_PATH
        );
    }

    // デフォルト設定ファイルの生成
    let _ = load_config(Path::new(CONFIG_PATH));
    println!("  -> Config ensured at {}", CONFIG_PATH);

    fs::write(DBUS_CONF_PATH, DBUS_POLICY_CONTENT)?;
    println!("  -> Created {}", DBUS_CONF_PATH);
    let _ = Command::new("systemctl").args(["reload", "dbus"]).status();

    let service_content = generate_systemd_service(BIN_DEST_PATH);
    fs::write(SERVICE_PATH, service_content)?;
    println!("  -> Created {}", SERVICE_PATH);

    Command::new("systemctl").args(["daemon-reload"]).status()?;
    Command::new("systemctl")
        .args(["enable", "--now", "autovolt.service"])
        .status()?;

    println!("[autovolt] Installation complete! KDE Power Profiles slider is now active.");
    Ok(())
}

fn uninstall_system() -> Result<(), Box<dyn std::error::Error>> {
    println!("[autovolt] Uninstalling system files...");

    let _ = Command::new("systemctl")
        .args(["disable", "--now", "autovolt.service"])
        .status();

    for path in [SERVICE_PATH, DBUS_CONF_PATH, BIN_DEST_PATH, CONFIG_PATH] {
        if Path::new(path).exists() {
            fs::remove_file(path)?;
            println!("  -> Removed {}", path);
        }
    }

    let _ = Command::new("systemctl").args(["daemon-reload"]).status();
    let _ = Command::new("systemctl").args(["reload", "dbus"]).status();

    println!("[autovolt] Uninstallation complete.");
    Ok(())
}

// ==========================================
// POWER CONTROL & D-BUS SERVER (zbus v6)
// ==========================================
struct PowerProfilesDaemon {
    active_profile: Arc<Mutex<String>>,
    config: Config,
}

impl PowerProfilesDaemon {
    fn apply_power_limit(&self, profile: &str) {
        let target_w = match profile {
            "performance" => self.config.limit_perf_w,
            "balanced" => self.config.limit_balanced_w,
            "power-saver" => self.config.limit_saver_w,
            _ => self.config.limit_balanced_w,
        };

        let target_uw = (target_w * 1_000_000).to_string();
        println!("[autovolt] Applying profile '{}' -> {}W", profile, target_w);

        for path in [&self.config.msr_limit_path, &self.config.mmio_limit_path] {
            if Path::new(path).exists() {
                let _ = fs::write(path, &target_uw);
            }
        }
    }
}

#[interface(name = "net.hadess.PowerProfiles")]
impl PowerProfilesDaemon {
    #[zbus(property)]
    async fn active_profile(&self) -> String {
        self.active_profile.lock().await.clone()
    }

    #[zbus(property)]
    async fn set_active_profile(
        &mut self,
        profile: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        if matches!(profile.as_str(), "power-saver" | "balanced" | "performance") {
            *self.active_profile.lock().await = profile.clone();
            self.apply_power_limit(&profile);
            self.active_profile_changed(&emitter).await?;
            Ok(())
        } else {
            Err(zbus::fdo::Error::InvalidArgs("Invalid profile".into()))
        }
    }

    #[zbus(property)]
    async fn profiles(&self) -> Vec<HashMap<String, Variant<'_>>> {
        vec![
            HashMap::from([
                ("Profile".into(), Variant::new("power-saver")),
                ("Driver".into(), Variant::new("autovolt")),
            ]),
            HashMap::from([
                ("Profile".into(), Variant::new("balanced")),
                ("Driver".into(), Variant::new("autovolt")),
            ]),
            HashMap::from([
                ("Profile".into(), Variant::new("performance")),
                ("Driver".into(), Variant::new("autovolt")),
            ]),
        ]
    }

    #[zbus(property)]
    async fn performance_degraded(&self) -> String {
        "".into()
    }

    #[zbus(property)]
    async fn active_profile_holds(&self) -> Vec<HashMap<String, Variant<'_>>> {
        vec![]
    }
}

fn read_supply_watts() -> u64 {
    let ucsi_dir = Path::new("/sys/class/power_supply/ucsi-source-psy-USBC0:1");
    if ucsi_dir.exists() {
        let volt_uv: u64 = fs::read_to_string(ucsi_dir.join("voltage_max_design"))
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0);
        let curr_ua: u64 = fs::read_to_string(ucsi_dir.join("current_max_design"))
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0);
        (volt_uv * curr_ua) / 1_000_000 / 1_000_000
    } else {
        let ac_online: u64 = fs::read_to_string("/sys/class/power_supply/AC/online")
            .unwrap_or_else(|_| "1".into())
            .trim()
            .parse()
            .unwrap_or(1);
        if ac_online == 1 { 65 } else { 0 }
    }
}

async fn run_daemon(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let active_profile = Arc::new(Mutex::new("balanced".to_string()));
    let daemon = PowerProfilesDaemon {
        active_profile: active_profile.clone(),
        config: config.clone(),
    };

    let conn = Connection::system().await?;
    conn.object_server()
        .at("/net/hadess/PowerProfiles", daemon)
        .await?;
    conn.request_name("net.hadess.PowerProfiles").await?;

    println!(
        "[autovolt] Daemon started (zbus v6). Using config: {:?}",
        config
    );

    // タスク1: ACアダプタ電力監視ループ
    let monitor_conn = conn.clone();
    let monitor_profile = active_profile.clone();
    let monitor_config = config.clone();
    tokio::spawn(async move {
        let mut last_w: Option<u64> = None;
        loop {
            let supply_w = read_supply_watts();
            if Some(supply_w) != last_w {
                println!("[autovolt] AC Power Negotiation Changed: {}W", supply_w);
                let new_prof = if supply_w >= 60 {
                    "performance"
                } else {
                    "power-saver"
                };

                *monitor_profile.lock().await = new_prof.to_string();

                // 一時的なインスタンスで適用処理を呼び出す
                let dummy_daemon = PowerProfilesDaemon {
                    active_profile: monitor_profile.clone(),
                    config: monitor_config.clone(),
                };
                dummy_daemon.apply_power_limit(new_prof);

                if let Ok(iface_ref) = monitor_conn
                    .object_server()
                    .interface::<_, PowerProfilesDaemon>("/net/hadess/PowerProfiles")
                    .await
                {
                    let _ = iface_ref
                        .get()
                        .await
                        .active_profile_changed(iface_ref.signal_emitter())
                        .await;
                }

                last_w = Some(supply_w);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    // タスク2: スリープ復帰（PrepareForSleep = false）検知によるRAPL再適用
    let sleep_conn = conn.clone();
    let sleep_profile = active_profile.clone();
    let sleep_config = config.clone();
    tokio::spawn(async move {
        if let Ok(proxy) = Proxy::new(
            &sleep_conn,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )
        .await
        {
            if let Ok(mut stream) = proxy.receive_signal("PrepareForSleep").await {
                while let Some(msg) = stream.next().await {
                    if let Ok(going_to_sleep) = msg.body().deserialize::<bool>() {
                        if !going_to_sleep {
                            println!(
                                "[autovolt] System resumed from sleep. Re-applying power limits..."
                            );
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            let current = sleep_profile.lock().await.clone();
                            let dummy_daemon = PowerProfilesDaemon {
                                active_profile: sleep_profile.clone(),
                                config: sleep_config.clone(),
                            };
                            dummy_daemon.apply_power_limit(&current);
                        }
                    }
                }
            }
        }
    });

    std::future::pending::<()>().await;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = load_config(&cli.config);

    match cli.command {
        Some(Commands::Install) => install_system(),
        Some(Commands::Uninstall) => uninstall_system(),
        None => run_daemon(config).await,
    }
}
