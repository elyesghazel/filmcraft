//! File ▸ Export ▸ Render on Another Machine: run an export on a host reached over SSH (a desktop
//! with a faster GPU, a server, any machine on the same Tailscale network) and bring the file back.
//!
//! The job:
//!
//! 1. resolves the export settings and range here, exactly as `file.exportMedia` would;
//! 2. checks the host: `ssh -o BatchMode=yes <host>` must work without a prompt, and the host
//!    needs `filmcraft-cli` (on its `PATH` or in `~/.local/bin`) and `rsync`;
//! 3. copies the media the sequence uses into `~/.cache/filmcraft-remote/media/` on the host. Files
//!    are named by a fingerprint of path, size and modification time, so each file crosses the
//!    network once and later renders of the same project only send what changed;
//! 4. writes a copy of the project whose used media point at those copies (unused media and
//!    proxies are dropped), and sends it with the fully resolved settings;
//! 5. runs `filmcraft-cli --progress --project <copy> exec file.exportMedia <params>` on the host,
//!    reading its progress lines, and copies the result back to the requested path.
//!
//! It is an ordinary background job (`jobs.list`, `jobs.cancel`): cancelling stops the remote
//! export too. ssh and rsync are the system's own programs, run as child processes; nothing here
//! links or bundles them.

use std::collections::BTreeSet;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::Duration;

use filmcraft_export::Progress;
use filmcraft_project::{ItemId, ItemKind, MediaRef, Project};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, bad, bool_p, str_p};
use crate::{EngineError, Result, Session};

const CMD: &str = "file.exportMediaRemote";
/// Progress is reported in thousandths.
const TOTAL: u64 = 1000;
/// Share of the bar each stage fills: checks, upload, render, download.
const UPLOAD_END: u64 = 150;
const RENDER_END: u64 = 950;
/// Where the job keeps its files on the host, under `$HOME`.
const REMOTE_BASE: &str = ".cache/filmcraft-remote";

/// One media file to send: where it is here and the name it gets in the host's media cache.
#[derive(Clone, Debug, PartialEq)]
struct Upload {
    item: ItemId,
    local: String,
    name: String,
    bytes: u64,
}

/// Everything the job thread needs, resolved on the session thread.
struct Plan {
    host: String,
    remote_cli: String,
    local_out: String,
    /// `file.exportMedia` params for the host, without `path` (added once the host's home is known).
    params: Value,
    project: Project,
    uploads: Vec<Upload>,
    ext: String,
    job: u64,
}

/// Single-quote `s` for a POSIX shell (and fish, which reads `'\''` the same way).
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A host name that ssh will read as a host, never as an option.
fn valid_host(h: &str) -> bool {
    !h.is_empty() && !h.starts_with('-') && h.len() <= 255 && h.chars().all(|c| c.is_ascii_alphanumeric() || "._-@:[]".contains(c))
}

/// Stable fingerprint of a media file: FNV-1a over its path, size and modification time.
fn fingerprint(path: &str, bytes: u64, mtime: u64) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.bytes().chain(bytes.to_le_bytes()).chain(mtime.to_le_bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The extension a cached copy keeps (decoders look at it), sanitised to ASCII alphanumerics.
fn safe_ext(path: &str) -> String {
    Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().chars().filter(char::is_ascii_alphanumeric).take(8).collect::<String>().to_ascii_lowercase())
        .unwrap_or_default()
}

/// The used media files of `seqs` and a copy of the project that keeps only what the render needs.
fn prepare(s: &Session, seqs: &[ItemId]) -> Result<(Project, Vec<Upload>)> {
    let (_, used) = crate::project_manager::used_ranges(&s.project, seqs);
    let mut proj = (*s.project).clone();
    // Drop media and sequences the render never reads: their files are not sent.
    let keep: BTreeSet<ItemId> = proj
        .items
        .values()
        .filter(|i| used.contains(&i.id) || !matches!(i.kind, ItemKind::Media(_) | ItemKind::Subclip { .. } | ItemKind::Sequence(_)))
        .map(|i| i.id)
        .collect();
    let drop: Vec<ItemId> = proj.items.keys().filter(|i| !keep.contains(i)).copied().collect();
    for i in drop {
        proj.items.remove(&i);
        proj.root.remove_item(i);
    }
    let mut uploads: Vec<Upload> = Vec::new();
    let ids: Vec<ItemId> = proj.items.keys().copied().collect();
    for id in ids {
        let Some(m) = proj.item(id).and_then(|i| i.as_media()) else { continue };
        let MediaRef::File { path } = &m.media else { continue };
        if m.info.kind == filmcraft_media::MediaKind::ImageSequence {
            return Err(bad(CMD, format!("{path}: image sequences can't be rendered on another machine yet")));
        }
        // Read through the media pool's resolved, policy-approved path, as Project Manager does.
        let local = s.media.follow(path).map_err(|why| EngineError::Other(format!("{path} is not opened: {why}")))?;
        let meta = std::fs::metadata(&local).map_err(|e| EngineError::Other(format!("{path} is offline: {e}")))?;
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let ext = safe_ext(&local);
        let key = fingerprint(&local, meta.len(), mtime);
        let name = if ext.is_empty() { key } else { format!("{key}.{ext}") };
        uploads.push(Upload { item: id, local, name, bytes: meta.len() });
    }
    Ok((proj, uploads))
}

