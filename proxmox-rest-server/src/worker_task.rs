use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::panic::UnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{bail, format_err, Error};
use futures::*;
use lazy_static::lazy_static;
use nix::fcntl::OFlag;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::signal::unix::SignalKind;
use tokio::sync::oneshot;

use proxmox_lang::try_block;
use proxmox_schema::upid::UPID;
use proxmox_sys::fs::{atomic_open_or_create_file, create_path, replace_file, CreateOptions};
use proxmox_sys::linux::procfs;
use proxmox_sys::task_warn;

use proxmox_sys::logrotate::{LogRotate, LogRotateFiles};
use proxmox_sys::WorkerTaskContext;

use crate::{CommandSocket, FileLogOptions, FileLogger};

struct TaskListLockGuard(File);

struct WorkerTaskSetup {
    file_opts: CreateOptions,
    taskdir: PathBuf,
    task_lock_fn: PathBuf,
    active_tasks_fn: PathBuf,
    task_index_fn: PathBuf,
    task_archive_fn: PathBuf,
}

static WORKER_TASK_SETUP: OnceCell<WorkerTaskSetup> = OnceCell::new();

fn worker_task_setup() -> Result<&'static WorkerTaskSetup, Error> {
    WORKER_TASK_SETUP
        .get()
        .ok_or_else(|| format_err!("WorkerTask library is not initialized"))
}

impl WorkerTaskSetup {
    fn new(basedir: PathBuf, file_opts: CreateOptions) -> Self {
        let mut taskdir = basedir;
        taskdir.push("tasks");

        let mut task_lock_fn = taskdir.clone();
        task_lock_fn.push(".active.lock");

        let mut active_tasks_fn = taskdir.clone();
        active_tasks_fn.push("active");

        let mut task_index_fn = taskdir.clone();
        task_index_fn.push("index");

        let mut task_archive_fn = taskdir.clone();
        task_archive_fn.push("archive");

        Self {
            file_opts,
            taskdir,
            task_lock_fn,
            active_tasks_fn,
            task_index_fn,
            task_archive_fn,
        }
    }

    fn lock_task_list_files(&self, exclusive: bool) -> Result<TaskListLockGuard, Error> {
        let options = self
            .file_opts
            .clone()
            .perm(nix::sys::stat::Mode::from_bits_truncate(0o660));

        let timeout = std::time::Duration::new(10, 0);

        let file =
            proxmox_sys::fs::open_file_locked(&self.task_lock_fn, timeout, exclusive, options)?;

        Ok(TaskListLockGuard(file))
    }

    fn log_directory(&self, upid: &UPID) -> std::path::PathBuf {
        let mut path = self.taskdir.clone();
        path.push(format!("{:02X}", upid.pstart & 255));
        path
    }

    fn log_path(&self, upid: &UPID) -> std::path::PathBuf {
        let mut path = self.log_directory(upid);
        path.push(upid.to_string());
        path
    }

    fn create_and_get_log_path(&self, upid: &UPID) -> Result<std::path::PathBuf, Error> {
        let mut path = self.log_directory(upid);
        let dir_opts = self
            .file_opts
            .clone()
            .perm(nix::sys::stat::Mode::from_bits_truncate(0o755));

        create_path(&path, None, Some(dir_opts))?;

        path.push(upid.to_string());
        Ok(path)
    }

