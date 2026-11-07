//! Account-free SSH tunnels, following Fedify CLI's provider registry.
//!
//! Mastodon: none
//
// Provider host keys: Fedify's tunnelservice.ts and dahlia/localtunnel's
// service.ts (verified upstream in July 2026). Never disable host checking.
use anyhow::{ensure, Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
};

const KNOWN_HOSTS: &str = "[fedify.com.es]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOepK/E2PANumZNFCicc/zv4EkyraFwqV8qveMtDC5b+\nserveo.net ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDxYGqSKVwJpQD1F0YIhz+bd5lpl7YesKjtrn1QD1RjQcSj724lJdCwlv4J8PcLuFFtlAA8AbGQju7qWdMN9ihdHvRcWf0tSjZ+bzwYkxaCydq4JnCrbvLJPwLFaqV1NdcOzY2NVLuX5CfY8VTHrps49LnO0QpGaavqrbk+wTWDD9MHklNfJ1zSFpQAkSQnSNSYi/M2J3hX7P0G2R7dsUvNov+UgNKpc4n9+Lq5Vmcqjqo2KhFyHP0NseDLpgjaqGJq2Kvit3QowhqZkK4K77AA65CxZjdDfpjwZSuX075F9vNi0IFpFkGJW9KlrXzI4lIzSAjPZBURhUb8nZSiPuzj\n[free.pinggy.io]:443 ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAACAQDF0YJigZJU62vn4rsKGRjIRTtMe/suc3d4YDe0iIvFzLMuaN78oxhWn9Uqefe1gN++dYVssspsgsvTXTTBcxxo3WoFeNr1z/+osJ45+Yxoa0pbaJdAwbr8CqjDa96r9/AhAXHoKncAByEOSiXfdWCXf84YC+Hu48/gZOqSZ3VqPz+nNGFByJcqYJ+jSELSqCNWVLWFxx7vH270Kymw2XkdOW47zzDNO7X4uByxHfaZMgI6phoaNglGizM0VNMQPL5GbspVGejFQE85QJbX3oF8vuCYnM+OMkwopHG+muh6Tro8+fm6G/fcmu34YJNbU3oaTdW1YPqvcKFX1AuIY9CA5lLZR9A1rOJ+fd4JEYaoTxwUN2ZPcrf7JEnvHmcV9hmupTSllJzLk4smDpl5PSknDm68/h/z/ZmaDlunGsHnn397fwCwS7sO9Q1yIuZ+Bri0td7+N2EK1mvM/qsnrSauOymcmqYVy6TLiejHdoVl8+lKqatkTxyFf/3MP8ylCKSoP0SJZratcU1n+0EciG+IjEzdPZ/1tuJZhBWqOUbYfUl+WgovH+J+AQKtoNzPP+fLtLNcmLEhx99N2y5l7A8IOlyy41Minq4N7V5X8Q7QHhEoocatNNn5JRYe/25P9aQelF0ItMD0PEmf8rIHWMqbwnwQ8pVVdDhE6mwhDskBIw==\n";

struct Service {
    name: &'static str,
    host: &'static str,
    ssh_port: &'static str,
    remote_port: u16,
    pattern: &'static str,
}
const SERVICES: &[Service] = &[
    Service {
        name: "fedify.com.es",
        host: "fedify.com.es",
        ssh_port: "2222",
        remote_port: 80,
        pattern: r"https://[a-z0-9]{16}\.fedify\.com\.es(?:/|\s|$)",
    },
    Service {
        name: "serveo.net",
        host: "serveo.net",
        ssh_port: "22",
        remote_port: 80,
        pattern: r"https://[a-z0-9-]+\.(?:serveo\.net|serveousercontent\.com)(?:/|\s|$)",
    },
    Service {
        name: "pinggy.io",
        host: "free.pinggy.io",
        ssh_port: "443",
        remote_port: 0,
        pattern: r"https://[a-z0-9-]+\.(?:free\.pinggy\.net|run\.pinggy-free\.link)(?:/|\s|$)",
    },
];