/// `file.exportMediaRemote`: resolve here, render on `host`, write `path` here.
pub fn run(s: &mut Session, p: &Value) -> Result<Value> {
    if cfg!(target_arch = "wasm32") {
        return Err(bad(CMD, "rendering on another machine needs the desktop app"));
    }
    let host = str_p(p, "host").map(str::trim).unwrap_or_default().to_string();
    if !valid_host(&host) {
        return Err(bad(CMD, "need `host`: an ssh host name such as `pc`, `user@server` or `100.64.0.2`"));
    }
    let remote_cli = str_p(p, "remoteCli").unwrap_or("filmcraft-cli").to_string();
    if remote_cli.is_empty() || remote_cli.contains(['\n', '\0']) {
        return Err(bad(CMD, "`remoteCli` must be a program name or path"));
    }
    let seq = crate::export_tools::sequence_param(s, p, CMD)?;
    let (_, mut settings) = crate::export_tools::settings_from_params(s, p, CMD)?;
    if settings.is_image_sequence() {
        return Err(bad(CMD, "image-sequence exports can't be rendered on another machine yet"));
    }
    if str_p(p, "path").is_none() {
        return Err(bad(CMD, "need `path`"));
    }
    let project = s.project.clone();
    let (range, warnings) = crate::export_tools::range_param(s, &project, seq, p.get("range"), p, CMD)?;
    settings.range = range;
    settings.path = crate::export_tools::output_path(s, p, seq, &settings, None);
    let local_out = settings.path.clone();
    let ext = Path::new(&local_out).extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "mp4".into());
    let mut remote_settings = serde_json::to_value(&settings).map_err(|e| EngineError::Other(e.to_string()))?;
    if let Some(o) = remote_settings.as_object_mut() {
        // the host decides these: its own output path and its own render range parameter
        o.remove("path");
        o.remove("range");
    }
    let mut params = json!({"sequence": seq.0, "settings": remote_settings});
    if let Some(r) = range {
        params["range"] = json!({"mode": "custom", "startTime": r.start.0, "endTime": r.end().0});
    } else {
        params["range"] = json!("entire");
        // `None` means the export's default range (In to Out, else the whole sequence): resolve it
        // here so the host renders the same frames.
        if let Ok(r) = filmcraft_export::export_range(&project, seq, &settings) {
            params["range"] = json!({"mode": "custom", "startTime": r.start.0, "endTime": r.end().0});
        }
    }
    let (proj, uploads) = prepare(s, &[seq])?;
    let upload_bytes: u64 = uploads.iter().map(|u| u.bytes).sum();
    let summary = json!({
        "host": host,
        "path": local_out,
        "files": uploads.len(),
        "uploadBytes": upload_bytes,
        "warnings": warnings,
    });
    if bool_p(p, "dryRun").unwrap_or(false) {
        return Ok(summary);
    }

    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let job = crate::Job {
        id,
        label: format!("Export on {host}: {}", Path::new(&local_out).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
        progress: Default::default(),
        result: Default::default(),
    };
    job.progress.total.store(TOTAL, Ordering::Relaxed);
    let plan = Plan { host, remote_cli, local_out, params, project: proj, uploads, ext, job: id };
    let (prog, res) = (job.progress.clone(), job.result.clone());
    let body = move || {
        let t0 = web_time::Instant::now();
        let r = execute(&plan, &prog);
        let secs = t0.elapsed().as_secs_f64();
        let r =
            r.map(|bytes| filmcraft_export::Report { path: plan.local_out.clone(), frames: 0, seconds: secs, bytes, render_fps: 0.0, extra_files: Vec::new() });
        if let Err(e) = &r {
            *prog.error.lock().unwrap_or_else(|x| x.into_inner()) = Some(e.clone());
        }
        set_status(
            &prog,
            match &r {
                Ok(_) => format!("Rendered on {} in {secs:.1}s", plan.host),
                Err(e) => e.clone(),
            },
        );
        prog.finished.store(true, Ordering::Relaxed);
        *res.lock().unwrap_or_else(|x| x.into_inner()) = Some(r);
    };
    let body = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), body);
    s.jobs.push(job);
    let wait = bool_p(p, "wait").unwrap_or(false);
    if wait {
        body();
    } else {
        std::thread::Builder::new().name("filmcraft-remote-render".into()).spawn(body).map_err(|e| EngineError::Other(e.to_string()))?;
    }
    let mut out = summary;
    out["job"] = json!(id);
    if wait && let Some(j) = s.jobs.iter().find(|j| j.id == id) {
        let v = j.to_json();
        if let Some(e) = v["result"]["error"].as_str() {
            return Err(EngineError::Other(format!("remote render failed: {e}")));
        }
        out["result"] = v["result"].clone();
    } else {
        out["poll"] = json!("jobs.list");
        out["cancel"] = json!("jobs.cancel");
    }
    Ok(out)
}

