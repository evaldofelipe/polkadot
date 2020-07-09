// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Implements a `CandidateBackingSubsystem`.

#![recursion_limit="256"]

use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bitvec::vec::BitVec;
use log;
use futures::{
	select, FutureExt, SinkExt, StreamExt,
	channel::{oneshot, mpsc},
	future::{self, Either},
	task::{Spawn, SpawnError, SpawnExt},
};
use futures_timer::Delay;
use streamunordered::{StreamUnordered, StreamYield};

use keystore::KeyStorePtr;
use polkadot_primitives::{
	Hash,
	parachain::{
		AbridgedCandidateReceipt, BackedCandidate, Id as ParaId, ValidatorId,
		ValidatorIndex, HeadData, SigningContext, PoVBlock, OmittedValidationData,
		CandidateDescriptor, LocalValidationData, GlobalValidationSchedule, AvailableData,
		ErasureChunk,
	},
};
use polkadot_node_primitives::{
	FromTableMisbehavior, Statement, SignedFullStatement, MisbehaviorReport, ValidationResult,
};
use polkadot_subsystem::{
	FromOverseer, OverseerSignal, Subsystem, SubsystemContext, SpawnedSubsystem,
	util::{
		self,
		request_head_data,
		request_signing_context,
		request_validator_groups,
		request_validators,
		Validator,
	},
};
use polkadot_subsystem::messages::{
	AllMessages, CandidateBackingMessage, CandidateSelectionMessage,
	RuntimeApiMessage, CandidateValidationMessage, ValidationFailed,
	StatementDistributionMessage, NewBackedCandidate, ProvisionerMessage, ProvisionableData,
	PoVDistributionMessage, AvailabilityStoreMessage,
};
use statement_table::{
	generic::AttestedCandidate as TableAttestedCandidate,
	Table, Context as TableContextTrait, Statement as TableStatement,
	SignedStatement as TableSignedStatement, Summary as TableSummary,
};

#[derive(Debug, derive_more::From)]
enum Error {
	CandidateNotFound,
	JobNotFound(Hash),
	InvalidSignature,
	#[from]
	Erasure(erasure_coding::Error),
	#[from]
	ValidationFailed(ValidationFailed),
	#[from]
	Oneshot(oneshot::Canceled),
	#[from]
	Mpsc(mpsc::SendError),
	#[from]
	Spawn(SpawnError),
	#[from]
	UtilError(util::Error),
}

/// Holds all data needed for candidate backing job operation.
struct CandidateBackingJob {
	/// The hash of the relay parent on top of which this job is doing it's work.
	parent: Hash,
	/// Inbound message channel receiving part.
	rx_to: mpsc::Receiver<ToJob>,
	/// Outbound message channel sending part.
	tx_from: mpsc::Sender<FromJob>,

	/// `HeadData`s of the parachains that this validator is assigned to.
	head_data: HeadData,
	/// The `ParaId`s assigned to this validator.
	assignment: ParaId,
	/// We issued `Valid` or `Invalid` statements on about these candidates.
	issued_validity: HashSet<Hash>,
	/// `Some(h)` if this job has already issues `Seconded` statemt for some candidate with `h` hash.
	seconded: Option<Hash>,
	/// We have already reported misbehaviors for these validators.
	reported_misbehavior_for: HashSet<ValidatorIndex>,

	table: Table<TableContext>,
	table_context: TableContext,
}

const fn group_quorum(n_validators: usize) -> usize {
	(n_validators / 2) + 1
}

#[derive(Default)]
struct TableContext {
	signing_context: SigningContext,
	validator: Option<Validator>,
	groups: HashMap<ParaId, Vec<ValidatorIndex>>,
	validators: Vec<ValidatorId>,
}

impl TableContextTrait for TableContext {
	fn is_member_of(&self, authority: ValidatorIndex, group: &ParaId) -> bool {
		self.groups.get(group).map_or(false, |g| g.iter().position(|&a| a == authority).is_some())
	}

	fn requisite_votes(&self, group: &ParaId) -> usize {
		self.groups.get(group).map_or(usize::max_value(), |g| group_quorum(g.len()))
	}
}

const CHANNEL_CAPACITY: usize = 64;

/// A message type that is sent from `CandidateBackingSubsystem` to `CandidateBackingJob`.
enum ToJob {
	/// A `CandidateBackingMessage`.
	CandidateBacking(CandidateBackingMessage),
	/// Stop working.
	Stop,
}

/// A message type that is sent from `CandidateBackingJob` to `CandidateBackingSubsystem`.
enum FromJob {
	AvailabilityStore(AvailabilityStoreMessage),
	RuntimeApiMessage(RuntimeApiMessage),
	CandidateValidation(CandidateValidationMessage),
	CandidateSelection(CandidateSelectionMessage),
	Provisioner(ProvisionerMessage),
	PoVDistribution(PoVDistributionMessage),
	StatementDistribution(StatementDistributionMessage),
}

impl From<FromJob> for AllMessages {
	fn from(f: FromJob) -> Self {
		match f {
			FromJob::AvailabilityStore(msg) => AllMessages::AvailabilityStore(msg),
			FromJob::RuntimeApiMessage(msg) => AllMessages::RuntimeApi(msg),
			FromJob::CandidateValidation(msg) => AllMessages::CandidateValidation(msg),
			FromJob::CandidateSelection(msg) => AllMessages::CandidateSelection(msg),
			FromJob::StatementDistribution(msg) => AllMessages::StatementDistribution(msg),
			FromJob::PoVDistribution(msg) => AllMessages::PoVDistribution(msg),
			FromJob::Provisioner(msg) => AllMessages::Provisioner(msg),
		}
	}
}

impl TryFrom<AllMessages> for FromJob {
	type Error = &'static str;

