//! Child processes of sources, and stopping them together with everything
//! they started.
//!
//! On Unix each child leads its own process group, which receives SIGTERM.
//! On Windows each child is put in a job object that kills its processes
//! when terminated or when its last handle closes, so children also die when
//! lmatrix itself is killed or its console window is closed.

use std::io;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Mutex;

/// Handle that stops a child and everything it started. Cheap to clone and
/// safe to use from other threads, also after the child has exited.
#[derive(Clone)]
pub struct ChildGroup(platform::Group);

impl ChildGroup {
	/// Stops every process of the group.
	pub fn terminate(&self) {
		self.0.terminate();
	}

	fn same_as(&self, other: &ChildGroup) -> bool {
		self.0.same_as(&other.0)
	}
}

/// Groups of children that are still running, stopped by
/// [`terminate_children`].
static CHILD_GROUPS: Mutex<Vec<ChildGroup>> = Mutex::new(Vec::new());

fn register_child_group(group: &ChildGroup) {
	if let Ok(mut groups) = CHILD_GROUPS.lock() {
		groups.push(group.clone());
	}
}

fn unregister_child_group(group: &ChildGroup) {
	if let Ok(mut groups) = CHILD_GROUPS.lock() {
		groups.retain(|registered| !registered.same_as(group));
	}
}

/// Stops every registered child group.
pub fn terminate_children() {
	let groups = match CHILD_GROUPS.lock() {
		Ok(mut groups) => std::mem::take(&mut *groups),
		Err(_) => return,
	};
	for group in groups {
		group.terminate();
	}
}

/// A child process that is stopped together with everything it started.
///
/// The group is registered while the child runs, so [`terminate_children`]
/// also reaches its descendants. Dropping a child that has not been waited
/// for terminates its group and reaps it.
pub struct GroupChild {
	child: Child,
	group: ChildGroup,
	reaped: bool,
}

impl GroupChild {
	/// Spawns `command` with stdin closed and stdout and stderr piped, in a
	/// new group.
	pub fn spawn(command: &mut Command) -> io::Result<Self> {
		command
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped());
		let (child, group) = platform::spawn(command)?;
		let group = ChildGroup(group);
		register_child_group(&group);
		Ok(Self {
			child,
			group,
			reaped: false,
		})
	}

	/// Handle for stopping the child from another thread.
	pub fn group(&self) -> ChildGroup {
		self.group.clone()
	}

	/// Takes the read end of the child's stdout.
	pub fn take_stdout(&mut self) -> Option<ChildStdout> {
		self.child.stdout.take()
	}

	/// Takes the read end of the child's stderr.
	pub fn take_stderr(&mut self) -> Option<ChildStderr> {
		self.child.stderr.take()
	}

	/// Stops the whole group.
	pub fn terminate(&self) {
		if !self.reaped {
			self.group.terminate();
		}
	}

	/// Waits for the child to exit and forgets its group.
	pub fn wait(&mut self) -> io::Result<ExitStatus> {
		let status = self.child.wait();
		self.reaped = true;
		unregister_child_group(&self.group);
		status
	}
}

impl Drop for GroupChild {
	fn drop(&mut self) {
		if !self.reaped {
			self.terminate();
			let _ = self.wait();
		}
	}
}

#[cfg(unix)]
mod platform {
	use std::io;
	use std::os::unix::process::CommandExt;
	use std::process::{Child, Command};

	/// Process group id (equal to the pid of the child that leads it).
	#[derive(Clone)]
	pub struct Group(i32);

	impl Group {
		/// Sends SIGTERM to the process group.
		pub fn terminate(&self) {
			if self.0 > 0 {
				// SAFETY: kill() has no memory-safety preconditions; a
				// negative pid addresses the process group we created.
				unsafe {
					libc::kill(-self.0, libc::SIGTERM);
				}
			}
		}

		pub fn same_as(&self, other: &Group) -> bool {
			self.0 == other.0
		}
	}

	/// Spawns the child as the leader of a new process group.
	pub fn spawn(command: &mut Command) -> io::Result<(Child, Group)> {
		let child = command.process_group(0).spawn()?;
		let pgid = child.id() as i32;
		Ok((child, Group(pgid)))
	}
}

#[cfg(windows)]
mod platform {
	use std::io;
	use std::os::windows::io::AsRawHandle;
	use std::os::windows::process::CommandExt;
	use std::process::{Child, Command};
	use std::sync::Arc;

	use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
	use windows_sys::Win32::System::JobObjects::{
		AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
		JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
		SetInformationJobObject, TerminateJobObject,
	};
	use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

	/// An owned job object handle, closed on drop. Closing the last handle
	/// kills the processes still in the job.
	struct Job(HANDLE);

	// SAFETY: a job object handle may be used and closed from any thread.
	unsafe impl Send for Job {}
	// SAFETY: the job functions used here are thread-safe.
	unsafe impl Sync for Job {}

	impl Job {
		fn new() -> io::Result<Self> {
			// SAFETY: no security attributes and no name are valid arguments.
			let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
			if handle.is_null() {
				return Err(io::Error::last_os_error());
			}
			let job = Job(handle);
			// SAFETY: an all-zero JOBOBJECT_EXTENDED_LIMIT_INFORMATION is a
			// valid value.
			let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
			limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
			// SAFETY: the handle is valid and the pointer and size describe
			// `limits`.
			let set = unsafe {
				SetInformationJobObject(
					job.0,
					JobObjectExtendedLimitInformation,
					(&raw const limits).cast(),
					size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
				)
			};
			if set == 0 {
				return Err(io::Error::last_os_error());
			}
			Ok(job)
		}
	}

	impl Drop for Job {
		fn drop(&mut self) {
			// SAFETY: the handle is owned by this value and closed once.
			unsafe {
				CloseHandle(self.0);
			}
		}
	}

	#[derive(Clone)]
	pub struct Group(Arc<Job>);

	impl Group {
		/// Kills every process in the job.
		pub fn terminate(&self) {
			// SAFETY: the handle stays open while this Arc exists.
			unsafe {
				TerminateJobObject(self.0.0, 1);
			}
		}

		pub fn same_as(&self, other: &Group) -> bool {
			Arc::ptr_eq(&self.0, &other.0)
		}
	}

	/// Spawns the child without a console of its own (so it cannot change
	/// the mode of lmatrix's console) and puts it in a new job object.
	pub fn spawn(command: &mut Command) -> io::Result<(Child, Group)> {
		let job = Job::new()?;
		let mut child = command.creation_flags(CREATE_NO_WINDOW).spawn()?;
		// SAFETY: both handles are valid for the duration of the call.
		if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) } == 0 {
			let error = io::Error::last_os_error();
			let _ = child.kill();
			let _ = child.wait();
			return Err(error);
		}
		Ok((child, Group(Arc::new(job))))
	}
}
