//! A persistent, isolated trial instance backed by Docker Compose.
//!
//! Mastodon: none

use anyhow::{bail, ensure, Context, Result};
use serde_json::json;
use std::{path::PathBuf, process::Stdio};
use tokio::process::Command;

#[derive(clap::Args, Debug)]
pub struct Options {
    /// Public HTTPS origin from your tunnel (no path or nonstandard port).
    #[arg(long, conflicts_with = "tailscale")]
    url: Option<String>,
    /// Discover this machine's Tailscale hostname and run a public Funnel.
    #[arg(long, conflicts_with = "url")]
    tailscale: bool,
    /// Trial directory. Automatic tunnels default to a new session under .eunha-try.
    #[arg(long)]
    directory: Option<PathBuf>,
    /// Account-free SSH provider; otherwise try Serveo, then Pinggy.
    #[arg(long, value_parser = ["serveo.net", "pinggy.io"], conflicts_with_all = ["url", "tailscale"])]
    tunnel_service: Option<String>,
    /// Loopback port to forward with your tunnel.
    #[arg(long, default_value_t = 3000, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,
    /// Owner email; required on the first run. Must have a valid mail domain.
    #[arg(long)]
    email: Option<String>,
    /// Container image, including the bundled frontend.
    #[arg(long, default_value = "ghcr.io/eunha-space/eunha:main")]
    image: String,
    /// Stop the trial containers, retaining all data and configuration.
    #[arg(long)]
    stop: bool,
}

pub(super) fn origin(value: &str) -> Result<String> {
    let url = url::Url::parse(value).context("expected a public HTTPS origin")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "use an HTTPS origin on port 443 without credentials, path, query or fragment"
    );
    ensure!(
        matches!(url.host(), Some(url::Host::Domain(host)) if host.contains('.') && host != "localhost"),
        "use a public DNS hostname"
    );
    Ok(url.origin().ascii_serialization())
}

async fn compose(dir: &std::path::Path, args: &[&str]) -> Result<()> {
    let status = Command::new("docker")
        .args(["compose", "--project-directory"])
        .arg(dir)
        .arg("-f")
        .arg(dir.join("compose.json"))
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("install Docker with Compose and start its daemon")?;
    ensure!(
        status.success(),
        "Docker Compose failed; trial data is retained in {}",
        dir.display()
    );
    Ok(())
}

const DEFAULT_IMAGE: &str = "ghcr.io/eunha-space/eunha:main";

async fn trial_image(requested: &str) -> Result<String> {
    let present = Command::new("docker")
        .args(["image", "inspect", requested])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("install Docker with Compose and start its daemon")?;
    if present.success() {
        return Ok(requested.to_owned());
    }
    let pulled = Command::new("docker")
        .args(["pull", requested])
        .status()
        .await?;
    if pulled.success() {
        return Ok(requested.to_owned());
    }
    ensure!(
        requested == DEFAULT_IMAGE,
        "could not pull {requested}; build a local image or check registry access"
    );
    let local = "eunha:trial";
    if Command::new("docker")
        .args(["image", "inspect", local])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?
        .success()
    {
        println!("Using the previously built {local} image.");
        return Ok(local.to_owned());
    }
    println!("The published image is unavailable. Building eunha from public source; the first build can take several minutes…");
    let built = Command::new("docker")
        .args([
            "build",
            "--tag",
            local,
            "https://github.com/eunha-space/eunha.git#main",
        ])
        .status()
        .await?;
    ensure!(built.success(), "could not build the trial image; from a source checkout run mise run container:build, then use --image eunha:local");
    Ok(local.to_owned())
}

fn configuration(public: &str, secret: &str) -> Result<String> {
    let (vapid_private, vapid_public) = eunha::push::generate_vapid_keypair()?;
    let primary_key = eunha::rails_encryption::generate_secret();
    let salt = eunha::rails_encryption::generate_secret();
    let domain = url::Url::parse(public)?.host_str().unwrap().to_owned();
    Ok(format!(
        r#"database_url = "postgres://eunha:{secret}@postgres/eunha"
redis_url = "redis://redis/"
bind_address = "0.0.0.0:3000"
[active_record_encryption]
primary_key = "{primary_key}"
key_derivation_salt = "{salt}"
[instance]
domain = {domain:?}
title = "eunha trial"
registrations_open = false
vapid_private_key = {vapid_private:?}
vapid_public_key = {vapid_public:?}
secret_key_base = "{secret}"
[media_storage]
bucket = "eunha"
region = "us-east-1"
endpoint = "http://storage:8333"
access_key_id = "eunha"
secret_access_key = "{secret}"
base_url = "{public}/trial-media/eunha"
"#
    ))
}

