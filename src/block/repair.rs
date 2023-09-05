use core::ops::Bound;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rand::Rng;
use tokio::fs;
use tokio::select;
use tokio::sync::mpsc;
use tokio::sync::watch;

use garage_util::background::*;
use garage_util::data::*;
use garage_util::error::*;
use garage_util::persister::PersisterShared;
use garage_util::time::*;
use garage_util::tranquilizer::Tranquilizer;

use crate::layout::*;
use crate::manager::*;

// Full scrub every 25 days with a random element of 10 days mixed in below
const SCRUB_INTERVAL: Duration = Duration::from_secs(3600 * 24 * 25);
// Scrub tranquility is initially set to 4, but can be changed in the CLI
// and the updated version is persisted over Garage restarts
const INITIAL_SCRUB_TRANQUILITY: u32 = 4;

// ---- ---- ----
// FIRST KIND OF REPAIR: FINDING MISSING BLOCKS/USELESS BLOCKS
// This is a one-shot repair operation that can be launched,
// checks everything, and then exits.
// ---- ---- ----

pub struct RepairWorker {
	manager: Arc<BlockManager>,
	next_start: Option<Hash>,
	block_iter: Option<BlockStoreIterator>,
}

impl RepairWorker {
	pub fn new(manager: Arc<BlockManager>) -> Self {
		Self {
			manager,
			next_start: None,
			block_iter: None,
		}
	}
}

#[async_trait]
impl Worker for RepairWorker {
	fn name(&self) -> String {
		"Block repair worker".into()
	}

	fn status(&self) -> WorkerStatus {
		match self.block_iter.as_ref() {
			None => {
				let idx_bytes = self
					.next_start
					.as_ref()
					.map(|x| x.as_slice())
					.unwrap_or(&[]);
				let idx_bytes = if idx_bytes.len() > 4 {
					&idx_bytes[..4]
				} else {
					idx_bytes
				};
				WorkerStatus {
					progress: Some("0.00%".into()),
					freeform: vec![format!(
						"Currently in phase 1, iterator position: {}",
						hex::encode(idx_bytes)
					)],
					..Default::default()
				}
			}
			Some(bi) => WorkerStatus {
				progress: Some(format!("{:.2}%", bi.progress() * 100.)),
				freeform: vec!["Currently in phase 2".into()],
				..Default::default()
			},
		}
	}

	async fn work(&mut self, _must_exit: &mut watch::Receiver<bool>) -> Result<WorkerState, Error> {
		match self.block_iter.as_mut() {
			None => {
				// Phase 1: Repair blocks from RC table.

				// We have to do this complicated two-step process where we first read a bunch
				// of hashes from the RC table, and then insert them in the to-resync queue,
				// because of SQLite. Basically, as long as we have an iterator on a DB table,
				// we can't do anything else on the DB. The naive approach (which we had previously)
				// of just iterating on the RC table and inserting items one to one in the resync
				// queue can't work here, it would just provoke a deadlock in the SQLite adapter code.
				// This is mostly because the Rust bindings for SQLite assume a worst-case scenario
				// where SQLite is not compiled in thread-safe mode, so we have to wrap everything
				// in a mutex (see db/sqlite_adapter.rs and discussion in PR #322).
				// TODO: maybe do this with tokio::task::spawn_blocking ?
				let mut batch_of_hashes = vec![];
				let start_bound = match self.next_start.as_ref() {
					None => Bound::Unbounded,
					Some(x) => Bound::Excluded(x.as_slice()),
				};
				for entry in self
					.manager
					.rc
					.rc
					.range::<&[u8], _>((start_bound, Bound::Unbounded))?
				{
					let (hash, _) = entry?;
					let hash = Hash::try_from(&hash[..]).unwrap();
					batch_of_hashes.push(hash);
					if batch_of_hashes.len() >= 1000 {
						break;
					}
				}
				if batch_of_hashes.is_empty() {
					// move on to phase 2
					self.block_iter = Some(BlockStoreIterator::new(&self.manager));
					return Ok(WorkerState::Busy);
				}

				for hash in batch_of_hashes.into_iter() {
					self.manager
						.resync
						.put_to_resync(&hash, Duration::from_secs(0))?;
					self.next_start = Some(hash)
				}

				Ok(WorkerState::Busy)
			}
			Some(bi) => {
				// Phase 2: Repair blocks actually on disk
				// Lists all blocks on disk and adds them to the resync queue.
				// This allows us to find blocks we are storing but don't actually need,
				// so that we can offload them if necessary and then delete them locally.
				if let Some((_path, hash)) = bi.next().await? {
					self.manager
						.resync
						.put_to_resync(&hash, Duration::from_secs(0))?;
					Ok(WorkerState::Busy)
				} else {
					Ok(WorkerState::Done)
				}
			}
		}
	}

