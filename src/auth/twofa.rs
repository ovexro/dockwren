//! Optional two-factor sign-in (TOTP). Off until the admin sets it up in Settings.
//! State is two files in the data dir: `totp.secret` (base32) and `totp.recovery` (SHA-256
//! of each unused recovery code). `dockwren disable-2fa` removes both.

use super::*;
use crate::totp;

const SECRET_FILE: &str = "totp.secret";
const RECOVERY_FILE: &str = "totp.recovery";
const RECOVERY_COUNT: usize = 8;

/// Turn 2FA off from the command line (lost phone and recovery codes).
pub fn disable(data_dir: &Path) -> io::Result<bool> {
    let mut was_on = false;
    for f in [SECRET_FILE, RECOVERY_FILE] {
        match fs::remove_file(data_dir.join(f)) {
            Ok(()) => was_on = true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(was_on)
}

impl Auth {
    fn file(&self, name: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(name))
    }

    fn secret(&self) -> Option<Vec<u8>> {
        let text = fs::read_to_string(self.file(SECRET_FILE)?).ok()?;
        totp::from_base32(&text).filter(|s| !s.is_empty())
    }

    pub fn two_factor_enabled(&self) -> bool {
        self.secret().is_some()
    }

    /// True when 2FA is off, or `input` is a valid current code or an unused recovery code.
    pub(super) fn second_factor_ok(&self, input: &str) -> bool {
        let Some(secret) = self.secret() else {
            return true;
        };
        let mut last = self.last_step.lock().unwrap();
        if let Some(step) = totp::verify(&secret, input, now_unix(), *last) {
            *last = step;
            return true;
        }
        drop(last);
        self.use_recovery(input)
    }

    fn use_recovery(&self, input: &str) -> bool {
        let Some(path) = self.file(RECOVERY_FILE) else {
            return false;
        };
        let given = sha_hex(&input.trim().to_ascii_lowercase());
        let text = fs::read_to_string(&path).unwrap_or_default();
        let mut hit = false;
        let mut keep = String::new();
        for line in text.lines() {
            // Compare every line, so the time does not depend on which one matched.
            if totp::same(line, &given) && !hit {
                hit = true;
            } else {
                keep.push_str(line);
                keep.push('\n');
            }
        }
        if hit {
            let _ = write_private(&path, &keep);
        }
        hit
    }

    pub(super) fn turn_on(&self, secret: &[u8]) -> io::Result<Vec<String>> {
        let (Some(sp), Some(rp)) = (self.file(SECRET_FILE), self.file(RECOVERY_FILE)) else {
            return Err(io::Error::other("no data directory"));
        };
        let codes: Vec<String> = (0..RECOVERY_COUNT).map(|_| totp::recovery_code()).collect();
        let hashes: String = codes.iter().map(|c| format!("{}\n", sha_hex(c))).collect();
        write_private(&rp, &hashes)?;
        write_private(&sp, &totp::base32(secret))?;
        Ok(codes)
    }
}

pub struct Setup {
    svg: String,
    key: String,
}

#[derive(Template)]
#[template(path = "twofa.html")]
struct TwofaTemplate {
    enabled: bool,
    setup: Option<Setup>,
    recovery: Vec<String>,
    error: Option<&'static str>,
    available: bool,
}

fn render(
    auth: &Auth,
    setup: Option<Setup>,
    recovery: Vec<String>,
    error: Option<&'static str>,
) -> Html<String> {
    Html(
        TwofaTemplate {
            enabled: auth.two_factor_enabled(),
            setup,
            recovery,
            error,
            available: auth.dir.is_some(),
        }
        .render()
        .expect("twofa template renders"),
    )
}

fn setup_for(secret: &[u8]) -> Option<Setup> {
    let uri = totp::otpauth_uri(secret, "admin", "Dockwren");
    Some(Setup {
        svg: totp::qr_svg(&uri)?,
        key: totp::base32(secret),
    })
}

pub async fn show(State(auth): State<SharedAuth>) -> Html<String> {
    render(&auth, None, Vec::new(), None)
}

pub async fn start(State(auth): State<SharedAuth>) -> Html<String> {
    if auth.two_factor_enabled() || auth.dir.is_none() {
        return render(&auth, None, Vec::new(), None);
    }
    let secret = totp::new_secret();
    *auth.pending.lock().unwrap() = Some(secret.clone());
    render(&auth, setup_for(&secret), Vec::new(), None)
}

#[derive(Deserialize)]
pub struct CodeForm {
    code: String,
}

pub async fn enable(State(auth): State<SharedAuth>, Form(f): Form<CodeForm>) -> Html<String> {
    let Some(secret) = auth.pending.lock().unwrap().clone() else {
        return render(&auth, None, Vec::new(), Some("Start the setup again."));
    };
    let Some(step) = totp::verify(&secret, &f.code, now_unix(), 0) else {
        return render(
            &auth,
            setup_for(&secret),
            Vec::new(),
            Some("That code is not right. Check the phone's clock and try the next code."),
        );
    };
    match auth.turn_on(&secret) {
        Ok(codes) => {
            *auth.pending.lock().unwrap() = None;
            *auth.last_step.lock().unwrap() = step;
            render(&auth, None, codes, None)
        }
        Err(_) => render(
            &auth,
            None,
            Vec::new(),
            Some("Could not save the settings."),
        ),
    }
}

pub async fn turn_off(State(auth): State<SharedAuth>, Form(f): Form<CodeForm>) -> Html<String> {
    if !auth.two_factor_enabled() {
        return render(&auth, None, Vec::new(), None);
    }
    // Needs a current code or a recovery code, so a stolen session alone cannot remove it.
    if !auth.second_factor_ok(&f.code) {
        auth.record_failure();
        return render(&auth, None, Vec::new(), Some("That code is not right."));
    }
    if let Some(dir) = &auth.dir {
        let _ = disable(dir);
    }
    render(&auth, None, Vec::new(), None)
}