    // atomically read/update the task list, update status of finished tasks
    // new_upid is added to the list when specified.
    fn update_active_workers(&self, new_upid: Option<&UPID>) -> Result<(), Error> {
        let lock = self.lock_task_list_files(true)?;

        // TODO remove with 1.x
        let mut finish_list: Vec<TaskListInfo> = read_task_file_from_path(&self.task_index_fn)?;
        let had_index_file = !finish_list.is_empty();

        // We use filter_map because one negative case wants to *move* the data into `finish_list`,
        // clippy doesn't quite catch this!
        #[allow(clippy::unnecessary_filter_map)]
        let mut active_list: Vec<TaskListInfo> = read_task_file_from_path(&self.active_tasks_fn)?
            .into_iter()
            .filter_map(|info| {
                if info.state.is_some() {
                    // this can happen when the active file still includes finished tasks
                    finish_list.push(info);
                    return None;
                }

                if !worker_is_active_local(&info.upid) {
                    // println!("Detected stopped task '{}'", &info.upid_str);
                    let now = proxmox_time::epoch_i64();
                    let status =
                        upid_read_status(&info.upid).unwrap_or(TaskState::Unknown { endtime: now });
                    finish_list.push(TaskListInfo {
                        upid: info.upid,
                        upid_str: info.upid_str,
                        state: Some(status),
                    });
                    return None;
                }

                Some(info)
            })
            .collect();

        if let Some(upid) = new_upid {
            active_list.push(TaskListInfo {
                upid: upid.clone(),
                upid_str: upid.to_string(),
                state: None,
            });
        }

        let active_raw = render_task_list(&active_list);

        let options = self
            .file_opts
            .clone()
            .perm(nix::sys::stat::Mode::from_bits_truncate(0o660));

        replace_file(&self.active_tasks_fn, active_raw.as_bytes(), options, false)?;

        finish_list.sort_unstable_by(|a, b| match (&a.state, &b.state) {
            (Some(s1), Some(s2)) => s1.cmp(s2),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            _ => a.upid.starttime.cmp(&b.upid.starttime),
        });

        if !finish_list.is_empty() {
            let options = self
                .file_opts
                .clone()
                .perm(nix::sys::stat::Mode::from_bits_truncate(0o660));

            let mut writer = atomic_open_or_create_file(
                &self.task_archive_fn,
                OFlag::O_APPEND | OFlag::O_RDWR,
                &[],
                options,
                false,
            )?;
            for info in &finish_list {
                writer.write_all(render_task_line(info).as_bytes())?;
            }
        }

        // TODO Remove with 1.x
        // for compatibility, if we had an INDEX file, we do not need it anymore
        if had_index_file {
            let _ = nix::unistd::unlink(&self.task_index_fn);
        }

        drop(lock);

        Ok(())
    }

    // Create task log directory with correct permissions
    fn create_task_log_dirs(&self) -> Result<(), Error> {
        try_block!({
            let dir_opts = self
                .file_opts
                .clone()
                .perm(nix::sys::stat::Mode::from_bits_truncate(0o755));

            create_path(&self.taskdir, Some(dir_opts.clone()), Some(dir_opts))?;
            // fixme:??? create_path(pbs_buildcfg::PROXMOX_BACKUP_RUN_DIR, None, Some(opts))?;
            Ok(())
        })
        .map_err(|err: Error| format_err!("unable to create task log dir - {}", err))
    }
}

/// Initialize the WorkerTask library
pub fn init_worker_tasks(basedir: PathBuf, file_opts: CreateOptions) -> Result<(), Error> {
    let setup = WorkerTaskSetup::new(basedir, file_opts);
    setup.create_task_log_dirs()?;
    WORKER_TASK_SETUP
        .set(setup)
        .map_err(|_| format_err!("init_worker_tasks failed - already initialized"))
}

