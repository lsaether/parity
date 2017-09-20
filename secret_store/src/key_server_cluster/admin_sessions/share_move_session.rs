// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::sync::Arc;
use std::collections::{BTreeMap, BTreeSet};
use parking_lot::Mutex;
use ethkey::{Secret, Signature};
use key_server_cluster::{Error, NodeId, SessionMeta, DocumentKeyShare, KeyStorage};
use key_server_cluster::cluster_sessions::ClusterSession;
use key_server_cluster::message::{ShareMoveMessage, InitializeShareMoveSession, ConfirmShareMoveInitialization,
	ShareMoveRequest, ShareMove, ShareMoveConfirm, ShareMoveError};

/// Share move session API.
pub trait Session: Send + Sync + 'static {
}

/// Share move session transport.
pub trait SessionTransport {
	/// Send message to given node.
	fn send(&self, node: &NodeId, message: ShareMoveMessage) -> Result<(), Error>;
}

/// Share move session.
pub struct SessionImpl<T: SessionTransport> {
	/// Session core.
	core: SessionCore<T>,
	/// Session data.
	data: Mutex<SessionData>,
}

/// Immutable session data.
struct SessionCore<T: SessionTransport> {
	/// Session metadata.
	pub meta: SessionMeta,
	/// Share add session id.
	pub sub_session: Secret,
	/// Session-level nonce.
	pub nonce: u64,
	/// Original key share (for old nodes only). TODO: is it possible to read from key_storage
	pub key_share: Option<DocumentKeyShare>,
	/// Session transport to communicate to other cluster nodes.
	pub transport: T,
	/// Key storage.
	pub key_storage: Arc<KeyStorage>,
}

/// Mutable session data.
struct SessionData {
	/// Session state.
	pub state: SessionState,
	/// Initialization confirmations to receive (all nodes set).
	pub init_confirmations_to_receive: BTreeSet<NodeId>,
	/// Move confirmations to receive.
	pub move_confirmations_to_receive: BTreeSet<NodeId>,
	/// Shares to move.
	pub shares_to_move: BTreeMap<NodeId, NodeId>,
	/// Received key share (filled on destination nodes only).
	pub received_key_share: Option<DocumentKeyShare>,
}

/// SessionImpl creation parameters
pub struct SessionParams<T: SessionTransport> {
	/// Session meta.
	pub meta: SessionMeta,
	/// Sub session identifier.
	pub sub_session: Secret,
	/// Session nonce.
	pub nonce: u64,
	/// Original key share (for master node only).
	pub key_share: Option<DocumentKeyShare>,
	/// Session transport to communicate to other cluster nodes.
	pub transport: T,
	/// Key storage.
	pub key_storage: Arc<KeyStorage>,
}

/// Share move session state.
#[derive(Debug, PartialEq)]
enum SessionState {
	/// Waiting for initialization.
	WaitingForInitialization,
	/// Waiting for initialization confirmation.
	WaitingForInitializationConfirm,
	/// Waiting for move confirmation.
	WaitingForMoveConfirmation,
	/// Session is finished.
	Finished,
}

impl<T> SessionImpl<T> where T: SessionTransport {
	/// Create new nested share addition session. Consensus is formed outside.
	pub fn new_nested(params: SessionParams<T>) -> Result<Self, Error> {
		Ok(SessionImpl {
			core: SessionCore {
				meta: params.meta,
				sub_session: params.sub_session,
				nonce: params.nonce,
				key_share: params.key_share,
				transport: params.transport,
				key_storage: params.key_storage,
			},
			data: Mutex::new(SessionData {
				state: SessionState::WaitingForInitialization,
				init_confirmations_to_receive: BTreeSet::new(),
				move_confirmations_to_receive: BTreeSet::new(),
				shares_to_move: BTreeMap::new(),
				received_key_share: None,
			}),
		})
	}

