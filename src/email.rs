#[derive(Clone)]
pub struct EmailSender {
    smtp: Option<lettre::AsyncSmtpTransport<lettre::Tokio1Executor>>,
    http: reqwest::Client,
    api_key: String,
    from: String,
}

impl EmailSender {
    pub fn new(http: reqwest::Client, api_key: String, from: String) -> Self {
        Self {
            smtp: None,
            http,
            api_key,
            from,
        }
    }

    pub fn with_smtp(mut self, config: &crate::config::SmtpConfig) -> anyhow::Result<Self> {
        use lettre::{
            transport::smtp::authentication::Credentials, AsyncSmtpTransport, Tokio1Executor,
        };
        let transport = match config.port {
            465 => AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host),
            587 => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host),
            _ => anyhow::bail!("SMTP requires port 465 or 587"),
        }
        .map_err(|_| anyhow::anyhow!("Invalid SMTP host"))?;
        config
            .from
            .parse::<lettre::message::Mailbox>()
            .map_err(|_| anyhow::anyhow!("Invalid SMTP sender"))?;
        self.smtp = Some(
            transport
                .credentials(Credentials::new(
                    config.username.clone(),
                    config.password.clone(),
                ))
                .timeout(Some(std::time::Duration::from_secs(30)))
                .build(),
        );
        self.from = config.from.clone();
        Ok(self)
    }

    /// `code` — when non-empty, displayed prominently for manual entry.
    ///          Leave empty when only the confirmation link is needed.
    pub async fn send_confirmation(
        &self,
        to: &str,
        name: &str,
        code: &str,
        confirm_url: &str,
        locale: &str,
    ) -> anyhow::Result<()> {
        let code_block = if code.is_empty() {
            String::new()
        } else if locale == "ko" {
            format!("<p>인증 코드: <strong style=\"font-size:1.5em;letter-spacing:0.15em\">{code}</strong></p>")
        } else {
            format!("<p>Your confirmation code: <strong style=\"font-size:1.5em;letter-spacing:0.15em\">{code}</strong></p>")
        };

        let (subject, body) = if locale == "ko" {
            (
                "이메일 주소를 인증해 주세요".to_string(),
                format!(
                    "<p>안녕하세요 {name},</p>\
                     {code_block}\
                     <p>또는 아래 링크를 클릭하여 자동으로 인증하세요.</p>\
                     <p><a href=\"{confirm_url}\">{confirm_url}</a></p>"
                ),
            )
        } else {
            (
                "Confirm your email address".to_string(),
                format!(
                    "<p>Hi {name},</p>\
                     {code_block}\
                     <p>Or click the link below to confirm automatically.</p>\
                     <p><a href=\"{confirm_url}\">{confirm_url}</a></p>"
                ),
            )
        };

        self.send(to, &subject, &body).await
    }

    pub async fn send_password_reset(
        &self,
        to: &str,
        name: &str,
        reset_url: &str,
        locale: &str,
    ) -> anyhow::Result<()> {
        let (subject, body) = if locale == "ko" {
            (
                "비밀번호 재설정".to_string(),
                format!(
                    "<p>안녕하세요 {name},</p>\
                     <p>아래 링크를 클릭하여 비밀번호를 재설정하세요. 이 링크는 1시간 동안 유효합니다.</p>\
                     <p><a href=\"{reset_url}\">{reset_url}</a></p>\
                     <p>비밀번호 재설정을 요청하지 않으셨다면 이 메일을 무시하세요.</p>"
                ),
            )
        } else {
            (
                "Reset your password".to_string(),
                format!(
                    "<p>Hi {name},</p>\
                     <p>Click the link below to reset your password. This link expires in 1 hour.</p>\
                     <p><a href=\"{reset_url}\">{reset_url}</a></p>\
                     <p>If you did not request a password reset, ignore this email.</p>"
                ),
            )
        };

        self.send(to, &subject, &body).await
    }

    pub async fn send_notification(
        &self,
        to: &str,
        name: &str,
        notification_type: &str,
        actor: &str,
        instance_url: &str,
        locale: &str,
    ) -> anyhow::Result<()> {
        let (subject, body) = match (locale, notification_type) {
            ("ko", "mention") => (
                format!("{actor}님이 회원님을 멘션했습니다"),
                format!("<p>안녕하세요 {name},</p><p><strong>{actor}</strong>님이 게시물에서 회원님을 멘션했습니다.</p><p><a href=\"{instance_url}\">{instance_url}</a>에서 확인하세요.</p>"),
            ),
            ("ko", "follow") => (
                format!("{actor}님이 회원님을 팔로우했습니다"),
                format!("<p>안녕하세요 {name},</p><p><strong>{actor}</strong>님이 회원님을 팔로우하기 시작했습니다.</p><p><a href=\"{instance_url}\">{instance_url}</a>에서 확인하세요.</p>"),
            ),
            ("ko", "favourite") => (
                format!("{actor}님이 회원님의 게시물을 좋아합니다"),
                format!("<p>안녕하세요 {name},</p><p><strong>{actor}</strong>님이 회원님의 게시물을 즐겨찾기했습니다.</p><p><a href=\"{instance_url}\">{instance_url}</a>에서 확인하세요.</p>"),
            ),
            ("ko", "reblog") => (
                format!("{actor}님이 회원님의 게시물을 부스트했습니다"),
                format!("<p>안녕하세요 {name},</p><p><strong>{actor}</strong>님이 회원님의 게시물을 부스트했습니다.</p><p><a href=\"{instance_url}\">{instance_url}</a>에서 확인하세요.</p>"),
            ),
            (_, "mention") => (
                format!("{actor} mentioned you"),
                format!("<p>Hi {name},</p><p><strong>{actor}</strong> mentioned you in a post.</p><p>Visit <a href=\"{instance_url}\">{instance_url}</a> to see it.</p>"),
            ),
            (_, "follow") => (
                format!("{actor} followed you"),
                format!("<p>Hi {name},</p><p><strong>{actor}</strong> started following you.</p><p>Visit <a href=\"{instance_url}\">{instance_url}</a> to see their profile.</p>"),
            ),
            (_, "favourite") => (
                format!("{actor} liked your post"),
                format!("<p>Hi {name},</p><p><strong>{actor}</strong> favourited your post.</p><p>Visit <a href=\"{instance_url}\">{instance_url}</a> to see it.</p>"),
            ),
            (_, "reblog") => (
                format!("{actor} boosted your post"),
                format!("<p>Hi {name},</p><p><strong>{actor}</strong> boosted your post.</p><p>Visit <a href=\"{instance_url}\">{instance_url}</a> to see it.</p>"),
            ),
            _ => return Ok(()),
        };

        self.send(to, &subject, &body).await
    }

    /// Tell an administrator that newer Mastodon releases exist than the one
    /// this build implements.
    ///
    /// Mastodon's `AdminMailer#new_software_updates` and
    /// `#new_critical_software_updates`. The wording differs because the
    /// subject is not eunha's own version: eunha reproduces a Mastodon
    /// release's schema and API, and it is that release which has been
    /// superseded.
    pub async fn send_software_updates(
        &self,
        to: &str,
        name: &str,
        instance_domain: &str,
        tracked_version: &str,
        versions: &[String],
        urgent: bool,
    ) -> anyhow::Result<()> {
        let listed = versions
            .iter()
            .map(|v| format!("<li>Mastodon {v}</li>"))
            .collect::<String>();

        let subject = if urgent {
            format!("[{instance_domain}] Critical Mastodon updates available")
        } else {
            format!("[{instance_domain}] Mastodon updates available")
        };

        let urgency = if urgent {
            "<p><strong>At least one of these is marked urgent.</strong></p>"
        } else {
            ""
        };

        let body = format!(
            "<p>Hi {name},</p>\
             <p>{instance_domain} runs eunha, which implements Mastodon \
             {tracked_version}. Newer Mastodon releases are available:</p>\
             <ul>{listed}</ul>{urgency}\
             <p>Adopting one of them means a newer eunha, not a Mastodon \
             upgrade. Nothing here updates itself.</p>"
        );

        self.send(to, &subject, &body).await
    }

    /// Tell an administrator that the Mastodon release this build implements is
    /// losing, or has lost, upstream support.
    ///
    /// Mastodon's `AdminMailer#end_of_support_*`. `days_remaining` is negative
    /// once the date has passed.
    pub async fn send_end_of_support(
        &self,
        to: &str,
        name: &str,
        instance_domain: &str,
        branch: &str,
        end_of_support: &str,
        days_remaining: i64,
    ) -> anyhow::Result<()> {
        let (subject, urgency) = if days_remaining < 0 {
            (
                format!("[{instance_domain}] Mastodon {branch} is out of support"),
                format!(
                    "<p>Support for Mastodon {branch} ended on {end_of_support}. It no \
                     longer receives fixes, including security fixes.</p>"
                ),
            )
        } else {
            (
                format!("[{instance_domain}] Mastodon {branch} loses support soon"),
                format!(
                    "<p>Support for Mastodon {branch} ends on {end_of_support}, in \
                     {days_remaining} days.</p>"
                ),
            )
        };

        let body = format!(
            "<p>Hi {name},</p>\
             <p>{instance_domain} runs eunha, which implements Mastodon \
             {branch}.</p>{urgency}\
             <p>An eunha that tracks a supported Mastodon release is the way \
             out of this; see the project's release notes.</p>"
        );

        self.send(to, &subject, &body).await
    }

    /// Mastodon's `UserMailer#warning`: a strike, told to the account it is
    /// against. `action` is the `AccountWarning#action` key, `text` the
    /// moderator's (already escaped) explanation, `reason` the report category
    /// and the rules it cited.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_warning(
        &self,
        to: &str,
        acct: &str,
        instance_domain: &str,
        action: &str,
        text: &str,
        reason: Option<(&str, &[String])>,
        cited_statuses: &[String],
    ) -> anyhow::Result<()> {
        let subject = match action {
            "delete_statuses" => format!("Your posts on {acct} have been removed"),
            "disable" => format!("Your account {acct} has been frozen"),
            "mark_statuses_as_sensitive" => {
                format!("Your posts on {acct} have been marked as sensitive")
            }
            "sensitive" => format!("Your posts on {acct} will be marked as sensitive from now on"),
            "silence" => format!("Your account {acct} has been limited"),
            "suspend" => format!("Your account {acct} has been suspended"),
            _ => format!("Warning for {acct}"),
        };
        let explanation = match action {
            "delete_statuses" => format!(
                "Some of your posts have been found to violate one or more community \
                 guidelines and have been subsequently removed by the moderators of \
                 {instance_domain}."
            ),
            "disable" => "You can no longer use your account, but your profile and other \
                          data remains intact. You can request a backup of your data, change \
                          account settings or delete your account."
                .to_string(),
            "mark_statuses_as_sensitive" => format!(
                "Some of your posts have been marked as sensitive by the moderators of \
                 {instance_domain}. This means that people will need to tap the media in \
                 the posts before a preview is displayed. You can mark media as sensitive \
                 yourself when posting in the future."
            ),
            "sensitive" => "From now on, all your uploaded media files will be marked as \
                            sensitive and hidden behind a click-through warning."
                .to_string(),
            "silence" => "You can still use your account but only people who are already \
                          following you will see your posts on this server, and you may be \
                          excluded from various discovery features. However, others may \
                          still manually follow you."
                .to_string(),
            "suspend" => "You can no longer use your account, and your profile and other \
                          data are no longer accessible. You can still login to request a \
                          backup of your data until the data is fully removed in about 30 \
                          days, but we will retain some basic data to prevent you from \
                          evading the suspension."
                .to_string(),
            _ => String::new(),
        };
        let mut body = String::new();
        if !explanation.is_empty() {
            body.push_str(&format!("<p>{explanation}</p>"));
        }
        if !text.is_empty() {
            body.push_str(&format!("<p>{text}</p>"));
        }
        if let Some((category, rules)) = reason {
            let label = match category {
                "spam" => "Spam",
                "violation" => "Content violates the following community guidelines",
                _ => category,
            };
            body.push_str(&format!("<p><strong>Reason:</strong> {label}</p>"));
            if !rules.is_empty() {
                body.push_str("<ul>");
                for rule in rules {
                    body.push_str(&format!("<li>{}</li>", html_escape(rule)));
                }
                body.push_str("</ul>");
            }
        }
        if !cited_statuses.is_empty() {
            body.push_str("<p><strong>Posts cited:</strong></p><ul>");
            for url in cited_statuses {
                let url = html_escape(url);
                body.push_str(&format!("<li><a href=\"{url}\">{url}</a></li>"));
            }
            body.push_str("</ul>");
        }
        body.push_str(&format!(
            "<p>If you believe this is an error, you can submit an appeal to the staff \
             of {instance_domain}.</p>"
        ));
        self.send(to, &subject, &body).await
    }

    /// Mastodon's `AdminMailer#new_report`.
    pub async fn send_new_report(
        &self,
        to: &str,
        instance_domain: &str,
        report_id: i64,
        reporter: Option<&str>,
        reporter_domain: Option<&str>,
        target: &str,
    ) -> anyhow::Result<()> {
        let subject = format!("New report for {instance_domain} (#{report_id})");
        let line = match (reporter, reporter_domain) {
            (Some(reporter), _) => format!(
                "{} has reported {}",
                html_escape(reporter),
                html_escape(target)
            ),
            (None, Some(domain)) => {
                format!(
                    "Someone from {} has reported {}",
                    html_escape(domain),
                    html_escape(target)
                )
            }
            (None, None) => format!("Someone has reported {}", html_escape(target)),
        };
        let url = format!("https://{instance_domain}/admin/reports/{report_id}");
        let body = format!("<p>{line}</p><p><a href=\"{url}\">{url}</a></p>");
        self.send(to, &subject, &body).await
    }

    /// Mastodon's `AdminMailer#new_pending_account`.
    pub async fn send_new_pending_account(
        &self,
        to: &str,
        instance_domain: &str,
        account_id: i64,
        username: &str,
        invite_request: Option<&str>,
    ) -> anyhow::Result<()> {
        let subject = format!("New account up for review on {instance_domain} ({username})");
        let url = format!("https://{instance_domain}/admin/accounts/{account_id}");
        let reason = invite_request
            .filter(|r| !r.is_empty())
            .map(|r| format!("<blockquote>{}</blockquote>", html_escape(r)))
            .unwrap_or_default();
        let body = format!(
            "<p>The details of the new account are below. You can approve or reject this \
             application.</p><p><strong>{}</strong></p>{reason}<p><a href=\"{url}\">{url}</a></p>",
            html_escape(username)
        );
        self.send(to, &subject, &body).await
    }

    /// `AdminMailer#new_trends`.
    pub async fn send_new_trends(
        &self,
        to: &str,
        instance_domain: &str,
        requested: &crate::trends::Requested,
    ) -> anyhow::Result<()> {
        let subject = format!("New trends up for review on {instance_domain}");
        let section = |title: &str, items: &[crate::trends::ReviewItem], path: &str| {
            if items.is_empty() {
                return String::new();
            }
            let list: String = items
                .iter()
                .map(|item| {
                    format!(
                        "<li>{}<br>{}</li>",
                        html_escape(&item.label),
                        html_escape(&item.detail)
                    )
                })
                .collect();
            let url = format!("https://{instance_domain}{path}");
            format!("<h2>{title}</h2><ul>{list}</ul><p>View: <a href=\"{url}\">{url}</a></p>")
        };
        let body =
            format!(
            "<p>The following items need a review before they can be displayed publicly:</p>{}{}{}",
            section("Trending links", &requested.links, "/admin/trends/links"),
            section(
                "Trending hashtags",
                &requested.tags,
                "/admin/trends/tags?status=pending_review"
            ),
            section("Trending posts", &requested.statuses, "/admin/trends/statuses"),
        );
        self.send(to, &subject, &body).await
    }

    async fn send(&self, to: &str, subject: &str, html: &str) -> anyhow::Result<()> {
        if let Some(smtp) = &self.smtp {
            use lettre::AsyncTransport;
            let message = lettre::Message::builder()
                .from(
                    self.from
                        .parse()
                        .map_err(|_| anyhow::anyhow!("Invalid SMTP sender"))?,
                )
                .to(to
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid email recipient"))?)
                .subject(subject)
                .header(lettre::message::header::ContentType::TEXT_HTML)
                .body(html.to_owned())
                .map_err(|_| anyhow::anyhow!("Could not build email"))?;
            smtp.send(message)
                .await
                .map_err(|_| anyhow::anyhow!("SMTP delivery failed"))?;
            return Ok(());
        }
        let payload = serde_json::json!({
            "from": self.from,
            "to": [to],
            "subject": subject,
            "html": html,
        });
        let resp = self
            .http
            .post("https://api.resend.com/emails")
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("Resend email delivery failed (HTTP {})", resp.status());
        }
        Ok(())
    }
}

/// Escape text for an HTML email body.
pub fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod smtp_tests {
    use super::*;

    #[tokio::test]
    async fn smtp_overrides_resend_and_requires_tls_ports() {
        let mut config = crate::config::SmtpConfig {
            host: "smtp.example.com".into(),
            port: 587,
            username: "private-login".into(),
            password: "private-password".into(),
            from: "mail@example.com".into(),
        };
        let sender = EmailSender::new(
            reqwest::Client::new(),
            "resend-secret".into(),
            "fallback@example.com".into(),
        )
        .with_smtp(&config)
        .unwrap();
        assert!(sender.smtp.is_some());
        assert_eq!(sender.from, "mail@example.com");
        config.port = 465;
        assert!(
            EmailSender::new(reqwest::Client::new(), String::new(), String::new())
                .with_smtp(&config)
                .is_ok()
        );
        config.port = 25;
        assert!(
            EmailSender::new(reqwest::Client::new(), String::new(), String::new())
                .with_smtp(&config)
                .is_err()
        );
    }
}