/// Optionally rotates and/or cleans up the task archive depending on its size and age.
///
/// Check if the current task-archive is bigger than 'size_threshold' bytes, and rotate in that
/// case. If the task archive is smaller, nothing will be done.
///
/// Retention is controlled by either 'max_days' or 'max_files', with 'max_days' having precedence.
///
/// If only 'max_files' is passed, all files coming latter than that will be deleted.
/// For 'max_days', the logs will be scanned until one is found that only has entries that are
/// older than the cut-off time of `now - max_days`. If such a older archive file is found, that
/// and all older ones will be deleted.
pub fn rotate_task_log_archive(
    size_threshold: u64,
    compress: bool,
    max_files: Option<usize>,
    max_days: Option<usize>,
    options: Option<CreateOptions>,
) -> Result<bool, Error> {
    let setup = worker_task_setup()?;

    let _lock = setup.lock_task_list_files(true)?;

    let max_files = if max_days.is_some() { None } else { max_files };

    let mut logrotate = LogRotate::new(&setup.task_archive_fn, compress, max_files, options)?;

    let mut rotated = logrotate.rotate(size_threshold)?;

    if let Some(max_days) = max_days {
        // NOTE: not on exact day-boundary but close enough for what's done here
        let cutoff_time = proxmox_time::epoch_i64() - (max_days * 24 * 60 * 60) as i64;
        let mut cutoff = false;
        let mut files = logrotate.files();
        // task archives have task-logs sorted by endtime, with the oldest at the start of the
        // file. So, peak into every archive and see if the first listed tasks' endtime would be
        // cut off. If that's the case we know that the next (older) task archive rotation surely
        // falls behind the cut-off line. We cannot say the same for the current archive without
        // checking its last (newest) line, but that's more complex, expensive and rather unlikely.
        for file_name in logrotate.file_names() {
            if !cutoff {
                let reader = match files.next() {
                    Some(file) => BufReader::new(file),
                    None => bail!("unexpected error: files do not match file_names"),
                };
                if let Some(Ok(line)) = reader.lines().next() {
                    if let Ok((_, _, Some(state))) = parse_worker_status_line(&line) {
                        if state.endtime() < cutoff_time {
                            // found first file with the oldest entry being cut-off, so next older
                            // ones are all up for deletion.
                            cutoff = true;
                            rotated = true;
                        }
                    }
                }
            } else if let Err(err) = std::fs::remove_file(&file_name) {
                log::error!("could not remove {:?}: {}", file_name, err);
            }
        }
    }

    Ok(rotated)
}