	fn try_from(f: AllMessages) -> Result<Self, Self::Error> {
		match f {
			AllMessages::AvailabilityStore(msg) => Ok(FromJob::AvailabilityStore(msg)),
			AllMessages::RuntimeApi(msg) => Ok(FromJob::RuntimeApiMessage(msg)),
			AllMessages::CandidateValidation(msg) => Ok(FromJob::CandidateValidation(msg)),
			AllMessages::CandidateSelection(msg) => Ok(FromJob::CandidateSelection(msg)),
			AllMessages::StatementDistribution(msg) => Ok(FromJob::StatementDistribution(msg)),
			AllMessages::PoVDistribution(msg) => Ok(FromJob::PoVDistribution(msg)),
			AllMessages::Provisioner(msg) => Ok(FromJob::Provisioner(msg)),
			_ => Err("can't convert this AllMessages variant to FromJob"),
		}
	}
}

// It looks like it's not possible to do an `impl From` given the current state of
// the code. So this does the necessary conversion.
fn primitive_statement_to_table(s: &SignedFullStatement) -> TableSignedStatement {
	let statement = match s.payload() {
		Statement::Seconded(c) => TableStatement::Candidate(c.clone()),
		Statement::Valid(h) => TableStatement::Valid(h.clone()),
		Statement::Invalid(h) => TableStatement::Invalid(h.clone()),
	};

	TableSignedStatement {
		statement,
		signature: s.signature().clone(),
		sender: s.validator_index(),
	}
}

impl CandidateBackingJob {
	/// Run asynchronously.
	async fn run(mut self) -> Result<(), Error> {
		while let Some(msg) = self.rx_to.next().await {
			match msg {
				ToJob::CandidateBacking(msg) => {
					self.process_msg(msg).await?;
				}
				_ => break,
			}
		}

		Ok(())
	}

	async fn issue_candidate_invalid_message(
		&mut self,
		candidate: AbridgedCandidateReceipt,
	) -> Result<(), Error> {
		self.tx_from.send(FromJob::CandidateSelection(
			CandidateSelectionMessage::Invalid(self.parent, candidate)
		)).await?;

		Ok(())
	}

	/// Validate the candidate that is requested to be `Second`ed and distribute validation result.
	async fn validate_and_second(
		&mut self,
		candidate: AbridgedCandidateReceipt,
		pov: Arc<PoVBlock>,
	) -> Result<ValidationResult, Error> {
		let (valid, global_validation_schedule, local_validation_data) = self.request_candidate_validation(candidate.clone(), pov.clone()).await?;
		let statement = match valid {
			ValidationResult::Valid => {
				// make PoV available for later distribution. Send data to the availability
				// store to keep. Sign and dispatch `valid` statement to network if we
				// have not seconded the given candidate.
				self.make_pov_available(pov, global_validation_schedule, local_validation_data).await?;
				self.issued_validity.insert(candidate.hash());
				Statement::Seconded(candidate)
			}
			ValidationResult::Invalid => {
				let candidate_hash = candidate.hash();
				self.issue_candidate_invalid_message(candidate).await?;
				Statement::Invalid(candidate_hash)
			}
		};

		if let Some(signed_statement) = self.sign_statement(statement) {
			self.import_statement(&signed_statement).await?;
			self.distribute_signed_statement(signed_statement).await?;
		}

		Ok(valid)
	}

	async fn get_backed(&self, mut tx: mpsc::Sender<NewBackedCandidate>) -> Result<(), Error> {
		let proposed = self.table.proposed_candidates(&self.table_context);

		for TableAttestedCandidate {
			candidate,
			validity_votes,
			..
		} in proposed.into_iter()
		{
			let (ids, validity_votes): (Vec<_>, Vec<_>) = validity_votes
						.into_iter()
						.map(|(id, vote)| (id, vote.into()))
						.unzip();

			let group = match self.table_context.groups.get(&self.assignment) {
				Some(group) => group,
				None => continue,
			};

			let mut validator_indices = BitVec::with_capacity(group.len());

			validator_indices.resize(group.len(), false);

			for id in ids.iter() {
				if let Some(position) = group.iter().position(|x| x == id) {
					validator_indices.set(position, true);
				}
			}

			tx.send(NewBackedCandidate(BackedCandidate {
				candidate,
				validity_votes,
				validator_indices,
			})).await?;
		}

		Ok(())
	}

	/// Check if there have happened any new misbehaviors and issue necessary messages.
	async fn issue_new_misbehaviors(&mut self) -> Result<(), Error> {
		let mut reports = Vec::new();

		for (k, v) in self.table.get_misbehavior().iter() {
			if !self.reported_misbehavior_for.contains(k) {
				self.reported_misbehavior_for.insert(*k);

				let f = FromTableMisbehavior {
					id: *k,
					report: v.clone(),
					signing_context: self.table_context.signing_context.clone(),
					key: self.table_context.validators[*k as usize].clone(),
				};

				if let Ok(report) = MisbehaviorReport::try_from(f) {
					let message = ProvisionerMessage::ProvisionableData(
						ProvisionableData::MisbehaviorReport(self.parent, report)
					);

					reports.push(message);
				}
			}
		}

		for report in reports.drain(..) {
			self.send_to_provisioner(report).await?
		}

		Ok(())
	}

	/// Import a statement into the statement table and return the summary of the import.
	async fn import_statement(
		&mut self,
		statement: &SignedFullStatement,
	) -> Result<Option<TableSummary>, Error> {
		let stmt = primitive_statement_to_table(statement);

		let summary = self.table.import_statement(&self.table_context, stmt);

		self.issue_new_misbehaviors().await?;

		return Ok(summary);
	}

