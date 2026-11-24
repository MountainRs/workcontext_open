use super::{queue::Decision, Control};
use std::{io::{Read, Write}, process::{Command, Stdio}, sync::atomic::Ordering, thread, time::{Duration, Instant}};

// The queue lease is five minutes. Leave time for revalidation and the commit.
const TIMEOUT: Duration = Duration::from_secs(250);
const OUTPUT_LIMIT: u64 = 2 * 1024 * 1024;

pub(super) fn interpreter() -> Result<Command, String> {
    use std::{path::PathBuf, sync::OnceLock};
    static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
    if EXECUTABLE.get().is_none() {
        // Windows py.exe and venv redirectors spawn a second process. Supervise the actual
        // interpreter so revocation and shutdown also terminate a blocked network call.
        let probe = crate::archive::archive_python()?.args(["-I", "-S", "-c",
            "import sys;sys.stdout.buffer.write((getattr(sys,'_base_executable',None) or sys.executable).encode('utf-8'))"])
            .stdout(Stdio::piped()).output().map_err(|_| "无法定位 Python 解释器")?;
        let path = PathBuf::from(std::str::from_utf8(&probe.stdout).map_err(|_| "解释器路径无效")?);
        if !probe.status.success() || !path.is_absolute() || !path.is_file() {return Err("解释器路径无效".into());}
        let _ = EXECUTABLE.set(path);
    }
    let mut command = Command::new(EXECUTABLE.get().expect("resolved interpreter"));
    command.args(["-E", "-s", "-X", "utf8"]);
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    Ok(command)
}

pub(super) fn call(mut command: Command, input: &[u8], control: &Control, generation: u64) -> Result<Decision, &'static str> {
    let current = || !control.stopping.load(Ordering::Acquire) && control.generation.load(Ordering::Acquire) == generation;
    if !current() { return Err("consent_changed"); }
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = super::process_tree::ProcessTree::spawn(&mut command).map_err(|_| "model_unavailable")?;
    let mut stdin = child.take_stdin().expect("piped stdin");
    let stdout = child.take_stdout().expect("piped stdout");
    let until = Instant::now() + TIMEOUT;
    thread::scope(|threads| {
        let writer = threads.spawn(move || stdin.write_all(input));
        let reader = threads.spawn(move || {
            let mut output = Vec::new();
            stdout.take(OUTPUT_LIMIT + 1).read_to_end(&mut output).map(|_| output)
        });
        let result = loop {
            if !current() { break Err("consent_changed"); }
            if Instant::now() >= until { break Err("model_timeout"); }
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break Ok(()),
                Ok(Some(_)) => break Err("model_failed"),
                Err(_) => break Err("model_unavailable"),
                Ok(None) => thread::sleep(Duration::from_millis(100)),
            }
        };
        if result.is_err() { child.stop(); }
        result?;
        writer.join().map_err(|_| "model_io")?.map_err(|_| "model_io")?;
        let output = reader.join().map_err(|_| "model_io")?.map_err(|_| "model_io")?;
        if output.len() as u64 > OUTPUT_LIMIT { return Err("invalid_model_response"); }
        serde_json::from_slice(&output).map_err(|_| "invalid_model_response")
    })
}