/// removes all task logs that are older than the oldest task entry in the
/// task archive
pub fn cleanup_old_tasks(worker: &dyn WorkerTaskContext, compressed: bool) -> Result<(), Error> {
    let setup = worker_task_setup()?;

    let _lock = setup.lock_task_list_files(true)?;

    let logrotate = LogRotate::new(&setup.task_archive_fn, compressed, None, None)?;

    let mut timestamp = None;
    if let Some(last_file) = logrotate.files().last() {
        let reader = BufReader::new(last_file);
        for line in reader.lines() {
            let line = line?;
            if let Ok((_, _, Some(state))) = parse_worker_status_line(&line) {
                timestamp = Some(state.endtime());
                break;
            }
        }
    }

    fn get_modified(entry: std::fs::DirEntry) -> Result<SystemTime, std::io::Error> {
        entry.metadata()?.modified()
    }

    if let Some(timestamp) = timestamp {
        let cutoff_time = if timestamp > 0 {
            SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(timestamp as u64))
        } else {
            SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(-timestamp as u64))
        }
        .ok_or_else(|| format_err!("could not calculate cutoff time"))?;

        for i in 0..256 {
            let mut path = setup.taskdir.clone();
            path.push(format!("{:02X}", i));
            let files = match std::fs::read_dir(path) {
                Ok(files) => files,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    task_warn!(worker, "could not check task logs in '{:02X}': {}", i, err);
                    continue;
                }
            };
            for file in files {
                let file = match file {
                    Ok(file) => file,
                    Err(err) => {
                        task_warn!(
                            worker,
                            "could not check some task log in '{:02X}': {}",
                            i,
                            err
                        );
                        continue;
                    }
                };
                let path = file.path();

                let modified = match get_modified(file) {
                    Ok(modified) => modified,
                    Err(err) => {
                        task_warn!(worker, "error getting mtime for '{:?}': {}", path, err);
                        continue;
                    }
                };

                if modified < cutoff_time {
                    match std::fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => {
                            task_warn!(worker, "could not remove file '{:?}': {}", path, err)
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Path to the worker log file
pub fn upid_log_path(upid: &UPID) -> Result<std::path::PathBuf, Error> {
    let setup = worker_task_setup()?;
    Ok(setup.log_path(upid))
}

/// Parse the time and exit status from the last log line in a worker task log file. Works only
/// correctly on finished tasks.
///
/// For parsing the task log file is opened, then seek'd to the end - 8 KiB as performance
/// optimization with the assumption that any last-line will fit in that range, and then the last
/// line in that range is searched and then parsed.
///
/// Note, this should be avoided for tasks that are still active, as then the 'endtime' and
/// exit-status might be wrong, e.g., if a log line resembles a exit status message by accident.
///
/// Further, if no line starting with a valid date-time is found in the trailing 8 KiB, it can only
/// be due to either being called on a still running task (e.g., with no output yet) by mistake, or
/// because the task was (unsafely) interrupted, e.g., due to a power loss. In that case the
/// end-time is also set to the start-time.
pub fn upid_read_status(upid: &UPID) -> Result<TaskState, Error> {
    let setup = worker_task_setup()?;

    let path = setup.log_path(upid);
    let mut file = File::open(path)?;

    /// speedup - only read tail
    use std::io::Seek;
    use std::io::SeekFrom;
    let _ = file.seek(SeekFrom::End(-8192)); // ignore errors

    let mut data = Vec::with_capacity(8192);
    file.read_to_end(&mut data)?;

    // strip newlines at the end of the task logs
    while data.last() == Some(&b'\n') {
        data.pop();
    }

    let last_line = match data.iter().rposition(|c| *c == b'\n') {
        Some(start) if data.len() > (start + 1) => &data[start + 1..],
        Some(_) => &data, // should not happen, since we removed all trailing newlines
        None => &data,
    };

    let last_line = std::str::from_utf8(last_line)
        .map_err(|err| format_err!("upid_read_status: utf8 parse failed: {}", err))?;

    let mut endtime = upid.starttime; // as fallback
    let mut iter = last_line.splitn(2, ": ");
    if let Some(time_str) = iter.next() {
        if let Ok(parsed_endtime) = proxmox_time::parse_rfc3339(time_str) {
            endtime = parsed_endtime; // save last found time for when the state cannot be parsed
            if let Some(rest) = iter.next().and_then(|rest| rest.strip_prefix("TASK ")) {
                if let Ok(state) = TaskState::from_endtime_and_message(parsed_endtime, rest) {
                    return Ok(state);
                }
            }
        }
    }

    Ok(TaskState::Unknown { endtime }) // no last line with both, end-time and task-state, found.
}

lazy_static! {
    static ref WORKER_TASK_LIST: Mutex<HashMap<usize, Arc<WorkerTask>>> =
        Mutex::new(HashMap::new());
}

/// checks if the task UPID refers to a worker from this process
fn is_local_worker(upid: &UPID) -> bool {
    upid.pid == crate::pid() && upid.pstart == crate::pstart()
}

/// Test if the task is still running
pub async fn worker_is_active(upid: &UPID) -> Result<bool, Error> {
    if is_local_worker(upid) {
        return Ok(WORKER_TASK_LIST.lock().unwrap().contains_key(&upid.task_id));
    }

    if procfs::check_process_running_pstart(upid.pid, upid.pstart).is_none() {
        return Ok(false);
    }

    let sock = crate::ctrl_sock_from_pid(upid.pid);
    let cmd = json!({
        "command": "worker-task-status",
        "args": {
            "upid": upid.to_string(),
        },
    });
    let status = crate::send_command(sock, &cmd).await?;

    if let Some(active) = status.as_bool() {
        Ok(active)
    } else {
        bail!("got unexpected result {:?} (expected bool)", status);
    }
}

/// Test if the task is still running (fast but inaccurate implementation)
///
/// If the task is spawned from a different process, we simply return if
/// that process is still running. This information is good enough to detect
/// stale tasks...
pub fn worker_is_active_local(upid: &UPID) -> bool {
    if is_local_worker(upid) {
        WORKER_TASK_LIST.lock().unwrap().contains_key(&upid.task_id)
    } else {
        procfs::check_process_running_pstart(upid.pid, upid.pstart).is_some()
    }
}

/// Register task control command on a [CommandSocket].
///
/// This create two commands:
///
/// * ``worker-task-abort <UPID>``: calls [abort_local_worker]
///
/// * ``worker-task-status <UPID>``: return true of false, depending on
/// whether the worker is running or stopped.
pub fn register_task_control_commands(commando_sock: &mut CommandSocket) -> Result<(), Error> {
    fn get_upid(args: Option<&Value>) -> Result<UPID, Error> {
        let args = if let Some(args) = args {
            args
        } else {
            bail!("missing args")
        };
        let upid = match args.get("upid") {
            Some(Value::String(upid)) => upid.parse::<UPID>()?,
            None => bail!("no upid in args"),
            _ => bail!("unable to parse upid"),
        };
        if !is_local_worker(&upid) {
            bail!("upid does not belong to this process");
        }
        Ok(upid)
    }

    commando_sock.register_command("worker-task-abort".into(), move |args| {
        let upid = get_upid(args)?;

        abort_local_worker(upid);

        Ok(Value::Null)
    })?;
    commando_sock.register_command("worker-task-status".into(), move |args| {
        let upid = get_upid(args)?;

        let active = WORKER_TASK_LIST.lock().unwrap().contains_key(&upid.task_id);

        Ok(active.into())
    })?;

    Ok(())
}

/// Try to abort a worker task, but do no wait
///
/// Errors (if any) are simply logged.
pub fn abort_worker_nowait(upid: UPID) {
    tokio::spawn(async move {
        if let Err(err) = abort_worker(upid).await {
            log::error!("abort worker task failed - {}", err);
        }
    });
}

/// Abort a worker task
///
/// By sending ``worker-task-abort`` to the control socket.
pub async fn abort_worker(upid: UPID) -> Result<(), Error> {
    let sock = crate::ctrl_sock_from_pid(upid.pid);
    let cmd = json!({
        "command": "worker-task-abort",
        "args": {
            "upid": upid.to_string(),
        },
    });
    crate::send_command(sock, &cmd).map_ok(|_| ()).await
}

fn parse_worker_status_line(line: &str) -> Result<(String, UPID, Option<TaskState>), Error> {
    let data = line.splitn(3, ' ').collect::<Vec<&str>>();

    let len = data.len();

    match len {
        1 => Ok((data[0].to_owned(), data[0].parse::<UPID>()?, None)),
        3 => {
            let endtime = i64::from_str_radix(data[1], 16)?;
            let state = TaskState::from_endtime_and_message(endtime, data[2])?;
            Ok((data[0].to_owned(), data[0].parse::<UPID>()?, Some(state)))
        }
        _ => bail!("wrong number of components"),
    }
}

/// Task State
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    /// The Task ended with an undefined state
    Unknown { endtime: i64 },
    /// The Task ended and there were no errors or warnings
    OK { endtime: i64 },
    /// The Task had 'count' amount of warnings and no errors
    Warning { count: u64, endtime: i64 },
    /// The Task ended with the error described in 'message'
    Error { message: String, endtime: i64 },
}

impl TaskState {
    pub fn endtime(&self) -> i64 {
        match *self {
            TaskState::Unknown { endtime } => endtime,
            TaskState::OK { endtime } => endtime,
            TaskState::Warning { endtime, .. } => endtime,
            TaskState::Error { endtime, .. } => endtime,
        }
    }

    fn result_text(&self) -> String {
        match self {
            TaskState::Error { message, .. } => format!("TASK ERROR: {}", message),
            other => format!("TASK {}", other),
        }
    }

    fn from_endtime_and_message(endtime: i64, s: &str) -> Result<Self, Error> {
        if s == "unknown" {
            Ok(TaskState::Unknown { endtime })
        } else if s == "OK" {
            Ok(TaskState::OK { endtime })
        } else if let Some(warnings) = s.strip_prefix("WARNINGS: ") {
            let count: u64 = warnings.parse()?;
            Ok(TaskState::Warning { count, endtime })
        } else if !s.is_empty() {
            let message = if let Some(err) = s.strip_prefix("ERROR: ") {
                err
            } else {
                s
            }
            .to_string();
            Ok(TaskState::Error { message, endtime })
        } else {
            bail!("unable to parse Task Status '{}'", s);
        }
    }
}

impl std::cmp::PartialOrd for TaskState {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.endtime().cmp(&other.endtime()))
    }
}