fn set_status(prog: &Progress, s: impl Into<String>) {
    *prog.status.lock().unwrap_or_else(|e| e.into_inner()) = s.into();
}

fn set_done(prog: &Progress, v: u64) {
    prog.done.store(v.min(TOTAL), Ordering::Relaxed);
}

fn cancelled(prog: &Progress) -> bool {
    prog.cancel.load(Ordering::Relaxed)
}

/// Options for every ssh connection of a job. Never prompt: a job has no terminal to ask for a
/// password or a host key on. On Unix the job's connections share one login (a render opens about
/// seven, and each new login costs a second or more over a VPN); the shared connection closes a
/// minute after the last use. Windows' OpenSSH can't share connections.
fn ssh_options() -> Vec<&'static str> {
    let mut o = vec!["-o", "BatchMode=yes", "-o", "ConnectTimeout=15"];
    if cfg!(unix) {
        o.extend(["-o", "ControlMaster=auto", "-o", "ControlPath=~/.ssh/filmcraft-%C", "-o", "ControlPersist=60"]);
    }
    o
}

fn ssh_base(host: &str) -> Command {
    let mut c = Command::new("ssh");
    c.args(ssh_options());
    c.args(["--", host]);
    c
}

/// Run `script` with `sh` on the host. The host's login shell may be fish or zsh, so the script is
/// handed to `sh -c` as one quoted word. `~/.local/bin` and `~/.cargo/bin` are added to `PATH`
/// because a non-interactive login often leaves them out.
fn remote(host: &str, script: &str) -> Command {
    let full = format!("PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:$PATH\"; export PATH; {script}");
    let mut c = ssh_base(host);
    c.arg(format!("sh -c {}", sq(&full)));
    c.stdin(Stdio::null());
    c
}

/// Run a short command to completion and return its stdout, or its stderr as the error.
fn output(mut c: Command, what: &str) -> std::result::Result<String, String> {
    let o = c.stdin(Stdio::null()).output().map_err(|e| format!("{what}: can't run {:?}: {e}", c.get_program()))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&o.stderr);
        let line = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("failed").trim();
        Err(format!("{what}: {line}"))
    }
}

/// Wait for `child`, stopping it when the job is cancelled. Each stderr line goes to `on_line`.
fn supervise(mut child: Child, prog: &Progress, mut on_line: impl FnMut(&str)) -> std::result::Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader = child.stderr.take().map(|err| {
        std::thread::spawn(move || {
            // progress is written with \r or \n; split on both
            let mut buf = Vec::new();
            let mut r = BufReader::new(err);
            let mut byte = [0u8; 1];
            while let Ok(1) = r.read(&mut byte) {
                if byte[0] == b'\n' || byte[0] == b'\r' {
                    if !buf.is_empty() {
                        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
                        buf.clear();
                    }
                } else if buf.len() < 64 * 1024 {
                    buf.push(byte[0]);
                }
            }
            if !buf.is_empty() {
                let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
            }
        })
    });
    let mut last = String::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                on_line(&line);
                if !line.trim().is_empty() && !line.starts_with("progress ") {
                    last = line;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if cancelled(prog) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cancelled".into());
        }
    }
    if let Some(r) = reader {
        let _ = r.join();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err(if last.is_empty() { format!("exited with {status}") } else { last.trim().to_string() }) }
}