	/// Initialize share add session on master node.
	pub fn initialize(&self, shares_to_move: BTreeMap<NodeId, NodeId>) -> Result<(), Error> {
		debug_assert_eq!(self.core.meta.self_node_id, self.core.meta.master_node_id);

		let old_key_share = self.core.key_share.as_ref()
			.expect("initialize is called on master node; master node owns its own key share; qed");
		check_shares_to_move(&self.core.meta.self_node_id, &shares_to_move, Some(&old_key_share.id_numbers))?;

		let mut data = self.data.lock();

		// check state
		if data.state != SessionState::WaitingForInitialization {
			return Err(Error::InvalidStateForRequest);
		}

		// update state
		data.state = SessionState::WaitingForInitializationConfirm;
		data.shares_to_move.extend(shares_to_move.clone());
		let move_confirmations_to_receive: Vec<_> = data.shares_to_move.values().cloned().collect();
		data.move_confirmations_to_receive.extend(move_confirmations_to_receive);
		data.init_confirmations_to_receive.extend(old_key_share.id_numbers.keys().cloned()
			.chain(shares_to_move.values().cloned()));
		data.init_confirmations_to_receive.remove(&self.core.meta.self_node_id);

		// send initialization request to every node
		for node in &data.init_confirmations_to_receive {
			self.core.transport.send(node, ShareMoveMessage::InitializeShareMoveSession(InitializeShareMoveSession {
				session: self.core.meta.id.clone().into(),
				sub_session: self.core.sub_session.clone().into(),
				session_nonce: self.core.nonce,
				shares_to_move: shares_to_move.iter().map(|(k, v)| (k.clone().into(), v.clone().into())).collect(),
			}))?;
		}

		Ok(())
	}

	/// Process single message.
	pub fn process_message(&self, sender: &NodeId, message: &ShareMoveMessage) -> Result<(), Error> {
		if self.core.nonce != message.session_nonce() {
			return Err(Error::ReplayProtection);
		}

		match message {
			&ShareMoveMessage::InitializeShareMoveSession(ref message) =>
				self.on_initialize_session(sender, message),
			&ShareMoveMessage::ConfirmShareMoveInitialization(ref message) =>
				self.on_confirm_initialization(sender, message),
			&ShareMoveMessage::ShareMoveRequest(ref message) =>
				self.on_share_move_request(sender, message),
			&ShareMoveMessage::ShareMove(ref message) =>
				self.on_share_move(sender, message),
			&ShareMoveMessage::ShareMoveConfirm(ref message) =>
				self.on_share_move_confirmation(sender, message),
			&ShareMoveMessage::ShareMoveError(ref message) =>
				self.on_session_error(sender, message),
		}
	}

	/// When initialization request is received.
	pub fn on_initialize_session(&self, sender: &NodeId, message: &InitializeShareMoveSession) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.sub_session == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		// awaiting this message from master node only
		if sender != &self.core.meta.master_node_id {
			return Err(Error::InvalidMessage);
		}

		// check shares_to_move
		let shares_to_move = message.shares_to_move.clone().into_iter().map(|(k, v)| (k.into(), v.into())).collect();
		check_shares_to_move(&self.core.meta.self_node_id, &shares_to_move, self.core.key_share.as_ref().map(|ks| &ks.id_numbers))?;

		// this node is either old on both (this && master) nodes, or new on both nodes
		let key_share = if let Some(share_destination) = shares_to_move.get(&self.core.meta.self_node_id) {
			Some(self.core.key_share.as_ref()
				.ok_or(Error::InvalidMessage)?)
		} else {
			if shares_to_move.values().any(|n| n == &self.core.meta.self_node_id) {
				if self.core.key_share.is_some() {
					return Err(Error::InvalidMessage);
				}
			}

			None
		};

		// update state
		let mut data = self.data.lock();
		if data.state != SessionState::WaitingForInitialization {
			return Err(Error::InvalidStateForRequest);
		}
		data.state = SessionState::WaitingForMoveConfirmation;
		data.shares_to_move.extend(shares_to_move);
		let move_confirmations_to_receive: Vec<_> = data.shares_to_move.values().cloned().collect();
		data.move_confirmations_to_receive.extend(move_confirmations_to_receive);

		// confirm initialization
		self.core.transport.send(sender, ShareMoveMessage::ConfirmShareMoveInitialization(ConfirmShareMoveInitialization {
			session: self.core.meta.id.clone().into(),
			sub_session: self.core.sub_session.clone().into(),
			session_nonce: self.core.nonce,
		}))?;