	async fn process_msg(&mut self, msg: CandidateBackingMessage) -> Result<(), Error> {
		match msg {
			CandidateBackingMessage::Second(_, candidate, pov) => {
				// If the message is a `CandidateBackingMessage::Second`, sign and dispatch a
				// Seconded statement only if we have not seconded any other candidate and
				// have not signed a Valid statement for the requested candidate.
				match self.seconded {
					// This job has not seconded a candidate yet.
					None => {
						let candidate_hash = candidate.hash();

						if !self.issued_validity.contains(&candidate_hash) {
							if let Ok(ValidationResult::Valid) = self.validate_and_second(
								candidate,
								pov,
							).await {
								self.seconded = Some(candidate_hash);
							}
						}
					}
					// This job has already seconded a candidate.
					Some(_) => {}
				}
			}
			CandidateBackingMessage::Statement(_, statement) => {
				self.check_statement_signature(&statement)?;
				self.maybe_validate_and_import(statement).await?;
			}
			CandidateBackingMessage::RegisterBackingWatcher(_, tx) => {
				self.get_backed(tx).await?;
			}
		}

		Ok(())
	}

	/// Kick off validation work and distribute the result as a signed statement.
	async fn kick_off_validation_work(
		&mut self,
		summary: TableSummary,
	) -> Result<ValidationResult, Error> {
		let candidate = self.table.get_candidate(&summary.candidate).ok_or(Error::CandidateNotFound)?;
		let candidate = candidate.clone();
		let descriptor = candidate.to_descriptor();
		let candidate_hash = candidate.hash();
		let pov = self.request_pov_from_distribution(descriptor).await?;
		let (valid, _, _) = self.request_candidate_validation(candidate, pov).await?;

		let statement = match valid {
			ValidationResult::Valid => Statement::Valid(candidate_hash),
			ValidationResult::Invalid => Statement::Invalid(candidate_hash),
		};

		self.issued_validity.insert(candidate_hash);

		if let Some(signed_statement) = self.sign_statement(statement) {
			self.distribute_signed_statement(signed_statement).await?;
		}

		Ok(valid)
	}

	/// Import the statement and kick off validation work if it is a part of our assignment.
	async fn maybe_validate_and_import(
		&mut self,
		statement: SignedFullStatement,
	) -> Result<(), Error> {
		if let Some(summary) = self.import_statement(&statement).await? {
			if let Statement::Seconded(_) = statement.payload() {
				if summary.group_id == self.assignment {
					self.kick_off_validation_work(summary).await?;
				}
			}
		}

		Ok(())
	}

	fn sign_statement(&self, statement: Statement) -> Option<SignedFullStatement> {
		Some(self.table_context.validator.as_ref()?.sign(statement))
	}

	fn check_statement_signature(&self, statement: &SignedFullStatement) -> Result<(), Error> {
		let idx = statement.validator_index() as usize;

		if self.table_context.validators.len() > idx {
			statement.check_signature(
				&self.table_context.signing_context,
				&self.table_context.validators[idx],
			).map_err(|_| Error::InvalidSignature)?;
		} else {
			return Err(Error::InvalidSignature);
		}

		Ok(())
	}

	async fn send_to_provisioner(&mut self, msg: ProvisionerMessage) -> Result<(), Error> {
		self.tx_from.send(FromJob::Provisioner(msg)).await?;

		Ok(())
	}

	async fn request_pov_from_distribution(
		&mut self,
		descriptor: CandidateDescriptor,
	) -> Result<Arc<PoVBlock>, Error> {
		let (tx, rx) = oneshot::channel();

		self.tx_from.send(FromJob::PoVDistribution(
			PoVDistributionMessage::FetchPoV(self.parent, descriptor, tx)
		)).await?;

		Ok(rx.await?)
	}

	async fn request_candidate_validation(
		&mut self,
		candidate: AbridgedCandidateReceipt,
		pov: Arc<PoVBlock>,
	) -> Result<(ValidationResult, GlobalValidationSchedule, LocalValidationData), Error> {
		let (tx, rx) = oneshot::channel();

		self.tx_from.send(FromJob::CandidateValidation(
				CandidateValidationMessage::Validate(
					self.parent,
					candidate,
					self.head_data.clone(),
					pov,
					tx,
				)
			)
		).await?;

		Ok(rx.await??)
	}

	async fn store_chunk(
		&mut self,
		id: ValidatorIndex,
		chunk: ErasureChunk,
	) -> Result<(), Error> {
		self.tx_from.send(FromJob::AvailabilityStore(
				AvailabilityStoreMessage::StoreChunk(self.parent, id, chunk)
			)
		).await?;

		Ok(())
	}

	async fn make_pov_available(
		&mut self,
		pov_block: Arc<PoVBlock>,
		global_validation: GlobalValidationSchedule,
		local_validation: LocalValidationData,
	) -> Result<(), Error> {
		let omitted_validation = OmittedValidationData {
			global_validation,
			local_validation,
		};

		let available_data = AvailableData {
			pov_block: pov_block.as_ref().clone(),
			omitted_validation,
		};

		let chunks = erasure_coding::obtain_chunks(
			self.table_context.validators.len(),
			&available_data,
		)?;

		let branches = erasure_coding::branches(chunks.as_ref());

		for (index, (chunk, proof)) in chunks.iter().zip(branches.map(|(proof, _)| proof)).enumerate() {
			let chunk = ErasureChunk {
				chunk: chunk.clone(),
				index: index as u32,
				proof,
			};

			self.store_chunk(index as ValidatorIndex, chunk).await?;
		}

		Ok(())
	}

	async fn distribute_signed_statement(&mut self, s: SignedFullStatement) -> Result<(), Error> {
		let smsg = StatementDistributionMessage::Share(self.parent, s);

		self.tx_from.send(FromJob::StatementDistribution(smsg)).await?;

		Ok(())
	}
}

