//! Bounded child-process execution. Termux:API commands can hang forever when the
//! companion app is missing or a permission dialog is pending, so every call has a hard
//! deadline, output cap, and is killed on timeout.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_OUT: u64 = 1 << 20;

pub fn run(argv: &[String], timeout: Duration) -> Result<String, String> {
    let (prog, args) = argv.split_first().ok_or("empty command")?;
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{prog}: {e}"))?;
    let mut out = child.stdout.take().ok_or("no stdout pipe")?;
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = (&mut out).take(MAX_OUT).read_to_string(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!("{prog}: timed out after {timeout:?}"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(format!("{prog}: {e}")),
        }
    };
    let text = reader
        .join()
        .map_err(|_| "reader thread panicked".to_string())?;
    if status.success() {
        Ok(text)
    } else {
        Err(format!("{prog}: exit {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn captures_output() {
        assert_eq!(
            run(&v(&["echo", "hi"]), Duration::from_secs(2))
                .unwrap()
                .trim(),
            "hi"
        );
    }

    #[test]
    fn kills_on_timeout() {
        let t = Instant::now();
        let e = run(&v(&["sleep", "10"]), Duration::from_millis(150)).unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn nonzero_exit_and_missing_binary_are_errors() {
        assert!(run(&v(&["false"]), Duration::from_secs(2)).is_err());
        assert!(run(&v(&["/definitely/not/here"]), Duration::from_secs(2)).is_err());
        assert!(run(&[], Duration::from_secs(1)).is_err());
    }
}