/// The rsync command for one transfer; `-s` keeps remote paths from being split by the shell.
fn rsync(from: &str, to: &str) -> Command {
    let mut c = Command::new("rsync");
    let ssh = format!("ssh {}", ssh_options().join(" "));
    c.args(["-s", "--partial", "--inplace", "--info=progress2", "-e", &ssh, "--", from, to]);
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    c
}

/// The fraction in an rsync `--info=progress2` line (`  1,234,567  42%  10.2MB/s  0:00:03`).
fn rsync_percent(line: &str) -> Option<f64> {
    line.split_whitespace().find_map(|w| w.strip_suffix('%')).and_then(|n| n.parse::<f64>().ok()).filter(|v| v.is_finite()).map(|v| (v / 100.0).clamp(0.0, 1.0))
}

/// A `progress 0.4231` line from `filmcraft-cli --progress`.
fn cli_progress(line: &str) -> Option<f64> {
    line.trim().strip_prefix("progress ").and_then(|n| n.trim().parse::<f64>().ok()).filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 1.0))
}

fn lerp(a: u64, b: u64, f: f64) -> u64 {
    a + ((b.saturating_sub(a)) as f64 * f.clamp(0.0, 1.0)) as u64
}

fn execute(plan: &Plan, prog: &Progress) -> std::result::Result<u64, String> {
    let host = plan.host.as_str();
    set_status(prog, format!("Connecting to {host}…"));
    let probe = format!(
        "printf '%s\\n' \"$HOME\"; command -v rsync >/dev/null 2>&1 || echo no-rsync; {cli} --version 2>/dev/null || echo no-cli",
        cli = sq(&plan.remote_cli)
    );
    let info = output(remote(host, &probe), &format!("can't reach {host} over ssh"))?;
    let mut lines = info.lines();
    let home = lines.next().unwrap_or_default().trim().to_string();
    let rest: Vec<&str> = lines.collect();
    if home.is_empty() || !home.starts_with('/') {
        return Err(format!("{host}: couldn't read the home directory"));
    }
    if rest.contains(&"no-rsync") {
        return Err(format!("{host} has no rsync: install it there (pacman -S rsync / apt install rsync)"));
    }
    if rest.contains(&"no-cli") {
        return Err(format!("{host} has no {}: install FilmCraft there (its filmcraft-cli must be on PATH or in ~/.local/bin)", plan.remote_cli));
    }
    let remote_version = rest.iter().find(|l| l.contains("filmcraft")).map(|l| l.trim().to_string()).unwrap_or_default();
    let base = format!("{home}/{REMOTE_BASE}");
    let media_dir = format!("{base}/media");
    let job_dir = format!("{base}/jobs/{}-{}", std::process::id(), plan.job);
    output(remote(host, &format!("mkdir -p {} {}", sq(&media_dir), sq(&job_dir))), &format!("{host}: preparing folders"))?;
    set_done(prog, 20);
    if cancelled(prog) {
        return Err("cancelled".into());
    }

    // ---- upload the media (each file once per host)
    let names: Vec<String> = plan.uploads.iter().map(|u| u.name.clone()).collect();
    let have = if names.is_empty() {
        String::new()
    } else {
        let list = names.iter().map(|n| sq(n)).collect::<Vec<_>>().join(" ");
        output(
            remote(host, &format!("cd {} && for f in {list}; do [ -f \"$f\" ] && echo \"$f\"; done; true", sq(&media_dir))),
            &format!("{host}: listing media"),
        )?
    };
    let present: BTreeSet<&str> = have.lines().map(str::trim).collect();
    let todo: Vec<&Upload> = plan.uploads.iter().filter(|u| !present.contains(u.name.as_str())).collect();
    let total_bytes: u64 = todo.iter().map(|u| u.bytes).sum::<u64>().max(1);
    let mut sent = 0u64;
    for (i, u) in todo.iter().enumerate() {
        set_status(prog, format!("Sending media to {host} ({}/{})…", i + 1, todo.len()));
        let part = format!("{media_dir}/{}.part", u.name);
        let child = rsync(&u.local, &format!("{host}:{part}")).spawn().map_err(|e| format!("can't run rsync: {e}"))?;
        let before = sent;
        supervise(child, prog, |line| {
            if let Some(f) = rsync_percent(line) {
                let now = before as f64 + u.bytes as f64 * f;
                set_done(prog, lerp(20, UPLOAD_END, now / total_bytes as f64));
            }
        })
        .map_err(|e| format!("sending {}: {e}", u.local))?;
        // a finished copy gets its real name, so a cut-off transfer is never taken for a whole file
        output(remote(host, &format!("mv -f {} {}", sq(&part), sq(&format!("{media_dir}/{}", u.name)))), &format!("{host}: storing media"))?;
        sent += u.bytes;
        set_done(prog, lerp(20, UPLOAD_END, sent as f64 / total_bytes as f64));
    }
    set_done(prog, UPLOAD_END);

    // ---- the project copy, pointing at the host's media cache
    let mut proj = plan.project.clone();
    for u in &plan.uploads {
        if let Some(m) = proj.item_mut(u.item).and_then(|i| i.as_media_mut()) {
            m.media = MediaRef::File { path: format!("{media_dir}/{}", u.name) };
            m.proxy = None;
        }
    }
    let data = filmcraft_format::encode(&proj, false);
    let tmp: PathBuf = std::env::temp_dir().join(format!("filmcraft-remote-{}-{}.fcproj", std::process::id(), plan.job));
    filmcraft_format::atomic_write(&tmp, &data).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let remote_proj = format!("{job_dir}/render.fcproj");
    let child = rsync(&tmp.to_string_lossy(), &format!("{host}:{remote_proj}")).spawn().map_err(|e| format!("can't run rsync: {e}"))?;
    let sent_proj = supervise(child, prog, |_| {});
    let _ = std::fs::remove_file(&tmp);
    sent_proj.map_err(|e| format!("sending the project: {e}"))?;

    // ---- render there
    let remote_out = format!("{job_dir}/out.{}", plan.ext);
    let mut params = plan.params.clone();
    params["path"] = json!(remote_out);
    let params = serde_json::to_string(&params).map_err(|e| e.to_string())?;
    set_status(prog, if remote_version.is_empty() { format!("Rendering on {host}…") } else { format!("Rendering on {host} ({remote_version})…") });
    let script = format!(
        "cd {jd} && echo $$ > pid && if {cli} --help 2>/dev/null | grep -q -- --progress; then P=--progress; else P=; fi; exec {cli} $P --project render.fcproj exec file.exportMedia {params} > result.json",
        jd = sq(&job_dir),
        cli = sq(&plan.remote_cli),
        params = sq(&params),
    );
    let mut cmd = remote(host, &script);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let child = cmd.spawn().map_err(|e| format!("can't run ssh: {e}"))?;
    let rendered = supervise(child, prog, |line| {
        if let Some(f) = cli_progress(line) {
            set_done(prog, lerp(UPLOAD_END, RENDER_END, f));
        }
    });
    if let Err(e) = rendered {
        if e == "cancelled" {
            // ssh going away does not always stop the remote process: stop it by its pid
            let _ = output(remote(host, &format!("kill \"$(cat {}/pid)\" 2>/dev/null; rm -rf {}", sq(&job_dir), sq(&job_dir))), "stopping");
            return Err(e);
        }
        let _ = output(remote(host, &format!("rm -rf {}", sq(&job_dir))), "cleaning up");
        return Err(format!("{host}: {e}"));
    }

    // ---- bring the file back
    set_status(prog, format!("Copying the render back from {host}…"));
    if let Some(dir) = Path::new(&plan.local_out).parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let part = format!("{}.part", plan.local_out);
    let child = rsync(&format!("{host}:{remote_out}"), &part).spawn().map_err(|e| format!("can't run rsync: {e}"))?;
    let got = supervise(child, prog, |line| {
        if let Some(f) = rsync_percent(line) {
            set_done(prog, lerp(RENDER_END, TOTAL - 5, f));
        }
    });
    let _ = output(remote(host, &format!("rm -rf {}", sq(&job_dir))), "cleaning up");
    if let Err(e) = got {
        let _ = std::fs::remove_file(&part);
        return Err(format!("copying the render back: {e}"));
    }
    std::fs::rename(&part, &plan.local_out).map_err(|e| format!("{}: {e}", plan.local_out))?;
    let bytes = std::fs::metadata(&plan.local_out).map(|m| m.len()).unwrap_or(0);
    set_done(prog, TOTAL);
    Ok(bytes)
}