struct JobHandle {
	abort_handle: future::AbortHandle,
	to_job: mpsc::Sender<ToJob>,
	finished: oneshot::Receiver<()>,
	su_handle: usize,
}

impl JobHandle {
	async fn stop(mut self) {
		let _ = self.to_job.send(ToJob::Stop).await;
		let stop_timer = Delay::new(Duration::from_secs(1));

		match future::select(stop_timer, self.finished).await {
			Either::Left((_, _)) => {}
			Either::Right((_, _)) => {
				self.abort_handle.abort();
			}
		}
	}

	async fn send_msg(&mut self, msg: ToJob) -> Result<(), Error> {
		Ok(self.to_job.send(msg).await?)
	}
}

struct Jobs<S> {
	spawner: S,
	running: HashMap<Hash, JobHandle>,
	outgoing_msgs: StreamUnordered<mpsc::Receiver<FromJob>>,
}

async fn run_job(
	parent: Hash,
	keystore: KeyStorePtr,
	rx_to: mpsc::Receiver<ToJob>,
	mut tx_from: mpsc::Sender<FromJob>,
) -> Result<(), Error> {
	let (validators, roster, signing_context) = futures::try_join!(
		request_validators(parent, &mut tx_from).await?,
		request_validator_groups(parent, &mut tx_from).await?,
		request_signing_context(parent, &mut tx_from).await?,
	)?;

	let validator = Validator::construct(&validators, signing_context, keystore.clone())?;

	let mut groups = HashMap::new();

	for assignment in roster.scheduled {
		if let Some(g) = roster.validator_groups.get(assignment.group_idx.0 as usize) {
			groups.insert(
				assignment.para_id,
				g.clone(),
			);
		}
	}

	let mut assignment = Default::default();

	if let Some(idx) = validators.iter().position(|k| *k == validator.id()) {
		let idx = idx as u32;
		for (para_id, group) in groups.iter() {
			if group.contains(&idx) {
				assignment = *para_id;
				break;
			}
		}
	}

	let head_data = request_head_data(parent, &mut tx_from, assignment).await?.await?;

	let table_context = TableContext {
		groups,
		validators,
		signing_context: validator.signing_context().clone(),
		validator: Some(validator),
	};

	let job = CandidateBackingJob {
		parent,
		rx_to,
		tx_from,
		head_data,
		assignment,
		issued_validity: HashSet::new(),
		seconded: None,
		reported_misbehavior_for: HashSet::new(),
		table: Table::default(),
		table_context,
	};

	job.run().await
}

impl<S: Spawn> Jobs<S> {
	fn new(spawner: S) -> Self {
		Self {
			spawner,
			running: HashMap::default(),
			outgoing_msgs: StreamUnordered::new(),
		}
	}

	fn spawn_job(&mut self, parent_hash: Hash, keystore: KeyStorePtr) -> Result<(), Error> {
		let (to_job_tx, to_job_rx) = mpsc::channel(CHANNEL_CAPACITY);
		let (from_job_tx, from_job_rx) = mpsc::channel(CHANNEL_CAPACITY);

		let (future, abort_handle) = future::abortable(async move {
			if let Err(e) = run_job(parent_hash, keystore, to_job_rx, from_job_tx).await {
				log::error!(
					"CandidateBackingJob({}) finished with an error {:?}",
					parent_hash,
					e,
				);
			}
		});

		let (finished_tx, finished) = oneshot::channel();

		let future = async move {
			let _ = future.await;
			let _ = finished_tx.send(());
		};
		self.spawner.spawn(future)?;

		let su_handle = self.outgoing_msgs.push(from_job_rx);

		let handle = JobHandle {
			abort_handle,
			to_job: to_job_tx,
			finished,
			su_handle,
		};

		self.running.insert(parent_hash, handle);

		Ok(())
	}

	async fn stop_job(&mut self, parent_hash: Hash) -> Result<(), Error> {
		match self.running.remove(&parent_hash) {
			Some(handle) => {
				Pin::new(&mut self.outgoing_msgs).remove(handle.su_handle);
				handle.stop().await;
				Ok(())
			}
			None => Err(Error::JobNotFound(parent_hash))
		}
	}

	async fn send_msg(&mut self, parent_hash: Hash, msg: ToJob) -> Result<(), Error> {
		if let Some(job) = self.running.get_mut(&parent_hash) {
			job.send_msg(msg).await?;
		}
		Ok(())
	}

	async fn next(&mut self) -> Option<FromJob> {
		self.outgoing_msgs.next().await.and_then(|(e, _)| match e {
			StreamYield::Item(e) => Some(e),
			_ => None,
		})
	}
}

/// An implementation of the Candidate Backing subsystem.
pub struct CandidateBackingSubsystem<S, Context> {
	spawner: S,
	keystore: KeyStorePtr,
	_context: std::marker::PhantomData<Context>,
}