		Ok(())
	}

	/// When session initialization confirmation message is received.
	pub fn on_confirm_initialization(&self, sender: &NodeId, message: &ConfirmShareMoveInitialization) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.sub_session == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		// awaiting this message on master node only
		if self.core.meta.self_node_id != self.core.meta.master_node_id {
			return Err(Error::InvalidMessage);
		}

		// check state
		let mut data = self.data.lock();
		if data.state != SessionState::WaitingForInitializationConfirm {
			return Err(Error::InvalidStateForRequest);
		}
		// do not expect double confirmations
		if !data.init_confirmations_to_receive.remove(sender) {
			return Err(Error::InvalidMessage);
		}
		// if not all init confirmations are received => return
		if !data.init_confirmations_to_receive.is_empty() {
			return Ok(());
		}

		// update state
		data.state = SessionState::WaitingForMoveConfirmation;
		// send share move requests
		for share_source in data.shares_to_move.keys().filter(|n| **n != self.core.meta.self_node_id) {
			self.core.transport.send(share_source, ShareMoveMessage::ShareMoveRequest(ShareMoveRequest {
				session: self.core.meta.id.clone().into(),
				sub_session: self.core.sub_session.clone().into(),
				session_nonce: self.core.nonce,
			}))?;
		}
		// move share if required
		if let Some(share_destination) = data.shares_to_move.get(&self.core.meta.self_node_id) {
			Self::move_share(&self.core, share_destination)?;
		}

		Ok(())
	}

	/// When share move request is received.
	pub fn on_share_move_request(&self, sender: &NodeId, message: &ShareMoveRequest) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.sub_session == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		// awaiting this message from master node only
		if sender != &self.core.meta.master_node_id {
			return Err(Error::InvalidMessage);
		}

		// check state
		let mut data = self.data.lock();
		if data.state != SessionState::WaitingForMoveConfirmation {
			return Err(Error::InvalidStateForRequest);
		}
		// move share
		if let Some(share_destination) = data.shares_to_move.get(&self.core.meta.self_node_id) {
			Self::move_share(&self.core, share_destination)
		} else {
			Err(Error::InvalidMessage)
		}
	}

	/// When moving share is received.
	pub fn on_share_move(&self, sender: &NodeId, message: &ShareMove) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.sub_session == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		// check state
		let mut data = self.data.lock();
		if data.state != SessionState::WaitingForMoveConfirmation {
			return Err(Error::InvalidStateForRequest);
		}
		// check that we are expecting this share
		if data.shares_to_move.get(sender) != Some(&self.core.meta.self_node_id) {
			return Err(Error::InvalidMessage);
		}

		// update state
		data.move_confirmations_to_receive.remove(&self.core.meta.self_node_id);
		data.received_key_share = Some(DocumentKeyShare {
			author: message.author.clone().into(),
			threshold: message.threshold,
			id_numbers: message.id_numbers.iter().map(|(k, v)| (k.clone().into(), v.clone().into())).collect(),
			polynom1: message.polynom1.iter().cloned().map(Into::into).collect(),
			secret_share: message.secret_share.clone().into(),
			common_point: message.common_point.clone().map(Into::into),
			encrypted_point: message.encrypted_point.clone().map(Into::into),
		});

		// send confirmation to all other nodes
		let all_nodes_set: BTreeSet<_> = data.shares_to_move.values().cloned()
			.chain(message.id_numbers.keys().cloned().map(Into::into))
			.collect();
		for node in all_nodes_set.into_iter().filter(|n| n != &self.core.meta.self_node_id) {
			self.core.transport.send(&node, ShareMoveMessage::ShareMoveConfirm(ShareMoveConfirm {
				session: self.core.meta.id.clone().into(),
				sub_session: self.core.sub_session.clone().into(),
				session_nonce: self.core.nonce,
			}))?;
		}

		// complete session if this was last share
		if data.move_confirmations_to_receive.is_empty() {
			Self::complete_session(&self.core, &mut *data)?;
		}

		Ok(())
	}

	/// When share is received from destination node.
	pub fn on_share_move_confirmation(&self, sender: &NodeId, message: &ShareMoveConfirm) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.sub_session == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		// check state
		let mut data = self.data.lock();
		if data.state != SessionState::WaitingForMoveConfirmation {
			return Err(Error::InvalidStateForRequest);
		}
		// find share source
		if !data.move_confirmations_to_receive.remove(sender) {
			return Err(Error::InvalidMessage);
		}
		if data.move_confirmations_to_receive.is_empty() {
			Self::complete_session(&self.core, &mut *data)?;
		}

		Ok(())
	}

	/// When error has occured on another node.
	pub fn on_session_error(&self, sender: &NodeId, message: &ShareMoveError) -> Result<(), Error> {
		let mut data = self.data.lock();

		warn!("{}: share move session failed with error: {} from {}", self.core.meta.self_node_id, message.error, sender);

		data.state = SessionState::Finished;

		Ok(())
	}

	/// Send share move message.
	fn move_share(core: &SessionCore<T>, share_destination: &NodeId) -> Result<(), Error> {
		let key_share = core.key_share.as_ref()
			.expect("move_share is called on nodes from shares_to_move.keys(); all 'key' nodes have shares; qed");
		core.transport.send(share_destination, ShareMoveMessage::ShareMove(ShareMove {
			session: core.meta.id.clone().into(),
			sub_session: core.sub_session.clone().into(),
			session_nonce: core.nonce,
			author: key_share.author.clone().into(),
			threshold: key_share.threshold,
			id_numbers: key_share.id_numbers.iter().map(|(k, v)| (k.clone().into(), v.clone().into())).collect(),
			polynom1: key_share.polynom1.iter().cloned().map(Into::into).collect(),
			secret_share: key_share.secret_share.clone().into(),
			common_point: key_share.common_point.clone().map(Into::into),
			encrypted_point: key_share.encrypted_point.clone().map(Into::into),
		}))
	}

	/// Complete session on this node.
	fn complete_session(core: &SessionCore<T>, data: &mut SessionData) -> Result<(), Error> {
		// if we are source node => remove share from storage
		if data.shares_to_move.contains_key(&core.meta.self_node_id) {
			return core.key_storage.remove(&core.meta.id)
				.map_err(|e| Error::KeyStorage(e.into()));
		}

		// else we need to update key_share.id_numbers.keys()
		let is_old_node = data.received_key_share.is_none();
		let mut key_share = data.received_key_share.take()
			.unwrap_or_else(|| core.key_share.as_ref()
				.expect("on target nodes received_key_share is non-empty; on old nodes key_share is not empty; qed")
				.clone());
		for (source_node, target_node) in &data.shares_to_move {
			let id_number = key_share.id_numbers.remove(source_node)
				.expect("source_node is old node; there's entry in id_numbers for each old node; qed");
			key_share.id_numbers.insert(target_node.clone(), id_number);
		}

		// ... and update key share in storage
		if is_old_node {
			core.key_storage.update(core.meta.id.clone(), key_share)
		} else {
			core.key_storage.insert(core.meta.id.clone(), key_share)
		}.map_err(|e| Error::KeyStorage(e.into()))
	}
}

