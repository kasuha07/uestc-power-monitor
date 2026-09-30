//! Offline HTTPS proxy fixture: every CAS/WeChat/portal request terminates here.
//! The real client's redirects, cookies, form parsing and encryption still run.
use openssl::{
    asn1::Asn1Time,
    hash::MessageDigest,
    pkey::PKey,
    rsa::Rsa,
    ssl::{SslAcceptor, SslMethod},
    x509::{X509, X509NameBuilder},
};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

pub struct School {
    pub address: std::net::SocketAddr,
    pub stage: Arc<AtomicUsize>,
    pub confirmed: Arc<AtomicBool>,
    pub sent_codes: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

fn read_headers(reader: &mut impl BufRead) -> std::io::Result<(String, usize)> {
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
    }
    Ok((first, length))
}

fn serve(
    stream: TcpStream,
    tls: Arc<SslAcceptor>,
    stage: Arc<AtomicUsize>,
    confirmed: Arc<AtomicBool>,
    sent: Arc<AtomicUsize>,
) {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut stream = BufReader::new(stream);
    let _ = read_headers(&mut stream).unwrap(); // CONNECT
    let mut stream = stream.into_inner();
    stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .unwrap();
    let Ok(stream) = tls.accept(stream) else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let Ok((request, length)) = read_headers(&mut reader) else {
        return;
    };
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let parts: Vec<_> = request.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    let path = parts[1].split('?').next().unwrap();
    let form: std::collections::HashMap<_, _> =
        url::form_urlencoded::parse(&body).into_owned().collect();
    let login_page = "<div id=\"pwdLoginDiv\"><input id=\"pwdEncryptSalt\" value=\"AbcDef1234567890\"><input id=\"execution\" value=\"e1s1\"></div>";
    let mut code = "200 OK";
    let mut extra = "";
    let response = match (method, path) {
        ("POST", "/authserver/login") => {
            if form.get("username").is_some_and(|s| s == "bad") {
                "<span id=\"showErrorTip\">secret-upstream-error ticket=ST-private</span>"
                    .to_owned()
            } else {
                stage.store(1, Ordering::SeqCst);
                code = "302 Found";
                extra = "Location: https://idas.uestc.edu.cn/authserver/reAuthCheck/reAuthLoginView.do\r\nSet-Cookie: TGT=mock-tgt; Path=/; Secure; HttpOnly\r\n";
                String::new()
            }
        }
        ("GET", "/authserver/login") => {
            if stage.load(Ordering::SeqCst) == 2 {
                code = "302 Found";
                extra =
                    "Location: https://idas.uestc.edu.cn/personalInfo/personCenter/index.html\r\n";
                String::new()
            } else {
                login_page.to_owned()
            }
        }
        (_, "/authserver/reAuthCheck/reAuthLoginView.do") => {
            include_str!("../../vendor/uestc-client/tests/fixtures/reauth-page.html").to_owned()
        }
        (_, "/authserver/reAuthCheck/changeReAuthType.do") => "{\"code\":1}".into(),
        (_, "/authserver/dynamicCode/getDynamicCodeByReauth.do") => {
            sent.fetch_add(1, Ordering::SeqCst);
            "{\"res\":\"success\"}".into()
        }
        (_, "/authserver/reAuthCheck/reAuthSubmit.do") => {
            if form.get("dynamicCode").is_some_and(|c| c == "123456") {
                stage.store(2, Ordering::SeqCst);
                "{\"code\":\"success\"}".into()
            } else {
                "{\"code\":\"reAuth_failed\",\"msg\":\"wrong code\"}".into()
            }
        }
        (_, "/authserver/combinedLogin.do") => {
            code = "302 Found";
            extra = "Location: https://open.weixin.qq.com/connect/qrconnect?appid=test&redirect_uri=https%3A%2F%2Fidas.uestc.edu.cn%2Fauthserver%2Fmock-callback&state=test\r\n";
            String::new()
        }
        (_, "/connect/qrconnect") => "<root><uuid>mock-uuid</uuid></root>".into(),
        (_, "/connect/l/qrconnect") => {
            if confirmed.load(Ordering::SeqCst) {
                "window.wx_errcode=405;window.wx_code='mock-code';".into()
            } else {
                "window.wx_errcode=408;".into()
            }
        }
        (_, "/authserver/mock-callback") => {
            stage.store(2, Ordering::SeqCst);
            "confirmed".into()
        }
        (_, "/common/getLanguageTypes.htl") => {
            code = "404 Not Found";
            "<html>retired endpoint</html>".into()
        }
        ("GET", "/site/user_info") => {
            if stage.load(Ordering::SeqCst) == 3 {
                code = "503 Service Unavailable";
                "temporary outage".into()
            } else if stage.load(Ordering::SeqCst) == 2 {
                "{\"e\":0,\"m\":\"ok\",\"d\":{\"mock\":true}}".into()
            } else {
                code = "401 Unauthorized";
                "{\"e\":401,\"m\":\"expired\"}".into()
            }
        }
        (_, "/site/bedroom") => {
            if stage.load(Ordering::SeqCst) == 2 {
                "{\"e\":0,\"m\":\"ok\",\"d\":{\"sydl\":26.91,\"syje\":14.44,\"roomName\":\"220407\"}}".into()
            } else if stage.load(Ordering::SeqCst) == 3 {
                "<html>temporary upstream error</html>".into()
            } else {
                code = "401 Unauthorized";
                "{\"e\":1,\"m\":\"expired\"}".into()
            }
        }
        _ => "ok".into(),
    };
    let response = format!(
        "HTTP/1.1 {code}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{response}",
        response.len()
    );
    let _ = reader.get_mut().write_all(response.as_bytes());
}

impl School {
    pub fn new() -> Self {
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "localhost").unwrap();
        let name = name.build();
        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        cert.set_not_after(&Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        let mut tls = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
        tls.set_private_key(&key).unwrap();
        tls.set_certificate(&cert.build()).unwrap();
        let tls = Arc::new(tls.build());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stage = Arc::new(AtomicUsize::new(0));
        let confirmed = Arc::new(AtomicBool::new(false));
        let sent_codes = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, c, n, end) = (
            stage.clone(),
            confirmed.clone(),
            sent_codes.clone(),
            stop.clone(),
        );
        std::thread::spawn(move || {
            while !end.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (tls, stage, confirmed, sent) =
                            (tls.clone(), s.clone(), c.clone(), n.clone());
                        std::thread::spawn(move || serve(stream, tls, stage, confirmed, sent));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            stage,
            confirmed,
            sent_codes,
            stop,
        }
    }

    pub fn builder(&self) -> reqwest::ClientBuilder {
        // No real UESTC or WeChat network traffic can bypass this explicit proxy.
        reqwest::Client::builder()
            .http1_only()
            .danger_accept_invalid_certs(true)
            .proxy(reqwest::Proxy::all(format!("http://{}", self.address)).unwrap())
    }
}

impl Drop for School {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}