	async fn wait_for_work(&mut self) -> WorkerState {
		unreachable!()
	}
}

// ---- ---- ----
// SECOND KIND OF REPAIR: SCRUBBING THE DATASTORE
// This is significantly more complex than the process above,
// as it is a continuously-running task that triggers automatically
// every SCRUB_INTERVAL, but can also be triggered manually
// and whose parameter (esp. speed) can be controlled at runtime.
// ---- ---- ----

mod v081 {
	use serde::{Deserialize, Serialize};

	#[derive(Serialize, Deserialize)]
	pub struct ScrubWorkerPersisted {
		pub tranquility: u32,
		pub(crate) time_last_complete_scrub: u64,
		pub(crate) corruptions_detected: u64,
	}

	impl garage_util::migrate::InitialFormat for ScrubWorkerPersisted {}
}

mod v082 {
	use garage_util::data::Hash;
	use serde::{Deserialize, Serialize};
	use std::path::PathBuf;

	use super::v081;

	#[derive(Serialize, Deserialize)]
	pub struct ScrubWorkerPersisted {
		pub tranquility: u32,
		pub(crate) time_last_complete_scrub: u64,
		pub(crate) time_next_run_scrub: u64,
		pub(crate) corruptions_detected: u64,
		#[serde(default)]
		pub(crate) checkpoint: Option<BlockStoreIterator>,
	}

	#[derive(Serialize, Deserialize, Clone)]
	pub struct BlockStoreIterator {
		pub todo: Vec<BsiTodo>,
	}

	#[derive(Serialize, Deserialize, Clone)]
	pub enum BsiTodo {
		Directory {
			path: PathBuf,
			progress_min: u64,
			progress_max: u64,
		},
		File {
			path: PathBuf,
			hash: Hash,
			progress: u64,
		},
	}

	impl garage_util::migrate::Migrate for ScrubWorkerPersisted {
		type Previous = v081::ScrubWorkerPersisted;
		const VERSION_MARKER: &'static [u8] = b"G082bswp";

		fn migrate(old: v081::ScrubWorkerPersisted) -> ScrubWorkerPersisted {
			use crate::repair::randomize_next_scrub_run_time;

			ScrubWorkerPersisted {
				tranquility: old.tranquility,
				time_last_complete_scrub: old.time_last_complete_scrub,
				time_next_run_scrub: randomize_next_scrub_run_time(old.time_last_complete_scrub),
				corruptions_detected: old.corruptions_detected,
				checkpoint: None,
			}
		}
	}
}

pub use v082::*;

pub struct ScrubWorker {
	manager: Arc<BlockManager>,
	rx_cmd: mpsc::Receiver<ScrubWorkerCommand>,

	work: ScrubWorkerState,
	tranquilizer: Tranquilizer,

	persister: PersisterShared<ScrubWorkerPersisted>,
}

fn randomize_next_scrub_run_time(timestamp: u64) -> u64 {
	// Take SCRUB_INTERVAL and mix in a random interval of 10 days to attempt to
	// balance scrub load across different cluster nodes.

	timestamp
		+ SCRUB_INTERVAL
			.saturating_add(Duration::from_secs(
				rand::thread_rng().gen_range(0..3600 * 24 * 10),
			))
			.as_millis() as u64
}

