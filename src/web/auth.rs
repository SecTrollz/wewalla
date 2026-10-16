//! Token, session-cookie and single-use WebSocket ticket handling.

use super::crypto::{ct_eq, hex, random_bytes};
use super::http::Request;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const COOKIE: &str = "rvcsi_session";
const TICKET_TTL: Duration = Duration::from_secs(30);
const MAX_TICKETS: usize = 64;
const FAIL_WINDOW: Duration = Duration::from_secs(60);
const FAIL_LIMIT: usize = 10;

/// Load the node token, creating it (0600, in a 0700 dir) on first run. A malformed
/// existing file is an error, never silently replaced.
pub fn load_or_create_token(dir: &Path) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join("token");
    match std::fs::read_to_string(&path) {
        Ok(t) => {
            let t = t.trim().to_string();
            if t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
                Ok(t)
            } else {
                Err(format!(
                    "{} is malformed (expected 64 hex chars); refusing to overwrite",
                    path.display()
                ))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let t = hex(&random_bytes(32)?);
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600);
            }
            use std::io::Write;
            o.open(&path)
                .and_then(|mut f| writeln!(f, "{t}"))
                .map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(t)
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[derive(Debug)]
pub struct Auth {
    token: String,
    tickets: Mutex<HashMap<String, Instant>>,
    fails: Mutex<VecDeque<Instant>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Denied,
    Throttled,
}

impl Auth {
    pub fn new(token: String) -> Self {
        Self {
            token,
            tickets: Mutex::new(HashMap::new()),
            fails: Mutex::new(VecDeque::new()),
        }
    }

    pub fn token_matches(&self, candidate: &str) -> bool {
        ct_eq(candidate.as_bytes(), self.token.as_bytes())
    }

    fn note_fail(&self) {
        if let Ok(mut f) = self.fails.lock() {
            f.push_back(Instant::now());
            while f.len() > FAIL_LIMIT * 4 {
                f.pop_front();
            }
        }
    }

    fn throttled(&self) -> bool {
        self.fails.lock().map_or(true, |mut f| {
            while f.front().is_some_and(|t| t.elapsed() > FAIL_WINDOW) {
                f.pop_front();
            }
            f.len() >= FAIL_LIMIT
        })
    }

    /// Bearer header or session cookie.
    pub fn check(&self, req: &Request) -> Verdict {
        if self.throttled() {
            return Verdict::Throttled;
        }
        let bearer = req
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim);
        let cookie = req.cookie(COOKIE);
        let given = bearer.or(cookie);
        match given {
            Some(t) if self.token_matches(t) => Verdict::Ok,
            Some(_) => {
                self.note_fail();
                Verdict::Denied
            }
            None => Verdict::Denied, // anonymous probes are not credential guesses
        }
    }

    pub fn login(&self, candidate: &str) -> Verdict {
        if self.throttled() {
            return Verdict::Throttled;
        }
        if self.token_matches(candidate) {
            Verdict::Ok
        } else {
            self.note_fail();
            Verdict::Denied
        }
    }

    pub fn mint_ticket(&self) -> Result<String, String> {
        let t = hex(&random_bytes(16)?);
        let mut g = self
            .tickets
            .lock()
            .map_err(|_| "ticket store poisoned".to_string())?;
        g.retain(|_, born| born.elapsed() < TICKET_TTL);
        if g.len() >= MAX_TICKETS {
            return Err("too many outstanding tickets".into());
        }
        g.insert(t.clone(), Instant::now());
        Ok(t)
    }

    /// Single use: a successful redemption removes the ticket.
    pub fn redeem_ticket(&self, t: &str) -> bool {
        self.tickets
            .lock()
            .is_ok_and(|mut g| g.remove(t).is_some_and(|born| born.elapsed() < TICKET_TTL))
    }

    pub fn session_cookie(&self) -> String {
        format!(
            "{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000",
            self.token
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rvcsi-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn token_created_private_and_stable() {
        let d = tmp("tok");
        let t1 = load_or_create_token(&d).unwrap();
        assert_eq!(t1.len(), 64);
        assert_eq!(load_or_create_token(&d).unwrap(), t1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(d.join("token"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&d).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        std::fs::write(d.join("token"), "short").unwrap();
        assert!(
            load_or_create_token(&d).is_err(),
            "malformed token must not be replaced"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tickets_are_single_use() {
        let a = Auth::new("t".repeat(64));
        let t = a.mint_ticket().unwrap();
        assert!(a.redeem_ticket(&t));
        assert!(!a.redeem_ticket(&t));
        assert!(!a.redeem_ticket("forged"));
    }

    #[test]
    fn ticket_store_is_bounded() {
        let a = Auth::new("t".repeat(64));
        for _ in 0..MAX_TICKETS {
            a.mint_ticket().unwrap();
        }
        assert!(a.mint_ticket().is_err());
    }

    #[test]
    fn repeated_bad_credentials_get_throttled() {
        let a = Auth::new("a".repeat(64));
        for _ in 0..FAIL_LIMIT {
            assert_eq!(a.login("wrong"), Verdict::Denied);
        }
        assert_eq!(a.login("wrong"), Verdict::Throttled);
        assert_eq!(
            a.login(&"a".repeat(64)),
            Verdict::Throttled,
            "even the right token waits out the window"
        );
    }
}