impl std::cmp::Ord for TaskState {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.endtime().cmp(&other.endtime())
    }
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskState::Unknown { .. } => write!(f, "unknown"),
            TaskState::OK { .. } => write!(f, "OK"),
            TaskState::Warning { count, .. } => write!(f, "WARNINGS: {}", count),
            TaskState::Error { message, .. } => write!(f, "{}", message),
        }
    }
}

/// Task details including parsed UPID
///
/// If there is no `state`, the task is still running.
#[derive(Debug)]
pub struct TaskListInfo {
    /// The parsed UPID
    pub upid: UPID,
    /// UPID string representation
    pub upid_str: String,
    /// Task `(endtime, status)` if already finished
    pub state: Option<TaskState>, // endtime, status
}

fn render_task_line(info: &TaskListInfo) -> String {
    let mut raw = String::new();
    if let Some(status) = &info.state {
        use std::fmt::Write as _;

        let _ = writeln!(raw, "{} {:08X} {}", info.upid_str, status.endtime(), status);
    } else {
        raw.push_str(&info.upid_str);
        raw.push('\n');
    }

    raw
}

fn render_task_list(list: &[TaskListInfo]) -> String {
    let mut raw = String::new();
    for info in list {
        raw.push_str(&render_task_line(info));
    }
    raw
}

