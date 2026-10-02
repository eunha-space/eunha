use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static CAPTURE: AtomicBool = AtomicBool::new(false);

/// Keep what every sender created from now on without SMTP would have sent,
/// instead of failing, so integration tests can read the mail an instance
/// sent with [`EmailSender::sent`]. Each sender keeps its own, so instances
/// that share a process do not see each other's.
pub fn capture_for_tests() {
    CAPTURE.store(true, Ordering::Relaxed);
}

/// A mail kept by [`capture_for_tests`].
#[derive(Debug, Clone)]
pub struct SentMail {
    pub to: String,
    pub subject: String,
    pub html: String,
    pub headers: Vec<(String, String)>,
}

impl SentMail {
    /// The value of header `name`, if the mail carried one.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone)]
pub struct EmailSender {
    smtp: Option<lettre::AsyncSmtpTransport<lettre::Tokio1Executor>>,
    from: String,
    outbox: Option<Arc<Mutex<Vec<SentMail>>>>,
}

impl EmailSender {
    pub fn new(config: Option<&crate::config::SmtpConfig>) -> anyhow::Result<Self> {
        let sender = Self {
            smtp: None,
            from: String::new(),
            outbox: (config.is_none() && CAPTURE.load(Ordering::Relaxed))
                .then(|| Arc::new(Mutex::new(Vec::new()))),
        };
        match config {
            Some(config) => sender.with_smtp(config),
            None => Ok(sender),
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

    /// `UserMailer#confirmation_instructions` for a pending reconfirmation:
    /// the `reconfirmation_instructions` template, to the new address.
    pub async fn send_reconfirmation_instructions(
        &self,
        to: &str,
        instance_domain: &str,
        confirm_url: &str,
    ) -> anyhow::Result<()> {
        let url = html_escape(confirm_url);
        let body = format!(
            "<h1>Verify email address</h1>\
             <p>Confirm the new address to change your email.</p>\
             <p><a href=\"{url}\">Verify email address</a></p>\
             <p>If this change wasn't initiated by you, please ignore this email. The email \
             address for the Mastodon account won't change until you access the link above.</p>"
        );
        self.send(
            to,
            &format!("Mastodon: Confirm email for {instance_domain}"),
            &body,
        )
        .await
    }

    /// Devise's `reset_password_instructions`, in Mastodon's words.
    pub async fn send_password_reset(
        &self,
        to: &str,
        name: &str,
        reset_url: &str,
        locale: &str,
    ) -> anyhow::Result<()> {
        let name = html_escape(name);
        let (subject, body) = if locale == "ko" {
            (
                "Mastodon: 비밀번호 재설정 안내".to_string(),
                format!(
                    "<h1>비밀번호 재설정</h1>\
                     <p>안녕하세요 {name},</p>\
                     <p>계정의 새 비밀번호를 요청하셨습니다.</p>\
                     <p><a href=\"{reset_url}\">비밀번호 변경</a></p>\
                     <p>요청하지 않으셨다면 이 메일을 무시하세요. 위 링크로 들어가 새 비밀번호를 \
                     만들기 전까지 비밀번호는 바뀌지 않습니다.</p>"
                ),
            )
        } else {
            (
                "Mastodon: Reset password instructions".to_string(),
                format!(
                    "<h1>Password reset</h1>\
                     <p>Hi {name},</p>\
                     <p>You requested a new password for your account.</p>\
                     <p><a href=\"{reset_url}\">Change password</a></p>\
                     <p>If you didn't request this, please ignore this email. Your password won't \
                     change until you access the link above and create a new one.</p>"
                ),
            )
        };

        self.send(to, &subject, &body).await
    }

    /// `NotificationMailer`: `mention`, `quote`, `follow`, `favourite`,
    /// `reblog` and `follow_request`, with the list headers that let a mail
    /// client unsubscribe in one click and, for a post, the conversation
    /// headers that thread it.
    pub async fn send_notification_mail(&self, mail: &NotificationMail) -> anyhow::Result<()> {
        let ko = mail.locale.starts_with("ko");
        let name = mail.actor_acct.as_str();
        // `notification_mailer.<type>.{subject,title,body}` and the button.
        let (subject, title, body, action) = match (mail.kind, ko) {
            ("mention", false) => (
                format!("You were mentioned by {name}"),
                "New mention",
                format!("You were mentioned by {name} in:"),
                "Reply",
            ),
            ("mention", true) => (
                format!("{name} 님의 멘션"),
                "새 답글",
                format!("{name} 님이 나를 멘션했습니다:"),
                "답장",
            ),
            ("quote", false) => (
                format!("{name} quoted your post"),
                "New quote",
                format!("Your post was quoted by {name}:"),
                "Reply",
            ),
            ("quote", true) => (
                format!("{name} 님이 내 게시물을 인용했습니다"),
                "새 인용",
                format!("당신의 게시물을 {name} 님이 인용했습니다:"),
                "답장",
            ),
            ("follow", false) => (
                format!("{name} is now following you"),
                "New follower",
                format!("{name} is now following you!"),
                "View profile",
            ),
            ("follow", true) => (
                format!("{name} 님이 나를 팔로우했습니다"),
                "새 팔로워",
                format!("{name} 님이 나를 팔로우했습니다!"),
                "프로필 보기",
            ),
            ("favourite", false) => (
                format!("{name} favorited your post"),
                "New favorite",
                format!("Your post was favorited by {name}:"),
                "View post",
            ),
            ("favourite", true) => (
                format!("{name} 님이 내 게시물을 마음에 들어했습니다"),
                "새 좋아요",
                format!("당신의 게시물을 {name} 님이 마음에 들어했습니다:"),
                "게시물 보기",
            ),
            ("reblog", false) => (
                format!("{name} boosted your post"),
                "New boost",
                format!("Your post was boosted by {name}:"),
                "View post",
            ),
            ("reblog", true) => (
                format!("{name} 님이 내 게시물을 부스트 했습니다"),
                "새 부스트",
                format!("당신의 게시물을 {name} 님이 부스트 했습니다:"),
                "게시물 보기",
            ),
            ("follow_request", false) => (
                format!("Pending follower: {name}"),
                "New follow request",
                format!("{name} has requested to follow you"),
                "Manage follow requests",
            ),
            ("follow_request", true) => (
                format!("{name} 님이 보낸 팔로우 요청"),
                "새 팔로우 요청",
                format!("{name} 님이 내게 팔로우 요청을 보냈습니다"),
                "팔로우 요청 관리",
            ),
            _ => return Ok(()),
        };
        let card = match (&mail.status, &mail.account) {
            (Some(status), _) => mailed_status_card(status),
            (None, Some(account)) => format!(
                "<table role=\"presentation\" width=\"100%\"><tr>\
                 <td width=\"48\"><img src=\"{avatar}\" alt=\"\" width=\"48\" height=\"48\"></td>\
                 <td><strong>{name}</strong><br>@{acct}</td></tr></table>",
                avatar = html_escape(&account.avatar_url),
                name = html_escape(&account.name),
                acct = html_escape(&account.acct),
            ),
            (None, None) => String::new(),
        };
        let (preferences, unsubscribe) = if ko {
            ("이메일 설정 변경", "구독 해제")
        } else {
            ("Change email preferences", "Unsubscribe")
        };
        let html = format!(
            "<h1>{title}</h1><p>{body}</p>{card}\
             <p><a href=\"{button_url}\">{action}</a></p><hr>\
             <p><small><a href=\"{preferences_url}\">{preferences}</a> · \
             <a href=\"{unsubscribe_url}\">{unsubscribe}</a></small></p>",
            body = html_escape(&body),
            button_url = html_escape(&mail.button_url),
            preferences_url = html_escape(&mail.preferences_url),
            unsubscribe_url = html_escape(&mail.unsubscribe_url),
        );
        let mut headers = vec![
            // `ApplicationMailer#set_autoreply_headers!`
            ("Auto-Submitted", "auto-generated".to_owned()),
            ("Precedence", "list".to_owned()),
            ("X-Auto-Response-Suppress", "All".to_owned()),
            // `set_list_headers!`
            ("List-ID", mail.list_id.clone()),
            ("List-Unsubscribe-Post", "List-Unsubscribe=One-Click".into()),
            ("List-Unsubscribe", format!("<{}>", mail.unsubscribe_url)),
        ];
        // `thread_by_conversation!`
        if let Some(thread) = &mail.conversation_message_id {
            headers.push(("In-Reply-To", thread.clone()));
            headers.push(("References", thread.clone()));
        }
        self.send_with_headers(&mail.to, &subject, &html, &headers)
            .await
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
    /// moderator's explanation as `linkify` writes it, `reason` the report category
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
        body.push_str(text);
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

    /// `AdminMailer#auto_close_registrations`.
    pub async fn send_auto_close_registrations(
        &self,
        to: &str,
        instance_domain: &str,
    ) -> anyhow::Result<()> {
        let instance = html_escape(instance_domain);
        let body = format!(
            "<p>Due to a lack of recent moderator activity, registrations on {instance} have \
             been automatically switched to requiring manual review, to prevent {instance} \
             from being used as a platform for potential bad actors. You can switch it back to \
             open registrations at any time.</p>"
        );
        self.send(
            to,
            &format!(
                "Registrations for {instance_domain} have been automatically switched to \
                 requiring approval"
            ),
            &body,
        )
        .await
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

    /// The address these emails come from, which the page confirming a
    /// subscription asks the subscriber to add to their contacts.
    pub fn from_address(&self) -> &str {
        &self.from
    }

    /// `EmailSubscriptionMailer#confirmation`.
    pub async fn send_subscription_confirmation(
        &self,
        envelope: &SubscriptionEnvelope,
        acct: &str,
        avatar_url: &str,
        confirm_url: &str,
    ) -> anyhow::Result<()> {
        let ko = envelope.locale.starts_with("ko");
        let name = html_escape(&envelope.name);
        let subject = if ko {
            "이메일 주소 확인"
        } else {
            "Confirm your email address"
        };
        let title = if ko {
            format!("{name} 님으로부터 이메일 업데이트를 받겠습니까?")
        } else {
            format!("Get email updates from {name}?")
        };
        let action = if ko {
            "이메일 주소 확인"
        } else {
            "Confirm email address"
        };
        let confirm_url = html_escape(confirm_url);
        let body = format!(
            "<p><img src=\"{avatar}\" alt=\"\" width=\"64\" height=\"64\"></p>\
             <h1>{title}</h1>\
             <p>Confirm you'd like to receive emails from {name} (@{acct}) when they \
             publish new posts.</p>\
             <p><a href=\"{confirm_url}\">{action}</a></p>\
             <p>If you're not sure why you received this email, you can delete it. You \
             will not be subscribed if you don't click on the link above.</p>{footer}",
            avatar = html_escape(avatar_url),
            acct = html_escape(acct),
            footer = subscription_footer(envelope),
        );
        self.send_with_headers(&envelope.to, subject, &body, &list_headers(envelope))
            .await
    }

    /// `EmailSubscriptionMailer#notification`: `posts`, newest first, and the
    /// invitation to an account here. `raw_name` is the account's display
    /// name as the subject uses it, blank or not; `excerpt` the newest post's
    /// text, truncated.
    pub async fn send_subscription_notification(
        &self,
        envelope: &SubscriptionEnvelope,
        raw_name: &str,
        excerpt: &str,
        posts: &[MailedStatus],
        sign_up_url: &str,
    ) -> anyhow::Result<()> {
        let ko = envelope.locale.starts_with("ko");
        let subject = match (posts.len() == 1, ko) {
            (true, false) => format!("New post: \"{excerpt}\""),
            (true, true) => format!("새 게시물: \"{excerpt}\""),
            (false, false) => format!("New posts from {raw_name}"),
            (false, true) => format!("{raw_name}의 새 게시물"),
        };
        let mut body = String::new();
        for post in posts {
            body.push_str(&mailed_status_card(post));
            body.push_str("<hr>");
        }
        let interact = if posts.len() == 1 {
            "Interact with this post and discover more like it."
        } else {
            "Interact with these posts and discover more."
        };
        let create_account = if ko {
            "마스토돈 계정 생성"
        } else {
            "Create a Mastodon account"
        };
        body.push_str(&format!(
            "<p>{interact}</p><p><a href=\"{}\">{create_account}</a></p>{}",
            html_escape(sign_up_url),
            subscription_footer(envelope),
        ));
        self.send_with_headers(&envelope.to, &subject, &body, &list_headers(envelope))
            .await
    }

    /// `UserMailer#terms_of_service_changed`. `date` is the version's usable
    /// effective date as Rails' `l` formats it, `url` its page, and
    /// `changelog_html` its changelog already rendered from Markdown.
    pub async fn send_terms_of_service_changed(
        &self,
        to: &str,
        instance_domain: &str,
        date: &str,
        url: &str,
        changelog_html: &str,
    ) -> anyhow::Result<()> {
        let domain = html_escape(instance_domain);
        let url = html_escape(url);
        let body = format!(
            "<h1>Important update</h1>\
             <p>The terms of service of {domain} are changing</p>\
             <p>You are receiving this e-mail because we're making some changes to our terms \
             of service at {domain}. These updates will take effect on <strong>{date}</strong>. \
             We encourage you to review the <a href=\"{url}\" target=\"_blank\">updated terms \
             in full here</a>.</p>\
             <p><strong>At a glance, here is what this update means for you:</strong></p>\
             {changelog_html}\
             <p>By continuing to use {domain}, you are agreeing to these terms. If you disagree \
             with the updated terms, you may terminate your agreement with {domain} at any time \
             by deleting your account.</p>\
             <p>The {domain} team</p>"
        );
        self.send(to, "Updates to our terms of service", &body)
            .await
    }

    /// `UserMailer#announcement_published`: the announcement's text, as
    /// `linkify` writes it.
    pub async fn send_announcement_published(
        &self,
        to: &str,
        instance_domain: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let domain = html_escape(instance_domain);
        let body = format!(
            "<h1>{domain} service announcement</h1>\
             <p>The administrators of {domain} are making an announcement:</p>\
             {text}"
        );
        self.send(to, "Service announcement", &body).await
    }

    /// `UserMailer#appeal_approved` (`approved` true) and
    /// `UserMailer#appeal_rejected`. The dates are `l(...)` in Mastodon's
    /// `default` format in UTC for the subject and `with_time_zone` in the
    /// user's `time_zone` for the body.
    pub async fn send_appeal_decided(
        &self,
        to: &str,
        instance_domain: &str,
        approved: bool,
        appeal_created_at: chrono::NaiveDateTime,
        strike_created_at: chrono::NaiveDateTime,
        time_zone: Option<&str>,
    ) -> anyhow::Result<()> {
        use crate::time_zones::format_with_time_zone;
        let date = appeal_created_at.format("%b %d, %Y, %H:%M");
        let appeal_date = format_with_time_zone(appeal_created_at.and_utc(), time_zone, "en");
        let strike_date = format_with_time_zone(strike_created_at.and_utc(), time_zone, "en");
        let (subject, title, explanation) = if approved {
            (
                format!("Your appeal from {date} has been approved"),
                "Appeal approved",
                format!(
                    "The appeal of the strike against your account on {strike_date} that you \
                     submitted on {appeal_date} has been approved. Your account is once again \
                     in good standing."
                ),
            )
        } else {
            (
                format!("Your appeal from {date} has been rejected"),
                "Appeal rejected",
                format!(
                    "The appeal of the strike against your account on {strike_date} that you \
                     submitted on {appeal_date} has been rejected."
                ),
            )
        };
        let url = format!("https://{instance_domain}/");
        let body =
            format!("<h1>{title}</h1><p>{explanation}</p><p><a href=\"{url}\">{url}</a></p>");
        self.send(to, &subject, &body).await
    }

    /// `AdminMailer#new_appeal`. `action` is the strike's
    /// `AccountWarning#action` key.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_new_appeal(
        &self,
        to: &str,
        instance_domain: &str,
        target: &str,
        action_taken_by: &str,
        strike_created_at: chrono::NaiveDateTime,
        action: &str,
        text: &str,
        strike_id: i64,
    ) -> anyhow::Result<()> {
        let subject = format!("{target} is appealing a moderation decision on {instance_domain}");
        // `admin_mailer.new_appeal.actions`.
        let kind = match action {
            "delete_statuses" => "to delete their posts",
            "disable" => "to freeze their account",
            "mark_statuses_as_sensitive" => "to mark their posts as sensitive",
            "sensitive" => "to mark their account as sensitive",
            "silence" => "to limit their account",
            "suspend" => "to suspend their account",
            _ => "a warning",
        };
        let date = strike_created_at.format("%b %d, %Y, %H:%M UTC");
        let url = format!("https://{instance_domain}/disputes/strikes/{strike_id}");
        let body = format!(
            "<p>{} is appealing a moderation decision by {} from {date}, which was {kind}. \
             They wrote:</p><blockquote>{}</blockquote><p>You can approve the appeal to undo \
             the moderation decision, or ignore it.</p><p>View: <a href=\"{url}\">{url}</a></p>",
            html_escape(target),
            html_escape(action_taken_by),
            html_escape(text).replace('\n', "<br>"),
        );
        self.send(to, &subject, &body).await
    }

    /// `UserMailer#two_factor_disabled`.
    pub async fn send_two_factor_disabled(
        &self,
        to: &str,
        instance_domain: &str,
    ) -> anyhow::Result<()> {
        let url = format!("https://{instance_domain}/settings");
        let body = format!(
            "<h1>2FA disabled</h1><p>Login is now possible using only e-mail address and \
             password.</p><p><a href=\"{url}\">{url}</a></p>"
        );
        self.send(to, "Mastodon: Two-factor authentication disabled", &body)
            .await
    }

    /// `UserMailer#backup_ready`. Upstream links its `/backups/{id}/download`,
    /// which needs a signed-in web session; eunha's settings are a
    /// single-page app, so this links its export page instead.
    pub async fn send_backup_ready(&self, to: &str, instance_domain: &str) -> anyhow::Result<()> {
        let url = format!("https://{instance_domain}/settings/export");
        let body = format!(
            "<h1>Archive takeout</h1><p>You requested a full backup of your Mastodon account.</p>\
             <p>It's now ready for download!</p><p><a href=\"{url}\">{url}</a></p>"
        );
        self.send(to, "Your archive is ready for download", &body)
            .await
    }

    /// One of `UserMailer`'s account-security notices: a title, the
    /// explanation under it, optionally a line of detail, and the account
    /// settings button. `notice` names the `devise.mailer.*` entry.
    pub async fn send_security_notice(
        &self,
        to: &str,
        instance_domain: &str,
        notice: SecurityNotice<'_>,
    ) -> anyhow::Result<()> {
        let (subject, title, explanation, extra): (String, &str, &str, Option<String>) =
            match notice {
                SecurityNotice::TwoFactorEnabled => (
                    "Mastodon: Two-factor authentication enabled".into(),
                    "2FA enabled",
                    "Two-factor authentication has been enabled for your account.",
                    Some(
                        "A token generated by the paired TOTP app will be required for login."
                            .into(),
                    ),
                ),
                SecurityNotice::TwoFactorRecoveryCodesChanged => (
                    "Mastodon: Two-factor recovery codes re-generated".into(),
                    "2FA recovery codes changed",
                    "The previous recovery codes have been invalidated and new ones generated.",
                    None,
                ),
                SecurityNotice::WebauthnEnabled => (
                    "Mastodon: Security key authentication enabled".into(),
                    "Security keys enabled",
                    "Security key authentication has been enabled for your account.",
                    Some("Your security key can now be used for login.".into()),
                ),
                SecurityNotice::WebauthnDisabled => (
                    "Mastodon: Authentication with security keys disabled".into(),
                    "Security keys disabled",
                    "Authentication with security keys has been disabled for your account.",
                    Some(
                        "Login is now possible using only the token generated by the paired \
                         TOTP app."
                            .into(),
                    ),
                ),
                SecurityNotice::WebauthnCredentialAdded(nickname) => (
                    "Mastodon: New security key".into(),
                    "A new security key has been added",
                    "The following security key has been added to your account",
                    Some(format!("<strong>{}</strong>", html_escape(nickname))),
                ),
                SecurityNotice::WebauthnCredentialDeleted(nickname) => (
                    "Mastodon: Security key deleted".into(),
                    "One of your security keys has been deleted",
                    "The following security key has been deleted from your account",
                    Some(format!("<strong>{}</strong>", html_escape(nickname))),
                ),
                SecurityNotice::EmailChanged(new_email) => (
                    "Mastodon: Email changed".into(),
                    "New email address",
                    "The email address for your account is being changed to:",
                    Some(format!(
                        "<strong>{}</strong></p><p>If you did not change your email, it is \
                         likely that someone has gained access to your account. Please change \
                         your password immediately or contact the server admin if you're locked \
                         out of your account.",
                        html_escape(new_email)
                    )),
                ),
                SecurityNotice::PasswordChange => (
                    "Mastodon: Password changed".into(),
                    "Password changed",
                    "The password for your account has been changed.",
                    Some(
                        "If you did not change your password, it is likely that someone has \
                         gained access to your account. Please change your password immediately \
                         or contact the server admin if you're locked out of your account."
                            .into(),
                    ),
                ),
            };
        let url = format!("https://{instance_domain}/settings");
        let extra = extra.map(|e| format!("<p>{e}</p>")).unwrap_or_default();
        let body = format!(
            "<h1>{title}</h1><p>{explanation}</p>{extra}\
             <p><a href=\"{url}\">Account settings</a></p>"
        );
        self.send(to, &subject, &body).await
    }

    /// `UserMailer#failed_2fa` (`suspicious` false) and
    /// `UserMailer#suspicious_sign_in`: where a sign-in came from.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_sign_in_alert(
        &self,
        to: &str,
        instance_domain: &str,
        suspicious: bool,
        remote_ip: &str,
        browser: &str,
        timestamp: chrono::DateTime<chrono::Utc>,
        time_zone: Option<&str>,
    ) -> anyhow::Result<()> {
        let (subject, title, explanation, details, further) = if suspicious {
            (
                "Your account has been accessed from a new IP address",
                "A new sign-in",
                "We've detected a sign-in to your account from a new IP address.",
                "Here are details of the sign-in:",
                "immediately and enable two-factor authentication to keep your account secure.",
            )
        } else {
            (
                "Second factor authentication failure",
                "Failed second factor authentication",
                "Someone has tried to sign in to your account but provided an invalid second \
                 authentication factor.",
                "Here are details of the sign-in attempt:",
                "immediately as it may be compromised.",
            )
        };
        let url = format!("https://{instance_domain}/account/password");
        let body = format!(
            "<h1>{title}</h1><p>{explanation}</p><p>{details}</p>\
             <p><strong>IP:</strong> {}<br><strong>Browser:</strong> {}<br>\
             <strong>Date:</strong> {}</p>\
             <p>If this wasn't you, we recommend that you \
             <a href=\"{url}\">change your password</a> {further}</p>",
            html_escape(remote_ip),
            html_escape(browser),
            crate::time_zones::format_with_time_zone(timestamp, time_zone, "en"),
        );
        self.send(to, subject, &body).await
    }

    async fn send(&self, to: &str, subject: &str, html: &str) -> anyhow::Result<()> {
        self.send_with_headers(to, subject, html, &[]).await
    }

    async fn send_with_headers(
        &self,
        to: &str,
        subject: &str,
        html: &str,
        headers: &[(&'static str, String)],
    ) -> anyhow::Result<()> {
        if let Some(smtp) = &self.smtp {
            use lettre::message::header::{HeaderName, HeaderValue};
            use lettre::AsyncTransport;
            let mut builder = lettre::Message::builder()
                .from(
                    self.from
                        .parse()
                        .map_err(|_| anyhow::anyhow!("Invalid SMTP sender"))?,
                )
                .to(to
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid email recipient"))?)
                .subject(subject)
                .header(lettre::message::header::ContentType::TEXT_HTML);
            for (name, value) in headers {
                builder = builder.raw_header(HeaderValue::new(
                    HeaderName::new_from_ascii_str(name),
                    value.clone(),
                ));
            }
            let message = builder
                .body(html.to_owned())
                .map_err(|_| anyhow::anyhow!("Could not build email"))?;
            smtp.send(message)
                .await
                .map_err(|_| anyhow::anyhow!("SMTP delivery failed"))?;
            return Ok(());
        }
        if let Some(outbox) = &self.outbox {
            outbox.lock().expect("outbox lock").push(SentMail {
                to: to.to_owned(),
                subject: subject.to_owned(),
                html: html.to_owned(),
                headers: headers
                    .iter()
                    .map(|(n, v)| ((*n).to_owned(), v.clone()))
                    .collect(),
            });
            return Ok(());
        }
        anyhow::bail!("SMTP is not configured")
    }

    /// What this sender kept under [`capture_for_tests`], oldest first.
    pub fn sent(&self) -> Vec<SentMail> {
        self.outbox
            .as_ref()
            .map(|o| o.lock().expect("outbox lock").clone())
            .unwrap_or_default()
    }
}

/// Escape text for an HTML email body.
pub fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// What every email to a subscriber carries besides its body.
#[derive(Debug, Clone)]
pub struct SubscriptionEnvelope {
    pub to: String,
    pub locale: String,
    /// The account's display name, or its username when that is blank.
    pub name: String,
    pub domain: String,
    /// `List-ID`, already in its angle brackets.
    pub list_id: String,
    pub unsubscribe_url: String,
    pub privacy_policy_url: String,
    /// `Setting.email_footer_text`, when an administrator set one.
    pub footer_text: Option<String>,
}

/// A post as `notification_mailer/status` shows it.
#[derive(Debug, Clone)]
pub struct MailedStatus {
    pub name: String,
    pub acct: String,
    pub avatar_url: String,
    pub spoiler_text: String,
    /// The post's HTML, as the API serves it.
    pub content: String,
    pub url: String,
    pub created_at: String,
}

/// The `notification_mailer/status` partial: the author, the content warning,
/// the post, and its date linking to it.
fn mailed_status_card(post: &MailedStatus) -> String {
    let warning = if post.spoiler_text.is_empty() {
        String::new()
    } else {
        format!(
            "<p><strong>{}</strong></p>",
            html_escape(&post.spoiler_text)
        )
    };
    format!(
        "<table role=\"presentation\" width=\"100%\"><tr>\
         <td width=\"48\"><img src=\"{avatar}\" alt=\"\" width=\"48\" height=\"48\"></td>\
         <td><strong>{name}</strong><br>@{acct}</td></tr></table>\
         {warning}<div>{content}</div>\
         <p><a href=\"{url}\">{created_at}</a></p>",
        avatar = html_escape(&post.avatar_url),
        name = html_escape(&post.name),
        acct = html_escape(&post.acct),
        content = post.content,
        url = html_escape(&post.url),
        created_at = html_escape(&post.created_at),
    )
}

/// An account as `application/mailer/account` shows it.
#[derive(Debug, Clone)]
pub struct MailedAccount {
    pub name: String,
    pub acct: String,
    pub avatar_url: String,
}

/// One `NotificationMailer` mail, ready to render.
#[derive(Debug, Clone)]
pub struct NotificationMail {
    /// `email_address_with_name(user.email, account.username)`.
    pub to: String,
    pub locale: String,
    /// The notification type, which is the mailer action.
    pub kind: &'static str,
    /// The other account's `pretty_acct`, which the subject names.
    pub actor_acct: String,
    /// The post, for `mention`, `quote`, `favourite` and `reblog`.
    pub status: Option<MailedStatus>,
    /// The account, for `follow` and `follow_request`.
    pub account: Option<MailedAccount>,
    pub button_url: String,
    /// `settings_preferences_notifications_url`.
    pub preferences_url: String,
    pub unsubscribe_url: String,
    /// `<type.username.domain>`.
    pub list_id: String,
    /// `<conversation-id.date@domain>`, for a post in a conversation.
    pub conversation_message_id: Option<String>,
}

/// `set_list_headers`: what lets a mail client offer to unsubscribe in one
/// click.
fn list_headers(envelope: &SubscriptionEnvelope) -> Vec<(&'static str, String)> {
    vec![
        ("List-ID", envelope.list_id.clone()),
        ("List-Unsubscribe-Post", "List-Unsubscribe=One-Click".into()),
        (
            "List-Unsubscribe",
            format!("<{}>", envelope.unsubscribe_url),
        ),
    ]
}

/// The footer of `email_subscription_mailer/notification`, which the
/// confirmation shares.
fn subscription_footer(envelope: &SubscriptionEnvelope) -> String {
    let mut footer = format!(
        "<p><small>You're receiving this email because you opted into email updates \
         from {name}. Don't want to receive these emails? \
         <a href=\"{unsubscribe}\">Unsubscribe</a></small></p>\
         <p><small>Emails are sent from {domain}, a server powered by Mastodon. To \
         understand how this server processes your personal data, refer to the \
         <a href=\"{privacy}\">Privacy Policy</a>.</small></p>",
        name = html_escape(&envelope.name),
        unsubscribe = html_escape(&envelope.unsubscribe_url),
        domain = html_escape(&envelope.domain),
        privacy = html_escape(&envelope.privacy_policy_url),
    );
    if let Some(text) = &envelope.footer_text {
        footer.push_str(&format!("<p><small>{}</small></p>", html_escape(text)));
    }
    footer
}

/// Which of `UserMailer`'s account-security notices to send.
#[derive(Debug, Clone, Copy)]
pub enum SecurityNotice<'a> {
    TwoFactorEnabled,
    TwoFactorRecoveryCodesChanged,
    WebauthnEnabled,
    WebauthnDisabled,
    /// The new key's nickname.
    WebauthnCredentialAdded(&'a str),
    /// The deleted key's nickname.
    WebauthnCredentialDeleted(&'a str),
    PasswordChange,
    /// `email_changed`, to the address being left, with the new one.
    EmailChanged(&'a str),
}

#[cfg(test)]
mod smtp_tests {
    use super::*;

    #[tokio::test]
    async fn smtp_requires_tls_ports_and_configuration() {
        let mut config = crate::config::SmtpConfig {
            host: "smtp.example.com".into(),
            port: 587,
            username: "private-login".into(),
            password: "private-password".into(),
            from: "mail@example.com".into(),
        };
        let sender = EmailSender::new(Some(&config)).unwrap();
        assert!(sender.smtp.is_some());
        assert_eq!(sender.from, "mail@example.com");
        config.port = 465;
        assert!(EmailSender::new(Some(&config)).is_ok());
        config.port = 25;
        assert!(EmailSender::new(Some(&config)).is_err());
        let unconfigured = EmailSender::new(None).unwrap();
        assert_eq!(
            unconfigured
                .send("mail@example.com", "subject", "body")
                .await
                .unwrap_err()
                .to_string(),
            "SMTP is not configured"
        );
    }
}