/// `file.remoteHosts`: hosts from `~/.ssh/config` (no wildcards), for the Export dialog's list.
fn remote_hosts(_s: &mut Session, _p: &Value) -> Result<Value> {
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else { return Ok(json!([])) };
    let text = std::fs::read_to_string(Path::new(&home).join(".ssh").join("config")).unwrap_or_default();
    Ok(json!(ssh_config_hosts(&text)))
}

fn ssh_config_hosts(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines().take(10_000) {
        let mut words = line.split_whitespace();
        if !words.next().is_some_and(|k| k.eq_ignore_ascii_case("host")) {
            continue;
        }
        for h in words {
            if !h.contains(['*', '?', '!']) && valid_host(h) && !out.iter().any(|o| o == h) {
                out.push(h.to_string());
            }
        }
    }
    out
}

pub fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: CMD,
            label: "Render on Another Machine…",
            menu: &["File", "Export"],
            shortcut: None,
            params: r#"{"host":str,"path":str,"preset":str?,"format":str?,"settings":{}?,"sequence":id?,"range":"entire|inOut|workArea|custom"?,"remoteCli":str="filmcraft-cli","dryRun":bool=false,"wait":bool=false}"#,
            enabled: |s| if s.project.sequences().next().is_some() { Ok(()) } else { Err("the project has no sequences".into()) },
            run,
            journal: false,
            named: crate::commands::NamedKeys::NONE,
        },
        CommandSpec {
            id: "file.remoteHosts",
            label: "Remote Render Hosts",
            menu: &[],
            shortcut: None,
            params: "{}",
            enabled: |_| Ok(()),
            run: remote_hosts,
            journal: false,
            named: crate::commands::NamedKeys::NONE,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_quotes_and_spaces() {
        assert_eq!(sq("a b"), "'a b'");
        assert_eq!(sq("it's"), r"'it'\''s'");
    }

    #[test]
    fn hosts_never_look_like_options() {
        assert!(valid_host("pc"));
        assert!(valid_host("elyes@100.86.70.80"));
        assert!(!valid_host("-oProxyCommand=x"));
        assert!(!valid_host("a b"));
        assert!(!valid_host("a;rm"));
        assert!(!valid_host(""));
    }

    #[test]
    fn progress_lines_parse() {
        assert_eq!(cli_progress("progress 0.25"), Some(0.25));
        assert_eq!(cli_progress("progress 7"), Some(1.0));
        assert_eq!(cli_progress("progress NaN"), None);
        assert_eq!(rsync_percent("  1,234,567  42%   10.20MB/s    0:00:03"), Some(0.42));
        assert_eq!(rsync_percent("sending incremental file list"), None);
    }

    #[test]
    fn fingerprint_changes_with_the_file() {
        let a = fingerprint("/v/a.mp4", 10, 1);
        assert_eq!(a, fingerprint("/v/a.mp4", 10, 1));
        assert_ne!(a, fingerprint("/v/a.mp4", 11, 1));
        assert_ne!(a, fingerprint("/v/a.mp4", 10, 2));
        assert_eq!(safe_ext("/x/clip.MP4"), "mp4");
        assert_eq!(safe_ext("/x/clip"), "");
        assert_eq!(safe_ext("/x/a.m;p4"), "mp4");
    }

    #[test]
    fn ssh_config_lists_plain_hosts() {
        let cfg = "Host pc laptop\n  HostName 100.1.2.3\nHost *\n  ServerAliveInterval 30\nhost vps\nMatch host x\n";
        assert_eq!(ssh_config_hosts(cfg), vec!["pc", "laptop", "vps"]);
    }

    #[test]
    fn bad_hosts_are_refused_before_any_ssh() {
        let mut s = Session::default();
        let _ = s.execute("file.openDemoProject", json!({}));
        let e = s.execute(CMD, json!({"host": "-oProxyCommand=evil", "path": "/tmp/x.mp4", "format": "h264"}));
        assert!(e.is_err());
        let e = s.execute(CMD, json!({"path": "/tmp/x.mp4", "format": "h264"}));
        assert!(e.is_err());
    }

    #[test]
    fn dry_run_lists_what_would_be_sent() {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        let v = s.execute(CMD, json!({"host": "pc", "path": "/tmp/x.mp4", "format": "h264", "dryRun": true})).unwrap();
        assert_eq!(v["host"], "pc");
        assert_eq!(v["path"], "/tmp/x.mp4");
        // the demo project is generated footage: nothing to upload
        assert_eq!(v["uploadBytes"], 0);
    }
}