impl<S, Context> CandidateBackingSubsystem<S, Context>
	where
		S: Spawn + Clone,
		Context: SubsystemContext<Message=CandidateBackingMessage>,
{
	/// Creates a new `CandidateBackingSubsystem`.
	pub fn new(keystore: KeyStorePtr, spawner: S) -> Self {
		Self {
			spawner,
			keystore,
			_context: std::marker::PhantomData,
		}
	}

	async fn run(
		mut ctx: Context,
		keystore: KeyStorePtr,
		spawner: S,
	) {
		let mut jobs = Jobs::new(spawner.clone());

		loop {
			select! {
				incoming = ctx.recv().fuse() => {
					match incoming {
						Ok(msg) => match msg {
							FromOverseer::Signal(OverseerSignal::StartWork(hash)) => {
								if let Err(e) = jobs.spawn_job(hash, keystore.clone()) {
									log::error!("Failed to spawn a job: {:?}", e);
									break;
								}
							}
							FromOverseer::Signal(OverseerSignal::StopWork(hash)) => {
								if let Err(e) = jobs.stop_job(hash).await {
									log::error!("Failed to spawn a job: {:?}", e);
									break;
								}
							}
							FromOverseer::Communication { msg } => {
								match msg {
									CandidateBackingMessage::Second(hash, _, _) |
									CandidateBackingMessage::Statement(hash, _) |
									CandidateBackingMessage::RegisterBackingWatcher(hash, _) => {
										let res = jobs.send_msg(
											hash.clone(),
											ToJob::CandidateBacking(msg),
										).await;

										if let Err(e) = res {
											log::error!(
												"Failed to send a message to a job: {:?}",
												e,
											);

											break;
										}
									}
									_ => (),
								}
							}
							_ => (),
						},
						Err(_) => break,
					}
				}
				outgoing = jobs.next().fuse() => {
					match outgoing {
						Some(msg) => {
							let _ = ctx.send_message(msg.into()).await;
						}
						None => break,
					}
				}
				complete => break,
			}
		}
	}
}