fn discover(service: &Service, output: &str) -> Option<String> {
    let pattern = regex::Regex::new(service.pattern).ok()?;
    let found = pattern.find(output)?.as_str().trim().trim_end_matches('/');
    super::try_instance::origin(found).ok()
}

pub async fn open(dir: &Path, port: u16, selected: Option<&str>) -> Result<(String, Child)> {
    let hosts = dir.join("tunnel-known-hosts");
    std::fs::write(&hosts, KNOWN_HOSTS)?;
    let quoted = hosts
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    for service in SERVICES
        .iter()
        .filter(|s| selected.is_none_or(|name| name == s.name))
    {
        println!("Creating a public HTTPS tunnel through {}…", service.name);
        let mut command = Command::new("ssh");
        command
            .args([
                "-F",
                "/dev/null",
                "-p",
                service.ssh_port,
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "GlobalKnownHostsFile=/dev/null",
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentityAgent=none",
                "-o",
                "PubkeyAuthentication=no",
                "-o",
                "PasswordAuthentication=no",
                "-o",
                "KbdInteractiveAuthentication=no",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ConnectTimeout=8",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "UpdateHostKeys=no",
            ])
            .arg("-o")
            .arg(format!("UserKnownHostsFile={quoted:?}"))
            .arg("-R")
            .arg(format!("{}:127.0.0.1:{port}", service.remote_port));
        if service.name == "pinggy.io" {
            command.arg("-t");
        }
        command.arg(service.host);
        if service.name == "pinggy.io" {
            command.arg("x:xff");
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("install OpenSSH (ssh), or use --tailscale / --url")?;
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let startup = async {
            let mut output = String::new();
            let mut out = [0; 4096];
            let mut err = [0; 4096];
            let mut out_open = true;
            let mut err_open = true;
            while out_open || err_open {
                let bytes = tokio::select! {
                    count = stdout.read(&mut out), if out_open => {
                        let n = count?;
                        out_open = n != 0;
                        out[..n].to_vec()
                    }
                    count = stderr.read(&mut err), if err_open => {
                        let n = count?;
                        err_open = n != 0;
                        err[..n].to_vec()
                    }
                };
                output.push_str(&String::from_utf8_lossy(&bytes));
                ensure!(output.len() < 65536, "tunnel startup output exceeded limit");
                if let Some(url) = discover(service, &output) {
                    return Ok(url);
                }
            }
            anyhow::bail!("tunnel exited before announcing a URL")
        };
        match tokio::time::timeout(std::time::Duration::from_secs(10), startup).await {
            Ok(Ok(url)) => {
                // Keep draining provider output without rendering untrusted messages.
                eunha::tenants::spawn(async move {
                    let _ = tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await;
                });
                eunha::tenants::spawn(async move {
                    let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
                });
                return Ok((url, child));
            }
            _ => {
                let _ = child.kill().await;
                eprintln!(
                    "{} did not establish a tunnel; trying the next available provider.",
                    service.name
                );
            }
        }
    }
    anyhow::bail!(
        "no tunnel provider connected; retry, select --tunnel-service, or use --tailscale / --url"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_only_provider_urls() {
        assert_eq!(
            discover(
                &SERVICES[0],
                "Ready: https://0123456789abcdef.fedify.com.es/\n"
            )
            .as_deref(),
            Some("https://0123456789abcdef.fedify.com.es")
        );
        assert!(discover(
            &SERVICES[0],
            "https://0123456789abcdef.fedify.com.es.evil.example/"
        )
        .is_none());
        assert!(discover(&SERVICES[0], "https://evil.example/").is_none());
        assert!(discover(&SERVICES[1], "https://trial.serveousercontent.com/").is_some());
        assert!(discover(&SERVICES[2], "https://trial.free.pinggy.net/").is_some());
    }
}