// note this is not locked, caller has to make sure it is
// this will skip (and log) lines that are not valid status lines
fn read_task_file<R: Read>(reader: R) -> Result<Vec<TaskListInfo>, Error> {
    let reader = BufReader::new(reader);
    let mut list = Vec::new();
    for line in reader.lines() {
        let line = line?;
        match parse_worker_status_line(&line) {
            Ok((upid_str, upid, state)) => list.push(TaskListInfo {
                upid_str,
                upid,
                state,
            }),
            Err(err) => {
                log::warn!("unable to parse worker status '{}' - {}", line, err);
                continue;
            }
        };
    }

    Ok(list)
}

// note this is not locked, caller has to make sure it is
fn read_task_file_from_path<P>(path: P) -> Result<Vec<TaskListInfo>, Error>
where
    P: AsRef<std::path::Path> + std::fmt::Debug,
{
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => bail!("unable to open task list {:?} - {}", path, err),
    };

    read_task_file(file)
}

/// Iterate over existing/active worker tasks
pub struct TaskListInfoIterator {
    list: VecDeque<TaskListInfo>,
    end: bool,
    archive: Option<LogRotateFiles>,
    lock: Option<TaskListLockGuard>,
}

impl TaskListInfoIterator {
    /// Creates a new iterator instance.
    pub fn new(active_only: bool) -> Result<Self, Error> {
        let setup = worker_task_setup()?;

        let (read_lock, active_list) = {
            let lock = setup.lock_task_list_files(false)?;
            let active_list = read_task_file_from_path(&setup.active_tasks_fn)?;

            let needs_update = active_list
                .iter()
                .any(|info| info.state.is_some() || !worker_is_active_local(&info.upid));

            // TODO remove with 1.x
            let index_exists = setup.task_index_fn.is_file();

            if needs_update || index_exists {
                drop(lock);
                setup.update_active_workers(None)?;
                let lock = setup.lock_task_list_files(false)?;
                let active_list = read_task_file_from_path(&setup.active_tasks_fn)?;
                (lock, active_list)
            } else {
                (lock, active_list)
            }
        };

        let archive = if active_only {
            None
        } else {
            let logrotate = LogRotate::new(&setup.task_archive_fn, true, None, None)?;
            Some(logrotate.files())
        };

        let lock = if active_only { None } else { Some(read_lock) };

        Ok(Self {
            list: active_list.into(),
            end: active_only,
            archive,
            lock,
        })
    }
}

impl Iterator for TaskListInfoIterator {
    type Item = Result<TaskListInfo, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(element) = self.list.pop_back() {
                return Some(Ok(element));
            } else if self.end {
                return None;
            } else {
                if let Some(mut archive) = self.archive.take() {
                    if let Some(file) = archive.next() {
                        let list = match read_task_file(file) {
                            Ok(list) => list,
                            Err(err) => return Some(Err(err)),
                        };
                        self.list.append(&mut list.into());
                        self.archive = Some(archive);
                        continue;
                    }
                }

                self.end = true;
                self.lock.take();
            }
        }
    }
}

/// Launch long running worker tasks.
///
/// A worker task can either be a whole thread, or a simply tokio
/// task/future. Each task can `log()` messages, which are stored
/// persistently to files. Task should poll the `abort_requested`
/// flag, and stop execution when requested.
pub struct WorkerTask {
    setup: &'static WorkerTaskSetup,
    upid: UPID,
    data: Mutex<WorkerTaskData>,
    abort_requested: AtomicBool,
}