impl<S, Context> Subsystem<Context> for CandidateBackingSubsystem<S, Context>
	where
		S: Spawn + Send + Clone + 'static,
		Context: SubsystemContext<Message=CandidateBackingMessage>,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let keystore = self.keystore.clone();
		let spawner = self.spawner.clone();

		SpawnedSubsystem(Box::pin(async move {
			Self::run(ctx, keystore, spawner).await;
		}))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use assert_matches::assert_matches;
	use futures::{
		executor::{self, ThreadPool},
		Future,
	};
	use polkadot_primitives::parachain::{
		AssignmentKind, BlockData, CollatorId, CoreAssignment, CoreIndex, GroupIndex,
		ValidatorPair, ValidityAttestation,
	};
	use polkadot_subsystem::messages::{RuntimeApiRequest, SchedulerRoster};
	use sp_keyring::Sr25519Keyring;
	use std::collections::HashMap;

	fn validator_pubkeys(val_ids: &[Sr25519Keyring]) -> Vec<ValidatorId> {
		val_ids.iter().map(|v| v.public().into()).collect()
	}

	struct TestState {
		chain_ids: Vec<ParaId>,
		keystore: KeyStorePtr,
		validators: Vec<Sr25519Keyring>,
		validator_public: Vec<ValidatorId>,
		global_validation_schedule: GlobalValidationSchedule,
		local_validation_data: LocalValidationData,
		roster: SchedulerRoster,
		head_data: HashMap<ParaId, HeadData>,
		signing_context: SigningContext,
		relay_parent: Hash,
	}

	impl Default for TestState {
		fn default() -> Self {
			let chain_a = ParaId::from(1);
			let chain_b = ParaId::from(2);
			let thread_a = ParaId::from(3);

			let chain_ids = vec![chain_a, chain_b, thread_a];

			let validators = vec![
				Sr25519Keyring::Alice,
				Sr25519Keyring::Bob,
				Sr25519Keyring::Charlie,
				Sr25519Keyring::Dave,
				Sr25519Keyring::Ferdie,
			];

			let keystore = keystore::Store::new_in_memory();
			// Make sure `Alice` key is in the keystore, so this mocked node will be a parachain validator.
			keystore.write().insert_ephemeral_from_seed::<ValidatorPair>(&validators[0].to_seed())
				.expect("Insert key into keystore");

			let validator_public = validator_pubkeys(&validators);

			let chain_a_assignment = CoreAssignment {
				core: CoreIndex::from(0),
				para_id: chain_a,
				kind: AssignmentKind::Parachain,
				group_idx: GroupIndex::from(0),
			};

			let chain_b_assignment = CoreAssignment {
				core: CoreIndex::from(1),
				para_id: chain_b,
				kind: AssignmentKind::Parachain,
				group_idx: GroupIndex::from(1),
			};

			let thread_collator: CollatorId = Sr25519Keyring::Two.public().into();

			let thread_a_assignment = CoreAssignment {
				core: CoreIndex::from(2),
				para_id: thread_a,
				kind: AssignmentKind::Parathread(thread_collator.clone(), 0),
				group_idx: GroupIndex::from(2),
			};

			let validator_groups = vec![vec![2, 0, 3], vec![1], vec![4]];

			let parent_hash_1 = [1; 32].into();

			let roster = SchedulerRoster {
				validator_groups,
				scheduled: vec![
					chain_a_assignment,
					chain_b_assignment,
					thread_a_assignment,
				],
				upcoming: vec![],
				availability_cores: vec![],
			};
			let signing_context = SigningContext {
				session_index: 1,
				parent_hash: parent_hash_1,
			};

			let mut head_data = HashMap::new();
			head_data.insert(chain_a, HeadData(vec![4, 5, 6]));

			let relay_parent = Hash::from([5; 32]);

			let local_validation_data = LocalValidationData {
				parent_head: HeadData(vec![7, 8, 9]),
				balance: Default::default(),
				code_upgrade_allowed: None,
			};

			let global_validation_schedule = GlobalValidationSchedule {
				max_code_size: 1000,
				max_head_data_size: 1000,
				block_number: Default::default(),
			};

			Self {
				chain_ids,
				keystore,
				validators,
				validator_public,
				roster,
				head_data,
				local_validation_data,
				global_validation_schedule,
				signing_context,
				relay_parent,
			}
		}
	}

	struct TestHarness {
		virtual_overseer: subsystem_test::TestSubsystemContextHandle<CandidateBackingMessage>,
	}

	fn test_harness<T: Future<Output=()>>(keystore: KeyStorePtr, test: impl FnOnce(TestHarness) -> T) {
		let pool = ThreadPool::new().unwrap();

		let (context, virtual_overseer) = subsystem_test::make_subsystem_context(pool.clone());

		let subsystem = CandidateBackingSubsystem::run(context, keystore, pool.clone());

		let test_fut = test(TestHarness {
			virtual_overseer,
		});

		futures::pin_mut!(test_fut);
		futures::pin_mut!(subsystem);

		executor::block_on(future::select(test_fut, subsystem));
	}

	// Tests that the subsystem performs actions that are requied on startup.
	async fn test_startup(
		virtual_overseer: &mut subsystem_test::TestSubsystemContextHandle<CandidateBackingMessage>,
		test_state: &TestState,
	) {
		// Start work on some new parent.
		virtual_overseer.send(FromOverseer::Signal(
			OverseerSignal::StartWork(test_state.relay_parent))
		).await;

		// Check that subsystem job issues a request for a validator set.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::Validators(tx))
			) if parent == test_state.relay_parent => {
				tx.send(test_state.validator_public.clone()).unwrap();
			}
		);

		// Check that subsystem job issues a request for the validator groups.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::ValidatorGroups(tx))
			) if parent == test_state.relay_parent => {
				tx.send(test_state.roster.clone()).unwrap();
			}
		);

		// Check that subsystem job issues a request for the signing context.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::SigningContext(tx))
			) if parent == test_state.relay_parent => {
				tx.send(test_state.signing_context.clone()).unwrap();
			}
		);

		// Check that subsystem job issues a request for the head data.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::HeadData(id, tx))
			) if parent == test_state.relay_parent => {
				tx.send(test_state.head_data.get(&id).unwrap().clone()).unwrap();
			}
		);
	}

	// Test that a `CandidateBackingMessage::Second` issues validation work
	// and in case validation is successful issues a `StatementDistributionMessage`.
	#[test]
	fn backing_second_works() {
		let test_state = TestState::default();
		test_harness(test_state.keystore.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;

			test_startup(&mut virtual_overseer, &test_state).await;

			let pov_block = Arc::new(PoVBlock {
				block_data: BlockData(vec![42, 43, 44]),
			});

			let pov_block_hash = pov_block.hash();
			let candidate = AbridgedCandidateReceipt {
				parachain_index: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_block_hash,
				..Default::default()
			};

			let second = CandidateBackingMessage::Second(
				test_state.relay_parent,
				candidate.clone(),
				pov_block.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: second }).await;

			let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::Validate(
						parent_hash,
						c,
						head_data,
						pov,
						tx,
					)
				) if parent_hash == test_state.relay_parent &&
					pov == pov_block && c == candidate => {
					assert_eq!(head_data, *expected_head_data);
					tx.send(Ok((
						ValidationResult::Valid,
						test_state.global_validation_schedule,
						test_state.local_validation_data,
					))).unwrap();
				}
			);

			for _ in 0..test_state.validators.len() {
				assert_matches!(
					virtual_overseer.recv().await,
					AllMessages::AvailabilityStore(
						AvailabilityStoreMessage::StoreChunk(parent_hash, _, _)
					) if parent_hash == test_state.relay_parent
				);
			}

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::StatementDistribution(
					StatementDistributionMessage::Share(
						parent_hash,
						signed_statement,
					)
				) if parent_hash == test_state.relay_parent => {
					signed_statement.check_signature(
						&test_state.signing_context,
						&test_state.validator_public[0],
					).unwrap();
				}
			);

			virtual_overseer.send(FromOverseer::Signal(
				OverseerSignal::StopWork(test_state.relay_parent))
			).await;
		});
	}

	// Test that the candidate reaches quorum succesfully.
	#[test]
	fn backing_works() {
		let test_state = TestState::default();
		test_harness(test_state.keystore.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;

			test_startup(&mut virtual_overseer, &test_state).await;

			let pov_block = Arc::new(PoVBlock {
				block_data: BlockData(vec![1, 2, 3]),
			});

			let pov_block_hash = pov_block.hash();

			let candidate_a = AbridgedCandidateReceipt {
				parachain_index: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_block_hash,
				..Default::default()
			};

			let candidate_a_hash = candidate_a.hash();

			let signed_a = SignedFullStatement::sign(
				Statement::Seconded(candidate_a.clone()),
				&test_state.signing_context,
				2,
				&test_state.validators[2].pair().into(),
			);

			let signed_b = SignedFullStatement::sign(
				Statement::Valid(candidate_a_hash),
				&test_state.signing_context,
				0,
				&test_state.validators[0].pair().into(),
			);

			let statement = CandidateBackingMessage::Statement(test_state.relay_parent, signed_a.clone());

			virtual_overseer.send(FromOverseer::Communication{ msg: statement }).await;

			// Sending a `Statement::Seconded` for our assignment will start
			// validation process. The first thing requested is PoVBlock from the
			// `PoVDistribution`.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::PoVDistribution(
					PoVDistributionMessage::FetchPoV(relay_parent, _, tx)
				) if relay_parent == test_state.relay_parent => {
					tx.send(pov_block.clone()).unwrap();
				}
			);

			let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

			// The next step is the actual request to Validation subsystem
			// to validate the `Seconded` candidate.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::Validate(
						relay_parent,
						candidate,
						head_data,
						pov,
						tx,
					)
				) if relay_parent == test_state.relay_parent && candidate == candidate_a => {
					assert_eq!(head_data, *expected_head_data);
					assert_eq!(pov, pov_block);
					tx.send(Ok((
						ValidationResult::Valid,
						test_state.global_validation_schedule,
						test_state.local_validation_data,
					))).unwrap();
				}
			);

			let statement = CandidateBackingMessage::Statement(
				test_state.relay_parent,
				signed_b.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: statement }).await;

			let (tx, mut rx) = mpsc::channel(0);

			// The backed candidats set should be not empty at this point.
			virtual_overseer.send(FromOverseer::Communication{
				msg: CandidateBackingMessage::RegisterBackingWatcher(
					test_state.relay_parent,
					tx,
				)
			}).await;

			let mut backed = Vec::new();
			while let Some(item) = rx.next().await {
				backed.push(item);
			}

			// `validity_votes` may be in any order so we can't do this in a single assert.
			assert_eq!(backed[0].0.candidate, candidate_a);
			assert_eq!(backed[0].0.validity_votes.len(), 2);
			assert!(backed[0].0.validity_votes.contains(
				&ValidityAttestation::Explicit(signed_b.signature().clone())
			));
			assert!(backed[0].0.validity_votes.contains(
				&ValidityAttestation::Implicit(signed_a.signature().clone())
			));
			assert_eq!(backed[0].0.validator_indices, bitvec::bitvec![Lsb0, u8; 1, 1, 0]);

			virtual_overseer.send(FromOverseer::Signal(
				OverseerSignal::StopWork(test_state.relay_parent))
			).await;
		});
	}

	// Issuing conflicting statements on the same candidate should
	// be a misbehavior.
	#[test]
	fn backing_misbehavior_works() {
		let test_state = TestState::default();
		test_harness(test_state.keystore.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;

			test_startup(&mut virtual_overseer, &test_state).await;

			let pov_block = Arc::new(PoVBlock {
				block_data: BlockData(vec![1, 2, 3]),
			});

			let pov_block_hash = pov_block.hash();
			let candidate_a = AbridgedCandidateReceipt {
				parachain_index: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_block_hash,
				..Default::default()
			};

			let candidate_a_hash = candidate_a.hash();

			let signed_a = SignedFullStatement::sign(
				Statement::Seconded(candidate_a.clone()),
				&test_state.signing_context,
				2,
				&test_state.validators[2].pair().into(),
			);

			let signed_b = SignedFullStatement::sign(
				Statement::Valid(candidate_a_hash),
				&test_state.signing_context,
				0,
				&test_state.validators[0].pair().into(),
			);

			let signed_c = SignedFullStatement::sign(
				Statement::Invalid(candidate_a_hash),
				&test_state.signing_context,
				0,
				&test_state.validators[0].pair().into(),
			);

			let statement = CandidateBackingMessage::Statement(test_state.relay_parent, signed_a.clone());

			virtual_overseer.send(FromOverseer::Communication{ msg: statement }).await;

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::PoVDistribution(
					PoVDistributionMessage::FetchPoV(relay_parent, _, tx)
				) if relay_parent == test_state.relay_parent => {
					tx.send(pov_block.clone()).unwrap();
				}
			);

			let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::Validate(
						relay_parent,
						candidate,
						head_data,
						pov,
						tx,
					)
				) if relay_parent == test_state.relay_parent && candidate == candidate_a => {
					assert_eq!(pov, pov_block);
					assert_eq!(head_data, *expected_head_data);
					tx.send(Ok((
						ValidationResult::Valid,
						test_state.global_validation_schedule,
						test_state.local_validation_data,
					))).unwrap();
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::StatementDistribution(
					StatementDistributionMessage::Share(
						relay_parent,
						signed_statement,
					)
				) if relay_parent == test_state.relay_parent => {
					signed_statement.check_signature(
						&test_state.signing_context,
						&test_state.validator_public[0],
					).unwrap();

					assert_eq!(*signed_statement.payload(), Statement::Valid(candidate_a_hash));
				}
			);

			let statement = CandidateBackingMessage::Statement(test_state.relay_parent, signed_b.clone());

			virtual_overseer.send(FromOverseer::Communication{ msg: statement }).await;

			let statement = CandidateBackingMessage::Statement(test_state.relay_parent, signed_c.clone());

			virtual_overseer.send(FromOverseer::Communication{ msg: statement }).await;

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::Provisioner(
					ProvisionerMessage::ProvisionableData(
						ProvisionableData::MisbehaviorReport(
							relay_parent,
							MisbehaviorReport::SelfContradiction(_, s1, s2),
						)
					)
				) if relay_parent == test_state.relay_parent => {
					s1.check_signature(
						&test_state.signing_context,
						&test_state.validator_public[s1.validator_index() as usize],
					).unwrap();

					s2.check_signature(
						&test_state.signing_context,
						&test_state.validator_public[s2.validator_index() as usize],
					).unwrap();
				}
			);
		});
	}

	// Test that if we are asked to second an invalid candidate we
	// can still second a valid one afterwards.
	#[test]
	fn backing_dont_second_invalid() {
		let test_state = TestState::default();
		test_harness(test_state.keystore.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;

			test_startup(&mut virtual_overseer, &test_state).await;

			let pov_block_a = Arc::new(PoVBlock {
				block_data: BlockData(vec![42, 43, 44]),
			});

			let pov_block_b = Arc::new(PoVBlock {
				block_data: BlockData(vec![45, 46, 47]),
			});

			let pov_block_hash_a = pov_block_a.hash();
			let pov_block_hash_b = pov_block_b.hash();

			let candidate_a = AbridgedCandidateReceipt {
				parachain_index: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_block_hash: pov_block_hash_a,
				..Default::default()
			};

			let candidate_a_hash = candidate_a.hash();

			let candidate_b = AbridgedCandidateReceipt {
				parachain_index: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_block_hash: pov_block_hash_b,
				..Default::default()
			};

			let second = CandidateBackingMessage::Second(
				test_state.relay_parent,
				candidate_a.clone(),
				pov_block_a.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: second }).await;

			let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::Validate(
						parent_hash,
						c,
						head_data,
						pov,
						tx,
					)
				) if parent_hash == test_state.relay_parent &&
					pov == pov_block_a && c == candidate_a => {
					assert_eq!(head_data, *expected_head_data);
					tx.send(Ok((
						ValidationResult::Invalid,
						test_state.global_validation_schedule.clone(),
						test_state.local_validation_data.clone(),
					))).unwrap();
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateSelection(
					CandidateSelectionMessage::Invalid(parent_hash, candidate)
				) if parent_hash == test_state.relay_parent && candidate == candidate_a
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::StatementDistribution(
					StatementDistributionMessage::Share(
						relay_parent,
						statement,
					)
				) if relay_parent == test_state.relay_parent => {
					assert_eq!(*statement.payload(), Statement::Invalid(candidate_a_hash));
				}
			);

			let second = CandidateBackingMessage::Second(
				test_state.relay_parent,
				candidate_b.clone(),
				pov_block_b.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: second }).await;

			let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::Validate(
						parent_hash,
						c,
						head_data,
						pov,
						tx,
					)
				) if parent_hash == test_state.relay_parent &&
					pov == pov_block_b && c == candidate_b => {
					assert_eq!(head_data, *expected_head_data);
					tx.send(Ok((
						ValidationResult::Valid,
						test_state.global_validation_schedule,
						test_state.local_validation_data,
					))).unwrap();
				}
			);

			for _ in 0..test_state.validators.len() {
				assert_matches!(
					virtual_overseer.recv().await,
					AllMessages::AvailabilityStore(
						AvailabilityStoreMessage::StoreChunk(parent_hash, _, _)
					) if parent_hash == test_state.relay_parent
				);
			}

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::StatementDistribution(
					StatementDistributionMessage::Share(
						parent_hash,
						signed_statement,
					)
				) if parent_hash == test_state.relay_parent => {
					signed_statement.check_signature(
						&test_state.signing_context,
						&test_state.validator_public[0],
					).unwrap();

					assert_eq!(*signed_statement.payload(), Statement::Seconded(candidate_b));
				}
			);

			virtual_overseer.send(FromOverseer::Signal(
				OverseerSignal::StopWork(test_state.relay_parent))
			).await;
		});
	}

	// Test that if we have already issued a statement (in this case `Invalid`) about a
	// candidate we will not be issuing a `Seconded` statement on it.
	#[test]
	fn backing_multiple_statements_work() {
		let test_state = TestState::default();
		test_harness(test_state.keystore.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;

			test_startup(&mut virtual_overseer, &test_state).await;

			let pov_block = Arc::new(PoVBlock {
				block_data: BlockData(vec![42, 43, 44]),
			});

			let pov_block_hash = pov_block.hash();

			let candidate = AbridgedCandidateReceipt {
				parachain_index: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_block_hash,
				..Default::default()
			};

			let candidate_hash = candidate.hash();

			let signed_a = SignedFullStatement::sign(
				Statement::Seconded(candidate.clone()),
				&test_state.signing_context,
				2,
				&test_state.validators[2].pair().into(),
			);

			// Send in a `Statement` with a candidate.
			let statement = CandidateBackingMessage::Statement(
				test_state.relay_parent,
				signed_a.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: statement }).await;

			// Subsystem requests PoV and requests validation.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::PoVDistribution(
					PoVDistributionMessage::FetchPoV(relay_parent, _, tx)
				) => {
					assert_eq!(relay_parent, test_state.relay_parent);
					tx.send(pov_block.clone()).unwrap();
				}
			);

			let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

			// Tell subsystem that this candidate is invalid.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::Validate(
						relay_parent,
						candidate_recvd,
						head_data,
						pov,
						tx,
					)
				) => {
					assert_eq!(relay_parent, test_state.relay_parent);
					assert_eq!(candidate_recvd, candidate);
					assert_eq!(head_data, *expected_head_data);
					assert_eq!(pov, pov_block);
					tx.send(Ok((
						ValidationResult::Invalid,
						test_state.global_validation_schedule,
						test_state.local_validation_data,
					))).unwrap();
				}
			);

			// The invalid message is shared.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::StatementDistribution(
					StatementDistributionMessage::Share(
						relay_parent,
						signed_statement,
					)
				) => {
					assert_eq!(relay_parent, test_state.relay_parent);
					signed_statement.check_signature(
						&test_state.signing_context,
						&test_state.validator_public[0],
					).unwrap();
					assert_eq!(*signed_statement.payload(), Statement::Invalid(candidate_hash));
				}
			);

			// Ask subsystem to `Second` a candidate that already has a statement issued about.
			// This should emit no actions from subsystem.
			let second = CandidateBackingMessage::Second(
				test_state.relay_parent,
				candidate.clone(),
				pov_block.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: second }).await;

			let pov_to_second = Arc::new(PoVBlock {
				block_data: BlockData(vec![3, 2, 1]),
			});

			let pov_block_hash = pov_to_second.hash();

			let candidate_to_second = AbridgedCandidateReceipt {
				parachain_index: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_block_hash,
				..Default::default()
			};

			let second = CandidateBackingMessage::Second(
				test_state.relay_parent,
				candidate_to_second.clone(),
				pov_to_second.clone(),
			);

			// In order to trigger _some_ actions from subsystem ask it to second another
			// candidate. The only reason to do so is to make sure that no actions were
			// triggered on the prev step.
			virtual_overseer.send(FromOverseer::Communication{ msg: second }).await;

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::Validate(
						relay_parent,
						_,
						_,
						pov,
						_,
					)
				) => {
					assert_eq!(relay_parent, test_state.relay_parent);
					assert_eq!(pov, pov_to_second);
				}
			);
		});
	}
}
