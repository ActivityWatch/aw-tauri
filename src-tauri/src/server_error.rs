//! Explain port conflicts without claiming that the info endpoint identifies a launcher.
use std::{fmt, io::Read, time::Duration};

#[derive(Debug)]
pub(crate) enum StartupError {
    PortConflict(String),
    Other(String),
}

impl StartupError {
    pub(crate) fn port_conflict(port: u16) -> Self {
        // A blocking client's runtime cannot be dropped inside Tokio. A short
        // dedicated thread also makes this callable from both startup paths.
        let activitywatch = std::thread::spawn(move || probe_activitywatch(port))
            .join()
            .unwrap_or(false);
        Self::PortConflict(port_conflict_message(port, activitywatch))
    }

    pub(crate) fn from_launch(port: u16, error: &rocket::Error) -> Self {
        match error.kind() {
            rocket::error::ErrorKind::Bind(error)
                if error.kind() == std::io::ErrorKind::AddrInUse =>
            {
                Self::port_conflict(port)
            }
            _ => Self::Other(format!("ActivityWatch server failed: {error}")),
        }
    }

    pub(crate) fn is_port_conflict(&self) -> bool {
        matches!(self, Self::PortConflict(_))
    }
}

impl From<String> for StartupError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

impl fmt::Display for StartupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PortConflict(message) | Self::Other(message) => formatter.write_str(message),
        }
    }
}

fn port_conflict_message(port: u16, activitywatch: bool) -> String {
    if activitywatch {
        format!(
            "Another ActivityWatch server is already running on port {port} (for example, from aw-qt). \
             Quit the other ActivityWatch instance, then start aw-tauri again. \
             If this happens at login, disable one of the two ActivityWatch startup entries."
        )
    } else {
        format!(
            "Port {port} is already in use. Quit the application using it, \
             or choose another port for aw-tauri with --port, then try again."
        )
    }
}

fn probe_activitywatch(port: u16) -> bool {
    let probe = || -> Option<bool> {
        // Only inspect this loopback listener: never follow redirects or use
        // environment proxies. A failed/unauthorized probe stays generic.
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(750))
            .build()
            .ok()?;
        let response = client
            .get(format!("http://127.0.0.1:{port}/api/0/info"))
            .send()
            .ok()?;
        if !response.status().is_success() {
            return Some(false);
        }
        let mut bytes = Vec::new();
        response.take(4097).read_to_end(&mut bytes).ok()?;
        if bytes.len() > 4096 {
            return Some(false);
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        Some(
            value["hostname"].is_string()
                && value["version"].is_string()
                && value["testing"].is_boolean(),
        )
    };
    probe().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, net::TcpListener, thread};

    fn with_response(response: String, test: impl FnOnce(u16)) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = [0; 2048];
            let count = stream.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /api/0/info "));
            stream.write_all(response.as_bytes()).unwrap();
        });
        test(port);
        server.join().unwrap();
    }

    fn response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn identifies_activitywatch_and_explains_login_conflict() {
        with_response(
            response(r#"{"hostname":"local","version":"v0.14.0","testing":false}"#),
            |port| {
                let error = StartupError::port_conflict(port);
                assert!(error.is_port_conflict());
                let message = error.to_string();
                assert!(message.contains("Another ActivityWatch server"));
                assert!(message.contains("for example, from aw-qt"));
                assert!(message.contains("startup entries"));
                assert!(message.contains(&port.to_string()));
            },
        );
    }

    #[test]
    fn unrelated_or_unreadable_service_is_not_labelled_activitywatch() {
        for body in [
            "not json",
            r#"{"version":"other"}"#,
            r#"{"hostname":"x","version":"x","testing":"false"}"#,
        ] {
            with_response(response(body), |port| {
                let message = StartupError::port_conflict(port).to_string();
                assert!(message.contains("--port"));
                assert!(!message.contains("aw-qt"));
            });
        }
    }

    #[test]
    fn redirects_and_oversized_responses_stay_generic() {
        for reply in [
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\n\r\n"
                .to_owned(),
            response(&"x".repeat(4097)),
        ] {
            with_response(reply, |port| assert!(!probe_activitywatch(port)));
        }
    }

    #[test]
    fn late_bind_conflict_uses_same_notification_class() {
        with_response(response("{}"), |port| {
            let error: rocket::Error =
                rocket::error::ErrorKind::Bind(std::io::Error::from(std::io::ErrorKind::AddrInUse))
                    .into();
            assert!(StartupError::from_launch(port, &error).is_port_conflict());
        });
        let error: rocket::Error = rocket::error::ErrorKind::Bind(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        ))
        .into();
        assert!(!StartupError::from_launch(5600, &error).is_port_conflict());
    }

    #[test]
    fn competing_listener_after_preflight_produces_port_conflict() {
        let available = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = available.local_addr().unwrap().port();
        drop(available);
        assert!(crate::is_port_available(port).unwrap());
        let _competitor = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let server = rocket::build().configure(rocket::Config {
                address: std::net::Ipv4Addr::LOCALHOST.into(),
                port,
                ..rocket::Config::default()
            });
            let error = server.launch().await.unwrap_err();
            assert!(StartupError::from_launch(port, &error).is_port_conflict());
        });
    }

    #[test]
    fn slow_listener_is_bounded() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let start = std::time::Instant::now();
        assert!(!probe_activitywatch(port));
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