impl<T> ClusterSession for SessionImpl<T> where T: SessionTransport {
	fn is_finished(&self) -> bool {
		self.data.lock().state == SessionState::Finished
	}

	fn on_session_timeout(&self) {
		unimplemented!()
	}

	fn on_node_timeout(&self, _node_id: &NodeId) {
		unimplemented!()
	}
}

fn check_shares_to_move(self_node_id: &NodeId, shares_to_move: &BTreeMap<NodeId, NodeId>, id_numbers: Option<&BTreeMap<NodeId, Secret>>) -> Result<(), Error> {
	// shares to move must not be empty
	if shares_to_move.is_empty() {
		return Err(Error::InvalidMessage);
	}

	if let Some(id_numbers) = id_numbers {
		// all keys in shares_to_move must be old nodes of the session
		if shares_to_move.keys().any(|n| !id_numbers.contains_key(n)) {
			return Err(Error::InvalidNodesConfiguration);
		}
		// all values in shares_to_move must be new nodes for the session
		if shares_to_move.values().any(|n| id_numbers.contains_key(n)) {
			return Err(Error::InvalidNodesConfiguration);
		}
	} else {
		// this node must NOT in keys of shares_to_move
		if shares_to_move.contains_key(self_node_id) {
			return Err(Error::InvalidMessage);
		}
		// this node must be in values of share_to_move
		if !shares_to_move.values().any(|n| n == self_node_id) {
			return Err(Error::InvalidMessage);
		}
	}

	// all values of the shares_to_move must be distinct
	if shares_to_move.values().collect::<BTreeSet<_>>().len() != shares_to_move.len() {
		return Err(Error::InvalidNodesConfiguration);
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;
	use std::collections::{VecDeque, BTreeMap, BTreeSet};
	use ethkey::{Random, Generator, Public, KeyPair, sign};
	use key_server_cluster::{NodeId, SessionId, Error, KeyStorage, DummyKeyStorage, SessionMeta};
	use key_server_cluster::cluster::Cluster;
	use key_server_cluster::cluster::tests::DummyCluster;
	use key_server_cluster::generation_session::tests::MessageLoop as GenerationMessageLoop;
	use key_server_cluster::math;
	use key_server_cluster::message::{Message, ServersSetChangeMessage, ShareAddMessage};
	use key_server_cluster::servers_set_change_session::tests::generate_key;
	use key_server_cluster::share_change_session::ShareChangeTransport;
	use super::{SessionImpl, SessionParams, SessionTransport};

	struct Node {
		pub cluster: Arc<DummyCluster>,
		pub key_storage: Arc<DummyKeyStorage>,
		pub session: SessionImpl<ShareChangeTransport>,
	}

	struct MessageLoop {
		pub session_id: SessionId,
		pub nodes: BTreeMap<NodeId, Node>,
		pub queue: VecDeque<(NodeId, NodeId, Message)>,
	}

	impl MessageLoop {
		pub fn new(gml: GenerationMessageLoop, threshold: usize, num_nodes_to_move: usize) -> Self {
			let new_nodes_ids: BTreeSet<_> = (0..num_nodes_to_move).map(|_| Random.generate().unwrap().public().clone()).collect();
			let shares_to_move: BTreeMap<_, _> = gml.nodes.keys().cloned().zip(new_nodes_ids.iter().cloned()).take(num_nodes_to_move).collect();

			let key_id = gml.session_id.clone();
			let session_id = SessionId::default();
			let sub_session = Random.generate().unwrap().secret().clone();
			let mut nodes = BTreeMap::new();
			let master_node_id = gml.nodes.keys().cloned().nth(0).unwrap();
			let meta = SessionMeta {
				self_node_id: master_node_id.clone(),
				master_node_id: master_node_id.clone(),
				id: session_id.clone(),
				threshold: threshold,
			};
 
			for (n, nd) in &gml.nodes {
				let cluster = nd.cluster.clone();
				let key_storage = nd.key_storage.clone();
				let mut meta = meta.clone();
				meta.self_node_id = n.clone();
				let session = SessionImpl::new_nested(SessionParams {
					meta: meta,
					sub_session: sub_session.clone(),
					transport: ShareChangeTransport::new(session_id.clone(), 1, cluster.clone()),
					key_storage: nd.key_storage.clone(),
					nonce: 1,
					key_share: Some(key_storage.get(&key_id).unwrap()),
				}).unwrap();
				nodes.insert(n.clone(), Node {
					cluster: cluster,
					key_storage: key_storage,
					session: session,
				});
			}
			for new_node_id in new_nodes_ids {
				let cluster = Arc::new(DummyCluster::new(new_node_id.clone()));
				let key_storage = Arc::new(DummyKeyStorage::default());
				let mut meta = meta.clone();
				meta.self_node_id = new_node_id;
				let session = SessionImpl::new_nested(SessionParams {
					meta: meta,
					sub_session: sub_session.clone(),
					transport: ShareChangeTransport::new(session_id.clone(), 1, cluster.clone()),
					key_storage: key_storage.clone(),
					nonce: 1,
					key_share: None,
				}).unwrap();
				nodes.insert(new_node_id, Node {
					cluster: cluster,
					key_storage: key_storage,
					session: session,
				});
			}

			MessageLoop {
				session_id: session_id,
				nodes: nodes,
				queue: Default::default(),
			}
		}

		pub fn run(&mut self) {
			while let Some((from, to, message)) = self.take_message() {
				self.process_message((from, to, message)).unwrap();
			}
		}

		pub fn take_message(&mut self) -> Option<(NodeId, NodeId, Message)> {
			self.nodes.values()
				.filter_map(|n| n.cluster.take_message().map(|m| (n.session.core.meta.self_node_id.clone(), m.0, m.1)))
				.nth(0)
				.or_else(|| self.queue.pop_front())
		}

		pub fn process_message(&mut self, msg: (NodeId, NodeId, Message)) -> Result<(), Error> {
			match {
				match msg.2 {
					Message::ServersSetChange(ServersSetChangeMessage::ServersSetChangeShareMoveMessage(ref message)) =>
						self.nodes[&msg.1].session.process_message(&msg.0, &message.message),
					_ => unreachable!("only servers set change messages are expected"),
				}
			} {
				Ok(_) => Ok(()),
				Err(Error::TooEarlyForRequest) => {
					self.queue.push_back(msg);
					Ok(())
				},
				Err(err) => Err(err),
			}
		}
	}

	#[test]
	fn node_moved_using_share_move() {
		// initial 2-of-3 session
		let (t, n) = (1, 3);
		let gml = generate_key(t, n);
		let gml_nodes: BTreeSet<_> = gml.nodes.keys().cloned().collect();
		let key_id = gml.session_id.clone();
		let master = gml.nodes.keys().cloned().nth(0).unwrap();
		let source_node = gml.nodes.keys().cloned().nth(1).unwrap();
		let joint_secret = math::compute_joint_secret(gml.nodes.values()
			.map(|nd| nd.key_storage.get(&key_id).unwrap().polynom1[0].clone())
			.collect::<Vec<_>>()
			.iter()).unwrap();
		let joint_key_pair = KeyPair::from_secret(joint_secret.clone()).unwrap();

		// add 1 node && move share
		let mut ml = MessageLoop::new(gml, t, 1);
		let new_nodes_set: BTreeSet<_> = ml.nodes.keys().cloned().filter(|n| !gml_nodes.contains(n)).collect();
		let target_node = new_nodes_set.into_iter().nth(0).unwrap();
		let shares_to_move = vec![(source_node.clone(), target_node)].into_iter().collect();
		ml.nodes[&master].session.initialize(shares_to_move);
		ml.run();

		// try to recover secret for every possible combination of nodes && check that secret is the same
		let document_secret_plain = math::generate_random_point().unwrap();
		for n1 in 0..n+1 {
			for n2 in n1+1..n+1 {
				let node1 = ml.nodes.keys().nth(n1).unwrap();
				let node2 = ml.nodes.keys().nth(n2).unwrap();
				if node1 == &source_node {
					assert!(ml.nodes.values().nth(n1).unwrap().key_storage.get(&key_id).is_err());
					continue;
				}
				if node2 == &source_node {
					assert!(ml.nodes.values().nth(n2).unwrap().key_storage.get(&key_id).is_err());
					continue;
				}

				let share1 = ml.nodes.values().nth(n1).unwrap().key_storage.get(&key_id).unwrap();
				let share2 = ml.nodes.values().nth(n2).unwrap().key_storage.get(&key_id).unwrap();
				let id_number1 = share1.id_numbers[ml.nodes.keys().nth(n1).unwrap()].clone();
				let id_number2 = share1.id_numbers[ml.nodes.keys().nth(n2).unwrap()].clone();

				// now encrypt and decrypt data
				let (document_secret_decrypted, document_secret_decrypted_test) =
					math::tests::do_encryption_and_decryption(t,
						joint_key_pair.public(),
						&[id_number1, id_number2],
						&[share1.secret_share, share2.secret_share],
						Some(&joint_secret),
						document_secret_plain.clone());

				assert_eq!(document_secret_plain, document_secret_decrypted_test);
				assert_eq!(document_secret_plain, document_secret_decrypted);
			}
		}
	}
}