impl Default for ScrubWorkerPersisted {
	fn default() -> Self {
		ScrubWorkerPersisted {
			time_last_complete_scrub: 0,
			time_next_run_scrub: randomize_next_scrub_run_time(now_msec()),
			tranquility: INITIAL_SCRUB_TRANQUILITY,
			corruptions_detected: 0,
			checkpoint: None,
		}
	}
}

#[derive(Default)]
enum ScrubWorkerState {
	Running {
		iterator: BlockStoreIterator,
		// time of the last checkpoint
		t_cp: u64,
	},
	Paused {
		iterator: BlockStoreIterator,
		// time at which the scrub should be resumed
		t_resume: u64,
	},
	#[default]
	Finished,
}

#[derive(Debug)]
pub enum ScrubWorkerCommand {
	Start,
	Pause(Duration),
	Resume,
	Cancel,
}

impl ScrubWorker {
	pub(crate) fn new(
		manager: Arc<BlockManager>,
		rx_cmd: mpsc::Receiver<ScrubWorkerCommand>,
		persister: PersisterShared<ScrubWorkerPersisted>,
	) -> Self {
		let work = match persister.get_with(|x| x.checkpoint.clone()) {
			None => ScrubWorkerState::Finished,
			Some(iterator) => ScrubWorkerState::Running {
				iterator,
				t_cp: now_msec(),
			},
		};
		Self {
			manager,
			rx_cmd,
			work,
			tranquilizer: Tranquilizer::new(30),
			persister,
		}
	}

	async fn handle_cmd(&mut self, cmd: ScrubWorkerCommand) {
		match cmd {
			ScrubWorkerCommand::Start => {
				self.work = match std::mem::take(&mut self.work) {
					ScrubWorkerState::Finished => {
						info!("Scrub worker initializing, now performing datastore scrub");
						let iterator = BlockStoreIterator::new(&self.manager);
						if let Err(e) = self
							.persister
							.set_with(|x| x.checkpoint = Some(iterator.clone()))
						{
							error!("Could not save scrub checkpoint: {}", e);
						}
						ScrubWorkerState::Running {
							iterator,
							t_cp: now_msec(),
						}
					}
					work => {
						error!("Cannot start scrub worker: already running!");
						work
					}
				};
			}
			ScrubWorkerCommand::Pause(dur) => {
				self.work = match std::mem::take(&mut self.work) {
					ScrubWorkerState::Running { iterator, .. }
					| ScrubWorkerState::Paused { iterator, .. } => {
						if let Err(e) = self
							.persister
							.set_with(|x| x.checkpoint = Some(iterator.clone()))
						{
							error!("Could not save scrub checkpoint: {}", e);
						}
						ScrubWorkerState::Paused {
							iterator,
							t_resume: now_msec() + dur.as_millis() as u64,
						}
					}
					work => {
						error!("Cannot pause scrub worker: not running!");
						work
					}
				};
			}
			ScrubWorkerCommand::Resume => {
				self.work = match std::mem::take(&mut self.work) {
					ScrubWorkerState::Paused { iterator, .. } => ScrubWorkerState::Running {
						iterator,
						t_cp: now_msec(),
					},
					work => {
						error!("Cannot resume scrub worker: not paused!");
						work
					}
				};
			}
			ScrubWorkerCommand::Cancel => {
				self.work = match std::mem::take(&mut self.work) {
					ScrubWorkerState::Running { .. } | ScrubWorkerState::Paused { .. } => {
						ScrubWorkerState::Finished
					}
					work => {
						error!("Cannot cancel scrub worker: not running!");
						work
					}
				}
			}
		}
	}
}

#[async_trait]
impl Worker for ScrubWorker {
	fn name(&self) -> String {
		"Block scrub worker".into()
	}

