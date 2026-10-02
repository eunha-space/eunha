use minijinja::{Environment, Value};
use once_cell::sync::Lazy;

static ENV: Lazy<Environment<'static>> = Lazy::new(|| {
    let mut env = Environment::new();
    env.add_template_owned(
        "authorize.html",
        include_str!("templates/authorize.html").to_string(),
    )
    .expect("authorize.html template is invalid");
    env.add_template_owned(
        "unknown_host.html",
        include_str!("templates/unknown_host.html").to_string(),
    )
    .expect("unknown_host.html template is invalid");
    env.add_template_owned(
        "signup.html",
        include_str!("templates/signup.html").to_string(),
    )
    .expect("signup.html template is invalid");
    env.add_template_owned(
        "account_login.html",
        include_str!("templates/account_login.html").to_string(),
    )
    .expect("account_login.html template is invalid");
    env.add_template_owned(
        "account_home.html",
        include_str!("templates/account_home.html").to_string(),
    )
    .expect("account_home.html template is invalid");
    env.add_template_owned(
        "account_password.html",
        include_str!("templates/account_password.html").to_string(),
    )
    .expect("account_password.html template is invalid");
    env.add_template_owned(
        "account_delete.html",
        include_str!("templates/account_delete.html").to_string(),
    )
    .expect("account_delete.html template is invalid");
    env.add_template_owned(
        "email_subscription.html",
        include_str!("templates/email_subscription.html").to_string(),
    )
    .expect("email_subscription.html template is invalid");
    env
});

pub fn render(name: &str, ctx: Value) -> String {
    ENV.get_template(name)
        .and_then(|t| t.render(ctx))
        .unwrap_or_else(|e| format!("Template error: {e}"))
}