impl std::fmt::Display for WorkerTask {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.upid.fmt(f)
    }
}

struct WorkerTaskData {
    logger: FileLogger,
    progress: f64, // 0..1
    warn_count: u64,
    pub abort_listeners: Vec<oneshot::Sender<()>>,
}

impl WorkerTask {
    pub fn new(
        worker_type: &str,
        worker_id: Option<String>,
        auth_id: String,
        to_stdout: bool,
    ) -> Result<Arc<Self>, Error> {
        let setup = worker_task_setup()?;

        let upid = UPID::new(worker_type, worker_id, auth_id)?;
        let task_id = upid.task_id;

        let path = setup.create_and_get_log_path(&upid)?;

        let logger_options = FileLogOptions {
            to_stdout,
            exclusive: true,
            prefix_time: true,
            read: true,
            file_opts: setup.file_opts.clone(),
            ..Default::default()
        };
        let logger = FileLogger::new(path, logger_options)?;

        let worker = Arc::new(Self {
            setup,
            upid: upid.clone(),
            abort_requested: AtomicBool::new(false),
            data: Mutex::new(WorkerTaskData {
                logger,
                progress: 0.0,
                warn_count: 0,
                abort_listeners: vec![],
            }),
        });

        // scope to drop the lock again after inserting
        {
            let mut hash = WORKER_TASK_LIST.lock().unwrap();
            hash.insert(task_id, worker.clone());
            crate::set_worker_count(hash.len());
        }

        setup.update_active_workers(Some(&upid))?;

        Ok(worker)
    }

    /// Spawn a new tokio task/future.
    pub fn spawn<F, T>(
        worker_type: &str,
        worker_id: Option<String>,
        auth_id: String,
        to_stdout: bool,
        f: F,
    ) -> Result<String, Error>
    where
        F: Send + 'static + FnOnce(Arc<WorkerTask>) -> T,
        T: Send + 'static + Future<Output = Result<(), Error>>,
    {
        let worker = WorkerTask::new(worker_type, worker_id, auth_id, to_stdout)?;
        let upid_str = worker.upid.to_string();
        let f = f(worker.clone());
        tokio::spawn(async move {
            let result = f.await;
            worker.log_result(&result);
        });

        Ok(upid_str)
    }

    /// Create a new worker thread.
    pub fn new_thread<F>(
        worker_type: &str,
        worker_id: Option<String>,
        auth_id: String,
        to_stdout: bool,
        f: F,
    ) -> Result<String, Error>
    where
        F: Send + UnwindSafe + 'static + FnOnce(Arc<WorkerTask>) -> Result<(), Error>,
    {
        let worker = WorkerTask::new(worker_type, worker_id, auth_id, to_stdout)?;
        let upid_str = worker.upid.to_string();

        let _child = std::thread::Builder::new()
            .name(upid_str.clone())
            .spawn(move || {
                let worker1 = worker.clone();
                let result = match std::panic::catch_unwind(move || f(worker1)) {
                    Ok(r) => r,
                    Err(panic) => match panic.downcast::<&str>() {
                        Ok(panic_msg) => Err(format_err!("worker panicked: {}", panic_msg)),
                        Err(_) => Err(format_err!("worker panicked: unknown type.")),
                    },
                };

                worker.log_result(&result);
            });

        Ok(upid_str)
    }

    /// create state from self and a result
    pub fn create_state(&self, result: &Result<(), Error>) -> TaskState {
        let warn_count = self.data.lock().unwrap().warn_count;

        let endtime = proxmox_time::epoch_i64();

        if let Err(err) = result {
            TaskState::Error {
                message: err.to_string(),
                endtime,
            }
        } else if warn_count > 0 {
            TaskState::Warning {
                count: warn_count,
                endtime,
            }
        } else {
            TaskState::OK { endtime }
        }
    }