pub async fn run(options: Options) -> Result<()> {
    let automatic = options.url.is_none() && !options.tailscale && !options.stop;
    let directory = options.directory.clone().unwrap_or_else(|| {
        if automatic {
            PathBuf::from(".eunha-try").join(uuid::Uuid::new_v4().simple().to_string())
        } else {
            PathBuf::from(".eunha-try")
        }
    });
    let dir = std::path::absolute(directory)?;
    if options.stop {
        ensure!(
            dir.join("compose.json").exists(),
            "no trial in {}",
            dir.display()
        );
        return compose(&dir, &["down"]).await;
    }
    let image = if !dir.join("compose.json").exists() {
        ensure!(
            options.email.is_some(),
            "first run needs --email for the owner account"
        );
        trial_image(&options.image).await?
    } else {
        options.image.clone()
    };
    let mut automatic_tunnel = None;
    let requested = if automatic {
        ensure!(!dir.join("compose.json").exists(), "an automatic tunnel gets a new hostname; use a new --directory, or resume this trial with its original --url");
        ensure!(
            options.email.is_some(),
            "first run needs --email for the owner account"
        );
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let (url, child) =
            super::trial_tunnel::open(&dir, options.port, options.tunnel_service.as_deref())
                .await?;
        automatic_tunnel = Some(child);
        Some(url)
    } else if options.tailscale {
        let output = Command::new("tailscale")
            .args(["status", "--json"])
            .output()
            .await
            .context("install Tailscale and sign in first")?;
        ensure!(output.status.success(), "tailscale status failed");
        let status: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let hostname = status["Self"]["DNSName"]
            .as_str()
            .context("Tailscale has no DNS name; enable MagicDNS")?;
        Some(origin(&format!(
            "https://{}",
            hostname.trim_end_matches('.')
        ))?)
    } else {
        options.url.as_deref().map(origin).transpose()?
    };
    if !dir.join("compose.json").exists() {
        let public = requested
            .as_deref()
            .context("first run needs --url https://your-tunnel.example or --tailscale")?;
        ensure!(
            options.email.is_some(),
            "first run needs --email for the owner account"
        );
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let secret = uuid::Uuid::new_v4().simple().to_string();
        let config = configuration(public, &secret)?;
        // Validate the complete generated configuration before creating containers.
        let _: eunha::config::Config = toml::from_str(&config)?;
        std::fs::write(dir.join("config.toml"), config)?;
        std::fs::write(dir.join("origin"), public)?;
        let host = url::Url::parse(public)?.host_str().unwrap().to_owned();
        std::fs::write(dir.join("Caddyfile"), format!(":80 {{\n handle_path /trial-media/* {{\n reverse_proxy storage:8333\n }}\n handle {{\n reverse_proxy web:3000 {{\n header_up Host {host}\n }}\n }}\n}}\n"))?;
        let storage_config = json!({"identities": [
            {"name":"eunha", "credentials":[{"accessKey":"eunha", "secretKey":secret}], "actions":["Admin", "Read", "Write", "List", "Tagging"]},
            {"name":"anonymous", "actions":["Read:eunha"]}
        ]});
        std::fs::write(
            dir.join("storage.json"),
            serde_json::to_vec_pretty(&storage_config)?,
        )?;
        let spec = json!({"name": format!("eunha-try-{}", uuid::Uuid::new_v4().simple()), "services": {
            "postgres": {"image":"postgres:18", "environment":{"POSTGRES_USER":"eunha", "POSTGRES_PASSWORD":secret,"POSTGRES_DB":"eunha"},"volumes":["postgres:/var/lib/postgresql"],"healthcheck":{"test":["CMD-SHELL","pg_isready -U eunha"],"interval":"2s","timeout":"5s","retries":60}},
            "redis":{"image":"redis:7-alpine","command":["redis-server","--appendonly","yes"],"volumes":["redis:/data"],"healthcheck":{"test":["CMD","redis-cli","ping"],"interval":"2s","timeout":"5s","retries":60}},
            "storage":{"image":"chrislusf/seaweedfs:latest","command":["mini","-dir=/data","-s3.config=/etc/eunha-storage.json","-bucket=eunha"],"volumes":["media:/data", "./storage.json:/etc/eunha-storage.json:ro"]},
            "web":{"image":image,"volumes":["./config.toml:/app/config.toml:ro"],"depends_on":{"postgres":{"condition":"service_healthy"},"redis":{"condition":"service_healthy"},"storage":{"condition":"service_started"}}},
            "proxy":{"image":"caddy:2-alpine","ports":[format!("127.0.0.1:{}:80",options.port)],"volumes":["./Caddyfile:/etc/caddy/Caddyfile:ro"],"depends_on":["web"]}
        },"volumes":{"postgres":{},"redis":{},"media":{}}});
        std::fs::write(dir.join("compose.json"), serde_json::to_vec_pretty(&spec)?)?;
    }
    let public = std::fs::read_to_string(dir.join("origin"))?;
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("compose.json"))?)?;
    let port: u16 = saved["services"]["proxy"]["ports"][0]
        .as_str()
        .and_then(|binding| binding.split(':').nth(1))
        .context("trial has no loopback port")?
        .parse()?;
    if requested.as_ref().is_some_and(|url| url != &public) {
        bail!("this trial belongs to {public}; use a new --directory for a different hostname");
    }
    compose(
        &dir,
        &["up", "-d", "--wait", "postgres", "redis", "storage"],
    )
    .await?;
    compose(&dir, &["run", "--rm", "web", "./eunha", "migrate"]).await?;
    compose(
        &dir,
        &["run", "--rm", "web", "./eunha", "settings", "import-config"],
    )
    .await?;
    if !dir.join("owner-created").exists() {
        let email = options
            .email
            .as_deref()
            .context("supply --email to finish creating the owner")?;
        compose(
            &dir,
            &[
                "run",
                "--rm",
                "web",
                "./eunha",
                "accounts",
                "create",
                "owner",
                "--email",
                email,
                "--confirmed",
                "--approve",
                "--role",
                "Owner",
            ],
        )
        .await?;
        std::fs::write(dir.join("owner-created"), "owner\n")?;
    }
    compose(&dir, &["up", "-d", "proxy"]).await?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?;
    let host = url::Url::parse(&public)?.host_str().unwrap().to_owned();
    let mut ready = false;
    for _ in 0..60 {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{port}/api/v2/instance"))
            .header(reqwest::header::HOST, &host)
            .send()
            .await
        {
            if response.status().is_success() {
                ready = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    ensure!(
        ready,
        "trial did not become ready; inspect docker compose -f {} logs web",
        dir.join("compose.json").display()
    );
    println!("Trial: {public}\nSign in as owner using the password printed on first creation.\nLocal listener: http://127.0.0.1:{}\nStop (data retained): eunha try --directory {} --stop", port, dir.display());
    println!(
        "\nGet a hosted instance at https://eunha.space and support the development of eunha.\n"
    );
    if options.tailscale {
        println!("Starting public Tailscale Funnel…");
        automatic_tunnel = Some(
            Command::new("tailscale")
                .args(["funnel", &port.to_string()])
                .kill_on_drop(true)
                .spawn()
                .context("could not start Tailscale Funnel")?,
        );
    }
    if let Some(mut tunnel) = automatic_tunnel {
        println!("Public trial running. Press Ctrl-C to stop (data retained).");
        let result: Result<()> = tokio::select! {
            biased;
            signal = tokio::signal::ctrl_c() => {
                signal?;
                let _ = tunnel.kill().await;
                Ok(())
            }
            status = tunnel.wait() => {
                let _ = status?;
                Err(anyhow::anyhow!("public tunnel exited; restart with a stable URL or create a fresh automatic trial"))
            }
        };
        let stopped = compose(&dir, &["down"]).await;
        result?;
        stopped?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_configuration_is_complete() {
        let text = configuration("https://trial.example", "test-secret").unwrap();
        let config: eunha::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(config.instance.local_domain, "trial.example");
        assert!(!config.instance.registrations_open);
        assert_eq!(
            config.media_storage.base_url,
            "https://trial.example/trial-media/eunha"
        );
        assert!(!config.instance.vapid_private_key.is_empty());
    }
    #[test]
    fn public_origins_only() {
        assert_eq!(
            origin("https://trial.example/").unwrap(),
            "https://trial.example"
        );
        for value in [
            "http://trial.example",
            "https://trial.example/path",
            "https://trial.example:8443",
            "https://user:pass@trial.example",
            "https://127.0.0.1",
            "https://localhost",
            "https://trial.example?q=x",
        ] {
            assert!(origin(value).is_err(), "{value}");
        }
    }
}