	fn status(&self) -> WorkerStatus {
		let (corruptions_detected, tranquility, time_last_complete_scrub, time_next_run_scrub) =
			self.persister.get_with(|p| {
				(
					p.corruptions_detected,
					p.tranquility,
					p.time_last_complete_scrub,
					p.time_next_run_scrub,
				)
			});

		let mut s = WorkerStatus {
			persistent_errors: Some(corruptions_detected),
			tranquility: Some(tranquility),
			..Default::default()
		};
		match &self.work {
			ScrubWorkerState::Running { iterator, .. } => {
				s.progress = Some(format!("{:.2}%", iterator.progress() * 100.));
			}
			ScrubWorkerState::Paused { iterator, t_resume } => {
				s.progress = Some(format!("{:.2}%", iterator.progress() * 100.));
				s.freeform = vec![format!(
					"Scrub paused, resumes at {}",
					msec_to_rfc3339(*t_resume)
				)];
			}
			ScrubWorkerState::Finished => {
				s.freeform = vec![
					format!(
						"Last scrub completed at {}",
						msec_to_rfc3339(time_last_complete_scrub),
					),
					format!(
						"Next scrub scheduled for {}",
						msec_to_rfc3339(time_next_run_scrub)
					),
				];
			}
		}
		s
	}

	async fn work(&mut self, _must_exit: &mut watch::Receiver<bool>) -> Result<WorkerState, Error> {
		match self.rx_cmd.try_recv() {
			Ok(cmd) => self.handle_cmd(cmd).await,
			Err(mpsc::error::TryRecvError::Disconnected) => return Ok(WorkerState::Done),
			Err(mpsc::error::TryRecvError::Empty) => (),
		};

		match &mut self.work {
			ScrubWorkerState::Running { iterator, t_cp } => {
				self.tranquilizer.reset();
				let now = now_msec();

				if let Some((_path, hash)) = iterator.next().await? {
					match self.manager.read_block(&hash).await {
						Err(Error::CorruptData(_)) => {
							error!("Found corrupt data block during scrub: {:?}", hash);
							self.persister.set_with(|p| p.corruptions_detected += 1)?;
						}
						Err(e) => return Err(e),
						_ => (),
					};

					if now - *t_cp > 60 * 1000 {
						self.persister
							.set_with(|p| p.checkpoint = Some(iterator.clone()))?;
						*t_cp = now;
					}

					Ok(self
						.tranquilizer
						.tranquilize_worker(self.persister.get_with(|p| p.tranquility)))
				} else {
					let next_scrub_timestamp = randomize_next_scrub_run_time(now);

					self.persister.set_with(|p| {
						p.time_last_complete_scrub = now;
						p.time_next_run_scrub = next_scrub_timestamp;
						p.checkpoint = None;
					})?;
					self.work = ScrubWorkerState::Finished;
					self.tranquilizer.clear();

					info!(
						"Datastore scrub completed, next scrub scheduled for {}",
						msec_to_rfc3339(next_scrub_timestamp)
					);

					Ok(WorkerState::Idle)
				}
			}
			_ => Ok(WorkerState::Idle),
		}
	}

	async fn wait_for_work(&mut self) -> WorkerState {
		let (wait_until, command) = match &self.work {
			ScrubWorkerState::Running { .. } => return WorkerState::Busy,
			ScrubWorkerState::Paused { t_resume, .. } => (*t_resume, ScrubWorkerCommand::Resume),
			ScrubWorkerState::Finished => (
				self.persister.get_with(|p| p.time_next_run_scrub),
				ScrubWorkerCommand::Start,
			),
		};

		let now = now_msec();
		if now >= wait_until {
			self.handle_cmd(command).await;
			return WorkerState::Busy;
		}
		let delay = Duration::from_millis(wait_until - now);
		select! {
			_ = tokio::time::sleep(delay) => self.handle_cmd(command).await,
			cmd = self.rx_cmd.recv() => if let Some(cmd) = cmd {
				self.handle_cmd(cmd).await;
			} else {
				return WorkerState::Done;
			}
		}

		match &self.work {
			ScrubWorkerState::Running { .. } => WorkerState::Busy,
			_ => WorkerState::Idle,
		}
	}
}

// ---- ---- ----
// UTILITY FOR ENUMERATING THE BLOCK STORE
// ---- ---- ----

const PROGRESS_FP: u64 = 1_000_000_000;