    /// Log task result, remove task from running list
    pub fn log_result(&self, result: &Result<(), Error>) {
        let state = self.create_state(result);
        self.log_message(state.result_text());

        WORKER_TASK_LIST.lock().unwrap().remove(&self.upid.task_id);
        let _ = self.setup.update_active_workers(None);
        crate::set_worker_count(WORKER_TASK_LIST.lock().unwrap().len());
    }

    /// Log a message.
    pub fn log_message<S: AsRef<str>>(&self, msg: S) {
        let mut data = self.data.lock().unwrap();
        data.logger.log(msg);
    }

    /// Log a message as warning.
    pub fn log_warning<S: AsRef<str>>(&self, msg: S) {
        let mut data = self.data.lock().unwrap();
        data.logger.log(format!("WARN: {}", msg.as_ref()));
        data.warn_count += 1;
    }

    /// Set progress indicator
    pub fn progress(&self, progress: f64) {
        if (0.0..=1.0).contains(&progress) {
            let mut data = self.data.lock().unwrap();
            data.progress = progress;
        } else {
            // fixme:  log!("task '{}': ignoring strange value for progress '{}'", self.upid, progress);
        }
    }

    /// Request abort
    pub fn request_abort(&self) {
        let prev_abort = self.abort_requested.swap(true, Ordering::SeqCst);
        if !prev_abort {
            self.log_message("received abort request ..."); // log abort only once
        }
        // noitify listeners
        let mut data = self.data.lock().unwrap();
        loop {
            match data.abort_listeners.pop() {
                None => {
                    break;
                }
                Some(ch) => {
                    let _ = ch.send(()); // ignore errors here
                }
            }
        }
    }

    /// Get a future which resolves on task abort
    pub fn abort_future(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel::<()>();

        let mut data = self.data.lock().unwrap();
        if self.abort_requested() {
            let _ = tx.send(());
        } else {
            data.abort_listeners.push(tx);
        }
        rx
    }

    pub fn upid(&self) -> &UPID {
        &self.upid
    }
}

impl WorkerTaskContext for WorkerTask {
    fn abort_requested(&self) -> bool {
        self.abort_requested.load(Ordering::SeqCst)
    }

    fn shutdown_requested(&self) -> bool {
        crate::shutdown_requested()
    }

    fn fail_on_shutdown(&self) -> Result<(), Error> {
        crate::fail_on_shutdown()
    }

    fn log(&self, level: log::Level, message: &std::fmt::Arguments) {
        match level {
            log::Level::Error => self.log_warning(message.to_string()),
            log::Level::Warn => self.log_warning(message.to_string()),
            log::Level::Info => self.log_message(message.to_string()),
            log::Level::Debug => self.log_message(format!("DEBUG: {}", message)),
            log::Level::Trace => self.log_message(format!("TRACE: {}", message)),
        }
    }
}

/// Wait for a locally spanned worker task
///
/// Note: local workers should print logs to stdout, so there is no
/// need to fetch/display logs. We just wait for the worker to finish.
pub async fn wait_for_local_worker(upid_str: &str) -> Result<(), Error> {
    let upid: UPID = upid_str.parse()?;

    let sleep_duration = core::time::Duration::new(0, 100_000_000);

    loop {
        if worker_is_active_local(&upid) {
            tokio::time::sleep(sleep_duration).await;
        } else {
            break;
        }
    }
    Ok(())
}

/// Request abort of a local worker (if existing and running)
pub fn abort_local_worker(upid: UPID) {
    if let Some(worker) = WORKER_TASK_LIST.lock().unwrap().get(&upid.task_id) {
        worker.request_abort();
    }
}

/// Wait for locally running worker, responding to SIGINT properly
pub async fn handle_worker(upid_str: &str) -> Result<(), Error> {
    let upid: UPID = upid_str.parse()?;
    let mut signal_stream = tokio::signal::unix::signal(SignalKind::interrupt())?;
    let abort_future = async move {
        while signal_stream.recv().await.is_some() {
            println!("got shutdown request (SIGINT)");
            abort_local_worker(upid.clone());
        }
        Ok::<_, Error>(())
    };

    let result_future = wait_for_local_worker(upid_str);

    futures::select! {
        result = result_future.fuse() => result?,
        abort = abort_future.fuse() => abort?,
    };

    Ok(())
}
