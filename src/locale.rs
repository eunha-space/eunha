/// Two-locale support (en / ko) for server-rendered pages.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Locale {
    En,
    Ko,
}

impl Locale {
    /// Resolve locale: explicit `?lang=` param wins, then first matching tag
    /// in the `Accept-Language` header, then English.
    pub fn detect(lang_param: Option<&str>, accept_language: Option<&str>) -> Self {
        if let Some(p) = lang_param {
            match p.to_lowercase().as_str() {
                "ko" => return Self::Ko,
                "en" => return Self::En,
                _ => {}
            }
        }
        if let Some(al) = accept_language {
            for tag in al.split(',') {
                let lang = tag.trim().split(';').next().unwrap_or("").trim();
                if lang.starts_with("ko") {
                    return Self::Ko;
                }
                if lang.starts_with("en") {
                    return Self::En;
                }
            }
        }
        Self::En
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ko => "ko",
        }
    }

    pub fn t(self, key: &str) -> &'static str {
        match (self, key) {
            // ── authorize page ───────────────────────────────────────────────
            (Self::En, "sign_in_to") => "Sign in to",
            (Self::Ko, "sign_in_to") => "로그인",
            (Self::En, "authorize") => "Authorize",
            (Self::Ko, "authorize") => "인증",
            // ── doorkeeper.authorizations.new ────────────────────────────────
            (Self::En, "authorization_required") => "Authorization required",
            (Self::Ko, "authorization_required") => "승인 필요",
            (Self::En, "authorization_prompt_html") => {
                "%{client_name} would like permission to access your account. <strong>Only approve this request if you recognize and trust this source.</strong>"
            }
            (Self::Ko, "authorization_prompt_html") => {
                "%{client_name}이 계정에 접근할 권한을 요청합니다. <strong>내가 알아볼 수 있고 신뢰할 수 있는 출처의 요청인 경우에만 승인하세요.</strong>"
            }
            (Self::En, "review_permissions") => "Review permissions",
            (Self::Ko, "review_permissions") => "권한 검토",
            (Self::En, "authorize_button") => "Authorize",
            (Self::Ko, "authorize_button") => "승인",
            (Self::En, "deny_button") => "Deny",
            (Self::Ko, "deny_button") => "거부",
            (Self::En, "signed_in_as") => "Signed in as:",
            (Self::Ko, "signed_in_as") => "다음과 같이 로그인 중:",
            (Self::En, "logout") => "Logout",
            (Self::Ko, "logout") => "로그아웃",
            // `doorkeeper.grouped_scopes`.
            (Self::En, "scope_access_read") => "Read-only access",
            (Self::Ko, "scope_access_read") => "읽기 전용 권한",
            (Self::En, "scope_access_read/write") => "Read and write access",
            (Self::Ko, "scope_access_read/write") => "읽기/쓰기 권한",
            (Self::En, "scope_access_write") => "Write-only access",
            (Self::Ko, "scope_access_write") => "쓰기 전용 권한",
            (Self::En, "scope_title_accounts") => "Accounts",
            (Self::Ko, "scope_title_accounts") => "계정",
            (Self::En, "scope_title_admin/accounts") => "Administration of accounts",
            (Self::Ko, "scope_title_admin/accounts") => "계정 관리",
            (Self::En, "scope_title_admin/all") => "All administrative functions",
            (Self::Ko, "scope_title_admin/all") => "모든 관리자 기능",
            (Self::En, "scope_title_admin/reports") => "Administration of reports",
            (Self::Ko, "scope_title_admin/reports") => "신고 관리",
            (Self::En, "scope_title_all") => "Full access to your account",
            (Self::Ko, "scope_title_all") => "계정에 대한 모든 권한",
            (Self::En, "scope_title_blocks") => "Blocks",
            (Self::Ko, "scope_title_blocks") => "차단",
            (Self::En, "scope_title_bookmarks") => "Bookmarks",
            (Self::Ko, "scope_title_bookmarks") => "북마크",
            (Self::En, "scope_title_collections") => "Collections",
            (Self::Ko, "scope_title_collections") => "컬렉션",
            (Self::En, "scope_title_conversations") => "Conversations",
            (Self::Ko, "scope_title_conversations") => "대화",
            (Self::En, "scope_title_favourites") => "Favorites",
            (Self::Ko, "scope_title_favourites") => "좋아요",
            (Self::En, "scope_title_filters") => "Filters",
            (Self::Ko, "scope_title_filters") => "필터",
            (Self::En, "scope_title_follow") => "Follows, Mutes and Blocks",
            (Self::Ko, "scope_title_follow") => "팔로우, 뮤트와 차단",
            (Self::En, "scope_title_follows") => "Follows",
            (Self::Ko, "scope_title_follows") => "팔로우",
            (Self::En, "scope_title_lists") => "Lists",
            (Self::Ko, "scope_title_lists") => "리스트",
            (Self::En, "scope_title_media") => "Media attachments",
            (Self::Ko, "scope_title_media") => "첨부된 미디어",
            (Self::En, "scope_title_mutes") => "Mutes",
            (Self::Ko, "scope_title_mutes") => "뮤트",
            (Self::En, "scope_title_notifications") => "Notifications",
            (Self::Ko, "scope_title_notifications") => "알림",
            (Self::En, "scope_title_profile") => "Your profile",
            (Self::Ko, "scope_title_profile") => "내 프로필",
            (Self::En, "scope_title_push") => "Push notifications",
            (Self::Ko, "scope_title_push") => "푸시 알림",
            (Self::En, "scope_title_reports") => "Reports",
            (Self::Ko, "scope_title_reports") => "신고",
            (Self::En, "scope_title_search") => "Search",
            (Self::Ko, "scope_title_search") => "검색",
            (Self::En, "scope_title_statuses") => "Posts",
            (Self::Ko, "scope_title_statuses") => "게시물",
            (Self::En, "email") => "Email",
            (Self::Ko, "email") => "이메일",
            (Self::En, "password") => "Password",
            (Self::Ko, "password") => "비밀번호",
            (Self::En, "sign_in") => "Sign in",
            (Self::Ko, "sign_in") => "로그인",
            (Self::En, "invalid_credentials") => "Invalid email or password.",
            (Self::Ko, "invalid_credentials") => "이메일 또는 비밀번호가 올바르지 않습니다.",
            (Self::En, "check_email") => {
                "Check your email and click the confirmation link to activate your account."
            }
            (Self::Ko, "check_email") => {
                "이메일을 확인하여 인증 링크를 클릭해 계정을 활성화하세요."
            }
            // ── auth.setup ───────────────────────────────────────────────────
            (Self::En, "setup_title") => "Check your inbox",
            (Self::Ko, "setup_title") => "수신함 확인하기",
            (Self::En, "setup_email_hint") => {
                "Click the link we sent to %{email}. We'll wait right here."
            }
            (Self::Ko, "setup_email_hint") => {
                "%{email}로 보낸 링크를 클릭해 시작하세요. 기다리고 있겠습니다."
            }
            (Self::En, "setup_link_not_received") => "Didn't get a link?",
            (Self::Ko, "setup_link_not_received") => "링크를 못 받으셨나요?",
            (Self::En, "setup_below_hint") => {
                "Check your spam folder, or request another one. You can correct your email address if it's wrong."
            }
            (Self::Ko, "setup_below_hint") => {
                "스팸 폴더를 체크해보거나, 새로 요청할 수 있습니다. 이메일을 잘못 입력한 경우 수정할 수 있습니다."
            }
            (Self::En, "setup_sent") => {
                "You will receive a new email with the confirmation link in a few minutes!"
            }
            (Self::Ko, "setup_sent") => {
                "확인 링크가 담긴 이메일이 몇 분 안에 도착할것입니다!"
            }
            (Self::En, "resend_confirmation") => "Resend confirmation link",
            (Self::Ko, "resend_confirmation") => "확인 링크 다시 보내기",
            (Self::En, "no_account") => "Don't have an account?",
            (Self::Ko, "no_account") => "계정이 없으신가요?",
            (Self::En, "sign_up") => "Sign up",
            (Self::Ko, "sign_up") => "가입하기",
            // ── signup page ──────────────────────────────────────────────────
            (Self::En, "create_account") => "Create account",
            (Self::Ko, "create_account") => "계정 만들기",
            (Self::En, "username") => "Username",
            (Self::Ko, "username") => "사용자 이름",
            (Self::En, "confirm_password") => "Confirm password",
            (Self::Ko, "confirm_password") => "비밀번호 확인",
            (Self::En, "already_account") => "Already have an account?",
            (Self::Ko, "already_account") => "이미 계정이 있으신가요?",
            (Self::En, "registrations_closed") => "Registrations are currently by invite only.",
            (Self::Ko, "registrations_closed") => "이 인스턴스는 초대로만 가입할 수 있습니다.",
            (Self::En, "invite_code") => "Invite code",
            (Self::Ko, "invite_code") => "초대 코드",
            (Self::En, "continue_btn") => "Continue",
            (Self::Ko, "continue_btn") => "계속",
            // ── signup error messages ────────────────────────────────────────
            (Self::En, "err_invite_required") => "An invite code is required.",
            (Self::Ko, "err_invite_required") => "초대 코드가 필요합니다.",
            (Self::En, "err_invalid_invite") => "Invalid invite code.",
            (Self::Ko, "err_invalid_invite") => "유효하지 않은 초대 코드입니다.",
            (Self::En, "err_invite_maxed") => "This invite has reached its use limit.",
            (Self::Ko, "err_invite_maxed") => "이 초대는 사용 횟수를 초과했습니다.",
            (Self::En, "err_invite_expired") => "This invite has expired.",
            (Self::Ko, "err_invite_expired") => "이 초대는 만료되었습니다.",
            (Self::En, "err_username_chars") => {
                "Username may only contain letters, numbers, and underscores."
            }
            (Self::Ko, "err_username_chars") => {
                "사용자 이름은 영문자, 숫자, 밑줄만 사용할 수 있습니다."
            }
            (Self::En, "err_invalid_email") => "Enter a valid email address.",
            (Self::Ko, "err_invalid_email") => "유효한 이메일 주소를 입력해주세요.",
            (Self::En, "err_password_short") => "Password must be at least 8 characters.",
            (Self::Ko, "err_password_short") => "비밀번호는 8자 이상이어야 합니다.",
            (Self::En, "err_password_mismatch") => "Passwords do not match.",
            (Self::Ko, "err_password_mismatch") => "비밀번호가 일치하지 않습니다.",
            (Self::En, "err_username_taken") => "That username is already taken.",
            (Self::Ko, "err_username_taken") => "이미 사용 중인 사용자 이름입니다.",
            (Self::En, "err_email_taken") => "An account with that email already exists.",
            (Self::Ko, "err_email_taken") => "이미 사용 중인 이메일입니다.",
            (Self::En, "err_server") => "Server error. Please try again.",
            (Self::Ko, "err_server") => "서버 오류가 발생했습니다. 다시 시도해주세요.",
            // ── account pages ────────────────────────────────────────────────
            (Self::En, "account") => "Account",
            (Self::Ko, "account") => "계정",
            (Self::En, "change_password") => "Change password",
            (Self::Ko, "change_password") => "비밀번호 변경",
            (Self::En, "current_password") => "Current password",
            (Self::Ko, "current_password") => "현재 비밀번호",
            (Self::En, "new_password") => "New password",
            (Self::Ko, "new_password") => "새 비밀번호",
            (Self::En, "confirm_new_password") => "Confirm new password",
            (Self::Ko, "confirm_new_password") => "새 비밀번호 확인",
            (Self::En, "password_mismatch") => "New passwords do not match.",
            (Self::Ko, "password_mismatch") => "새 비밀번호가 일치하지 않습니다.",
            (Self::En, "sign_out") => "Sign out",
            (Self::Ko, "sign_out") => "로그아웃",
            (Self::En, "go_to_timeline") => "Go to timeline",
            (Self::Ko, "go_to_timeline") => "타임라인으로 돌아가기",
            (Self::En, "back_to_account") => "← Account",
            (Self::Ko, "back_to_account") => "← 계정",
            (Self::En, "password_changed") => "Password changed.",
            (Self::Ko, "password_changed") => "비밀번호가 변경되었습니다.",
            (Self::En, "password_error") => "Failed. Check your current password.",
            (Self::Ko, "password_error") => "실패했습니다. 현재 비밀번호를 확인해 주세요.",
            // ── account deletion (Mastodon `deletes.*`) ──────────────────────
            (Self::En, "delete_account") => "Delete account",
            (Self::Ko, "delete_account") => "계정 삭제",
            (Self::En, "delete_warning_before") => {
                "Before proceeding, please read these notes carefully:"
            }
            (Self::Ko, "delete_warning_before") => "진행하기 전, 주의사항을 꼼꼼히 읽어보세요:",
            (Self::En, "delete_warning_irreversible") => {
                "You will not be able to restore or reactivate your account"
            }
            (Self::Ko, "delete_warning_irreversible") => {
                "계정을 복구하거나 다시 사용할 수 없게 됩니다"
            }
            (Self::En, "delete_warning_username_unavailable") => {
                "Your username will remain unavailable"
            }
            (Self::Ko, "delete_warning_username_unavailable") => {
                "당신의 계정명은 앞으로 사용할 수 없습니다"
            }
            (Self::En, "delete_warning_data_removal") => {
                "Your posts and other data will be permanently removed"
            }
            (Self::Ko, "delete_warning_data_removal") => {
                "당신의 게시물과 다른 정보들은 영구적으로 삭제 됩니다"
            }
            (Self::En, "delete_warning_caches") => {
                "Content that has been cached by other servers may persist"
            }
            (Self::Ko, "delete_warning_caches") => {
                "다른 서버에 캐싱된 정보들은 남아있을 수 있습니다"
            }
            (Self::En, "delete_confirm_password") => {
                "Enter your current password to verify your identity"
            }
            (Self::Ko, "delete_confirm_password") => {
                "본인 확인을 위해 현재 사용 중인 암호를 입력해 주십시오"
            }
            (Self::En, "delete_confirm_username") => "Enter your username to confirm the procedure",
            (Self::Ko, "delete_confirm_username") => {
                "절차를 진행하려면 당신의 사용자명을 입력하세요"
            }
            (Self::En, "delete_challenge_not_passed") => {
                "The information you entered was not correct"
            }
            (Self::Ko, "delete_challenge_not_passed") => "입력한 정보가 올바르지 않습니다",
            (Self::En, "delete_success") => "Your account was successfully deleted",
            (Self::Ko, "delete_success") => "계정이 성공적으로 삭제되었습니다",
            // ── signup approval ──────────────────────────────────────────────
            (Self::En, "reason") => "Why do you want to join?",
            (Self::Ko, "reason") => "가입 이유를 알려주세요",
            (Self::En, "reason_hint") => "Tell us a bit about yourself.",
            (Self::Ko, "reason_hint") => "간단히 소개해 주세요.",
            (Self::En, "pending_approval") => {
                "Your account is pending approval. You will be notified once approved."
            }
            (Self::Ko, "pending_approval") => "계정 승인 대기 중입니다. 승인되면 알려드리겠습니다.",
            (Self::En, "apply_for_account") => "Apply for an account",
            (Self::Ko, "apply_for_account") => "계정 신청하기",
            // ── email confirmation ───────────────────────────────────────────
            (Self::En, "confirm_success") => {
                "Your email is confirmed. Sign in to start using your account."
            }
            (Self::Ko, "confirm_success") => {
                "이메일이 인증되었습니다. 로그인하여 계정을 사용하세요."
            }
            (Self::En, "confirm_invalid") => {
                "That confirmation link is no longer valid. It may already have been used, or it may have expired — try signing in."
            }
            (Self::Ko, "confirm_invalid") => {
                "인증 링크가 더 이상 유효하지 않습니다. 이미 사용되었거나 만료되었을 수 있습니다. 로그인해 보세요."
            }
            // ── two-factor authentication ────────────────────────────────────
            (Self::En, "two_factor_title") => "Two-factor authentication",
            (Self::Ko, "two_factor_title") => "2단계 인증",
            (Self::En, "otp_hint") => {
                "Enter the two-factor code generated by your phone app or use one of your recovery codes:"
            }
            (Self::Ko, "otp_hint") => {
                "휴대전화 앱에서 생성된 2단계 인증 코드를 입력하거나 복구 코드 중 하나를 사용하세요:"
            }
            (Self::En, "otp_attempt") => "Two-factor code",
            (Self::Ko, "otp_attempt") => "2단계 인증 코드",
            (Self::En, "link_to_webauthn") => "Use your security key device",
            (Self::Ko, "link_to_webauthn") => "보안 키 장치 사용",
            (Self::En, "webauthn_title") => "Use one of your security keys to sign in",
            (Self::Ko, "webauthn_title") => "보안 키 중 하나를 사용해 로그인하세요",
            (Self::En, "webauthn_hint") => {
                "If it's an USB key be sure to insert it and, if necessary, tap it."
            }
            (Self::Ko, "webauthn_hint") => "USB 키라면 꽂은 뒤 필요하면 눌러 주세요.",
            (Self::En, "use_security_key") => "Use security key",
            (Self::Ko, "use_security_key") => "보안 키 사용",
            (Self::En, "link_to_otp") => {
                "Enter a two-factor code from your phone or a recovery code"
            }
            (Self::Ko, "link_to_otp") => "휴대전화의 2단계 인증 코드나 복구 코드 입력",
            (Self::En, "webauthn_not_supported") => "This browser doesn't support security keys",
            (Self::Ko, "webauthn_not_supported") => "이 브라우저는 보안 키를 지원하지 않습니다",
            (Self::En, "invalid_otp_token") => "Invalid two-factor code",
            (Self::Ko, "invalid_otp_token") => "2단계 인증 코드가 올바르지 않습니다",
            (Self::En, "invalid_security_key") => "Invalid security key",
            (Self::Ko, "invalid_security_key") => "보안 키가 올바르지 않습니다",
            (Self::En, "rate_limited") => "Too many authentication attempts, try again later.",
            (Self::Ko, "rate_limited") => "인증 시도가 너무 많습니다. 나중에 다시 시도하세요.",
            (Self::En, "session_timeout") => "Your session expired. Please login again to continue.",
            (Self::Ko, "session_timeout") => "세션이 만료되었습니다. 계속하려면 다시 로그인하세요.",
            (Self::En, "two_factor_role_requirement") => {
                "%{domain} requires you to set up Two-Factor Authentication before you can use Mastodon."
            }
            (Self::Ko, "two_factor_role_requirement") => {
                "%{domain}을(를) 사용하려면 먼저 2단계 인증을 설정해야 합니다."
            }
            (Self::En, "otp_instructions") => {
                "Scan this QR code into Google Authenticator or a similar TOTP app on your phone. From now on, that app will generate tokens that you will have to enter when logging in."
            }
            (Self::Ko, "otp_instructions") => {
                "이 QR 코드를 Google Authenticator 같은 TOTP 앱으로 스캔하세요. 이제부터 그 앱이 생성하는 토큰을 로그인할 때 입력해야 합니다."
            }
            (Self::En, "otp_manual_instructions") => {
                "If you can't scan the QR code and need to enter it manually, here is the plain-text secret:"
            }
            (Self::Ko, "otp_manual_instructions") => {
                "QR 코드를 스캔할 수 없어 직접 입력해야 한다면, 다음 비밀 키를 사용하세요:"
            }
            (Self::En, "otp_code_hint") => "Enter the code generated by your authenticator app to confirm",
            (Self::Ko, "otp_code_hint") => "확인을 위해 인증 앱에서 생성된 코드를 입력하세요",
            (Self::En, "otp_enable") => "Enable",
            (Self::Ko, "otp_enable") => "활성화",
            (Self::En, "otp_wrong_code") => {
                "The entered code was invalid! Are server time and device time correct?"
            }
            (Self::Ko, "otp_wrong_code") => {
                "입력한 코드가 올바르지 않습니다! 서버와 기기의 시간이 맞는지 확인하세요."
            }
            (Self::En, "two_factor_enabled_success") => "Two-factor authentication successfully enabled",
            (Self::Ko, "two_factor_enabled_success") => "2단계 인증이 활성화되었습니다",
            (Self::En, "recovery_instructions") => {
                "If you ever lose access to your phone, you can use one of the recovery codes below to regain access to your account. Keep the recovery codes safe. For example, you may print them and store them with other important documents."
            }
            (Self::Ko, "recovery_instructions") => {
                "휴대전화를 잃어버렸을 때 아래 복구 코드 중 하나로 계정에 다시 접근할 수 있습니다. 복구 코드를 안전하게 보관하세요. 예를 들어 인쇄해서 다른 중요한 문서와 함께 보관할 수 있습니다."
            }
            (Self::En, "resume_app_authorization") => "Resume application authorization",
            (Self::Ko, "resume_app_authorization") => "애플리케이션 인증 계속하기",
            (Self::En, "continue") => "Continue",
            (Self::Ko, "continue") => "계속",
            (Self::En, "two_factor_unavailable") => {
                "Two-factor authentication is not available on this server."
            }
            (Self::Ko, "two_factor_unavailable") => "이 서버에서는 2단계 인증을 사용할 수 없습니다.",
            // ── account deletion, for a user not yet confirmed or approved ───
            (Self::En, "delete_warning_email_change") => {
                "You can change your email address on the account page without deleting your account"
            }
            (Self::Ko, "delete_warning_email_change") => {
                "계정을 삭제하지 않고도 계정 페이지에서 이메일 주소를 바꿀 수 있습니다"
            }
            (Self::En, "delete_warning_email_reconfirmation") => {
                "If you are not receiving the confirmation email, you can request it again"
            }
            (Self::Ko, "delete_warning_email_reconfirmation") => {
                "확인 메일을 받지 못했다면 다시 요청할 수 있습니다"
            }
            (Self::En, "delete_warning_email_contact") => {
                "If it still doesn't arrive, you can email for help:"
            }
            (Self::Ko, "delete_warning_email_contact") => {
                "그래도 오지 않는다면 이메일로 도움을 요청할 수 있습니다:"
            }
            (Self::En, "delete_warning_username_available") => {
                "Your username will become available again"
            }
            (Self::Ko, "delete_warning_username_available") => {
                "사용자 이름은 다시 사용할 수 있게 됩니다"
            }
            (Self::En, "delete_warning_more_details") => "For more details, see the",
            (Self::Ko, "delete_warning_more_details") => "자세한 내용은 다음을 참고하세요:",
            // ── password reset ───────────────────────────────────────────────
            (Self::En, "reset_password") => "Reset password",
            (Self::Ko, "reset_password") => "비밀번호 재설정",
            (Self::En, "set_new_password") => "Set new password",
            (Self::Ko, "set_new_password") => "새 비밀번호 설정",
            (Self::En, "forgot_password") => "Forgot your password?",
            (Self::Ko, "forgot_password") => "비밀번호를 잊으셨나요?",
            // ── sign-up agreement and age ────────────────────────────────────
            (Self::En, "date_of_birth") => "Date of birth",
            (Self::Ko, "date_of_birth") => "생년월일",
            (Self::En, "terms_of_service") => "terms of service",
            (Self::Ko, "terms_of_service") => "이용 약관",
            (Self::En, "privacy_policy") => "privacy policy",
            (Self::Ko, "privacy_policy") => "개인정보 처리방침",
            (Self::En, "agree_terms") => "I have read and agree to the %{terms} and %{privacy}",
            (Self::Ko, "agree_terms") => "%{terms} 및 %{privacy}을 읽었으며 이에 동의합니다",
            (Self::En, "agree_privacy") => "I have read and agree to the %{privacy}",
            (Self::Ko, "agree_privacy") => "%{privacy}을 읽었으며 이에 동의합니다",
            // ── notification_mailer.<type>.subject, a push's title ──────────
            (Self::En, "notification_mailer.admin.report.subject") => "%{name} submitted a report",
            (Self::Ko, "notification_mailer.admin.report.subject") => "%{name} 님이 신고를 제출했습니다",
            (Self::En, "notification_mailer.admin.sign_up.subject") => "%{name} signed up",
            (Self::Ko, "notification_mailer.admin.sign_up.subject") => "%{name} 님이 가입했습니다",
            (Self::En, "notification_mailer.favourite.subject") => "%{name} favorited your post",
            (Self::Ko, "notification_mailer.favourite.subject") => {
                "%{name} 님이 내 게시물을 마음에 들어했습니다"
            }
            (Self::En, "notification_mailer.follow.subject") => "%{name} is now following you",
            (Self::Ko, "notification_mailer.follow.subject") => "%{name} 님이 나를 팔로우했습니다",
            (Self::En, "notification_mailer.follow_request.subject") => "Pending follower: %{name}",
            (Self::Ko, "notification_mailer.follow_request.subject") => {
                "%{name} 님이 보낸 팔로우 요청"
            }
            (Self::En, "notification_mailer.mention.subject") => "You were mentioned by %{name}",
            (Self::Ko, "notification_mailer.mention.subject") => "%{name} 님의 멘션",
            (Self::En, "notification_mailer.moderation_warning.subject") => {
                "You have received a moderation warning"
            }
            (Self::Ko, "notification_mailer.moderation_warning.subject") => "중재 경고를 받았습니다",
            (Self::En, "notification_mailer.poll.subject") => "A poll by %{name} has ended",
            (Self::Ko, "notification_mailer.poll.subject") => "%{name}의 설문이 종료됨",
            (Self::En, "notification_mailer.quote.subject") => "%{name} quoted your post",
            (Self::Ko, "notification_mailer.quote.subject") => "%{name} 님이 내 게시물을 인용했습니다",
            (Self::En, "notification_mailer.quoted_update.subject") => {
                "%{name} edited a post you have quoted"
            }
            (Self::Ko, "notification_mailer.quoted_update.subject") => {
                "%{name} 님이 내가 인용한 게시물을 수정했습니다"
            }
            (Self::En, "notification_mailer.reblog.subject") => "%{name} boosted your post",
            (Self::Ko, "notification_mailer.reblog.subject") => {
                "%{name} 님이 내 게시물을 부스트 했습니다"
            }
            (Self::En, "notification_mailer.severed_relationships.subject") => {
                "You have lost connections due to a moderation decision"
            }
            (Self::Ko, "notification_mailer.severed_relationships.subject") => {
                "중재 결정으로 인해 연결이 끊어졌습니다"
            }
            (Self::En, "notification_mailer.status.subject") => "%{name} just posted",
            (Self::Ko, "notification_mailer.status.subject") => {
                "%{name} 님이 방금 게시물을 올렸습니다"
            }
            (Self::En, "notification_mailer.update.subject") => "%{name} edited a post",
            (Self::Ko, "notification_mailer.update.subject") => "%{name} 님이 게시물을 수정했습니다",
            // ── admin dashboard ──────────────────────────────────────────────
            (Self::En, "admin.dashboard.media_storage") => "Media storage",
            (Self::Ko, "admin.dashboard.media_storage") => "미디어 저장소",
            (Self::En, "admin.dashboard.website") => "Website",
            (Self::Ko, "admin.dashboard.website") => "웹사이트",
            (Self::En, "generic.none") => "None",
            (Self::Ko, "generic.none") => "없음",
            // fallback
            _ => "",
        }
    }
}
