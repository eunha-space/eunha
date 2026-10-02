//! Which browser and platform a `User-Agent` names, as Mastodon's
//! `BrowserDetection` asks the `browser` gem. Only the identifiers Mastodon
//! has names for (`sessions.browsers.*` and `sessions.platforms.*`) are told
//! apart; everything else is `generic` on `unknown_platform`.

/// `(browser id, platform id)`.
pub fn detect(user_agent: &str) -> (&'static str, &'static str) {
    (browser(user_agent), platform(user_agent))
}

/// `sessions.description`: "%{browser} on %{platform}".
pub fn describe(user_agent: &str) -> String {
    let (browser, platform) = detect(user_agent);
    format!("{} on {}", browser_name(browser), platform_name(platform))
}

fn browser(ua: &str) -> &'static str {
    // Order matters: most browsers also claim to be Safari or Chrome.
    let checks: &[(&str, &str)] = &[
        ("Edg", "edge"),
        ("OPR/", "opera"),
        ("Opera", "opera"),
        ("Electron/", "electron"),
        ("UCBrowser", "uc_browser"),
        ("MicroMessenger", "micro_messenger"),
        ("QQBrowser", "qq"),
        ("HuaweiBrowser", "huawei_browser"),
        ("AlipayClient", "alipay"),
        ("Weibo", "weibo"),
        ("PhantomJS", "phantom_js"),
        ("Otter", "otter"),
        ("Firefox/", "firefox"),
        ("FxiOS", "firefox"),
        ("CriOS", "chrome"),
        ("Chrome/", "chrome"),
        ("MSIE", "ie"),
        ("Trident/", "ie"),
        ("BlackBerry", "blackberry"),
        ("Safari/", "safari"),
    ];
    checks
        .iter()
        .find(|(needle, _)| ua.contains(needle))
        .map(|(_, id)| *id)
        .unwrap_or("generic")
}

fn platform(ua: &str) -> &'static str {
    let checks: &[(&str, &str)] = &[
        ("Windows Phone", "windows_phone"),
        ("Windows Mobile", "windows_mobile"),
        ("Windows", "windows"),
        ("iPhone", "ios"),
        ("iPad", "ios"),
        ("iPod", "ios"),
        ("Android", "android"),
        ("CrOS", "chrome_os"),
        ("KAIOS", "kai_os"),
        ("BlackBerry", "blackberry"),
        ("Macintosh", "mac"),
        ("Mac OS X", "mac"),
        ("Linux", "linux"),
    ];
    checks
        .iter()
        .find(|(needle, _)| ua.contains(needle))
        .map(|(_, id)| *id)
        .unwrap_or("unknown_platform")
}

/// `sessions.browsers.*`.
pub fn browser_name(id: &str) -> &'static str {
    match id {
        "alipay" => "Alipay",
        "blackberry" => "BlackBerry",
        "chrome" => "Chrome",
        "edge" => "Microsoft Edge",
        "electron" => "Electron",
        "firefox" => "Firefox",
        "huawei_browser" => "Huawei Browser",
        "ie" => "Internet Explorer",
        "micro_messenger" => "MicroMessenger",
        "opera" => "Opera",
        "otter" => "Otter",
        "phantom_js" => "PhantomJS",
        "qq" => "QQ Browser",
        "safari" => "Safari",
        "uc_browser" => "UC Browser",
        "weibo" => "Weibo",
        _ => "Unknown browser",
    }
}

/// `sessions.platforms.*`.
pub fn platform_name(id: &str) -> &'static str {
    match id {
        "android" => "Android",
        "blackberry" => "BlackBerry",
        "chrome_os" => "ChromeOS",
        "ios" => "iOS",
        "kai_os" => "KaiOS",
        "linux" => "Linux",
        "mac" => "macOS",
        "windows" => "Windows",
        "windows_mobile" => "Windows Mobile",
        "windows_phone" => "Windows Phone",
        _ => "Unknown Platform",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_common_browsers_apart() {
        let mac_firefox =
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.5; rv:128.0) Gecko/20100101 Firefox/128.0";
        assert_eq!(detect(mac_firefox), ("firefox", "mac"));
        assert_eq!(describe(mac_firefox), "Firefox on macOS");

        let windows_edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                            (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0";
        assert_eq!(detect(windows_edge), ("edge", "windows"));

        let iphone_safari = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) \
                             AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 \
                             Mobile/15E148 Safari/604.1";
        assert_eq!(detect(iphone_safari), ("safari", "ios"));

        let android_chrome = "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like \
                              Gecko) Chrome/126.0.0.0 Mobile Safari/537.36";
        assert_eq!(detect(android_chrome), ("chrome", "android"));

        assert_eq!(describe(""), "Unknown browser on Unknown Platform");
    }
}