impl BlockStoreIterator {
	fn new(manager: &BlockManager) -> Self {
		let min_cap = manager
			.data_layout
			.data_dirs
			.iter()
			.filter_map(|x| x.capacity())
			.min()
			.unwrap_or(0);

		let sum_cap = manager
			.data_layout
			.data_dirs
			.iter()
			.map(|x| x.capacity().unwrap_or(min_cap /* approximation */))
			.sum::<u64>() as u128;

		let mut cum_cap = 0;
		let mut todo = vec![];
		for dir in manager.data_layout.data_dirs.iter() {
			let cap = match dir.state {
				DataDirState::Active { capacity } => capacity,
				_ => min_cap,
			};

			let progress_min = ((cum_cap as u128 * PROGRESS_FP as u128) / (sum_cap as u128)) as u64;
			let progress_max =
				(((cum_cap + cap) as u128 * PROGRESS_FP as u128) / (sum_cap as u128)) as u64;
			cum_cap += cap;

			todo.push(BsiTodo::Directory {
				path: dir.path.clone(),
				progress_min,
				progress_max,
			});
		}
		// entries are processed back-to-front (because of .pop()),
		// so reverse entries to process them in increasing progress bounds
		todo.reverse();

		let ret = Self { todo };
		debug_assert!(ret.progress_invariant());

		ret
	}

	/// Returns progress done, between 0 and 1
	fn progress(&self) -> f32 {
		self.todo
			.last()
			.map(|x| match x {
				BsiTodo::Directory { progress_min, .. } => *progress_min,
				BsiTodo::File { progress, .. } => *progress,
			})
			.map(|x| x as f32 / PROGRESS_FP as f32)
			.unwrap_or(1.0)
	}

	async fn next(&mut self) -> Result<Option<(PathBuf, Hash)>, Error> {
		loop {
			match self.todo.pop() {
				None => return Ok(None),
				Some(BsiTodo::Directory {
					path,
					progress_min,
					progress_max,
				}) => {
					let istart = self.todo.len();

					let mut reader = fs::read_dir(&path).await?;
					while let Some(ent) = reader.next_entry().await? {
						let name = if let Ok(n) = ent.file_name().into_string() {
							n
						} else {
							continue;
						};
						let ft = ent.file_type().await?;
						if ft.is_dir() && hex::decode(&name).is_ok() {
							self.todo.push(BsiTodo::Directory {
								path: ent.path(),
								progress_min: 0,
								progress_max: 0,
							});
						} else if ft.is_file() {
							let filename = name.split_once('.').map(|(f, _)| f).unwrap_or(&name);
							if filename.len() == 64 {
								if let Ok(h) = hex::decode(filename) {
									let mut hash = [0u8; 32];
									hash.copy_from_slice(&h);
									self.todo.push(BsiTodo::File {
										path: ent.path(),
										hash: hash.into(),
										progress: 0,
									});
								}
							}
						}
					}

					let count = self.todo.len() - istart;
					for (i, ent) in self.todo[istart..].iter_mut().enumerate() {
						let p1 = progress_min
							+ ((progress_max - progress_min) * i as u64) / count as u64;
						let p2 = progress_min
							+ ((progress_max - progress_min) * (i + 1) as u64) / count as u64;
						match ent {
							BsiTodo::Directory {
								progress_min,
								progress_max,
								..
							} => {
								*progress_min = p1;
								*progress_max = p2;
							}
							BsiTodo::File { progress, .. } => {
								*progress = p1;
							}
						}
					}
					self.todo[istart..].reverse();
					debug_assert!(self.progress_invariant());
				}
				Some(BsiTodo::File { path, hash, .. }) => {
					return Ok(Some((path, hash)));
				}
			}
		}
	}

	// for debug_assert!
	fn progress_invariant(&self) -> bool {
		let iter = self.todo.iter().map(|x| match x {
			BsiTodo::Directory { progress_min, .. } => progress_min,
			BsiTodo::File { progress, .. } => progress,
		});
		let iter_1 = iter.clone().skip(1);
		iter.zip(iter_1).all(|(prev, next)| prev >= next)
	}
}
