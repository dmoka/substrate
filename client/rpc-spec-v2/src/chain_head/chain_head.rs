// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! API implementation for `chainHead`.

use crate::{
	chain_head::{
		api::ChainHeadApiServer,
		chain_head_follow::ChainHeadFollower,
		error::Error as ChainHeadRpcError,
		event::{ChainHeadEvent, ChainHeadResult, ErrorEvent, FollowEvent, NetworkConfig},
		subscription::{SubscriptionManagement, SubscriptionManagementError},
	},
	SubscriptionTaskExecutor,
};
use codec::Encode;
use jsonrpsee::{
	core::{async_trait, RpcResult},
	types::{ErrorObjectOwned, SubscriptionId},
	PendingSubscriptionAcceptError, PendingSubscriptionSink, SubscriptionSink,
};
use log::debug;
use sc_client_api::{
	Backend, BlockBackend, BlockchainEvents, CallExecutor, ChildInfo, ExecutorProvider, StorageKey,
	StorageProvider,
};
use sc_rpc::utils::SubscriptionResponse;
use sp_api::CallApiAt;
use sp_blockchain::{Error as BlockChainError, HeaderBackend, HeaderMetadata};
use sp_core::{hexdisplay::HexDisplay, storage::well_known_keys, traits::CallContext, Bytes};
use sp_runtime::traits::Block as BlockT;
use std::{marker::PhantomData, sync::Arc, time::Duration};

pub(crate) const LOG_TARGET: &str = "rpc-spec-v2";

/// An API for chain head RPC calls.
pub struct ChainHead<BE: Backend<Block>, Block: BlockT, Client> {
	/// Substrate client.
	client: Arc<Client>,
	/// Backend of the chain.
	backend: Arc<BE>,
	/// Executor to spawn subscriptions.
	executor: SubscriptionTaskExecutor,
	/// Keep track of the pinned blocks for each subscription.
	subscriptions: Arc<SubscriptionManagement<Block, BE>>,
	/// The hexadecimal encoded hash of the genesis block.
	genesis_hash: String,
	/// Phantom member to pin the block type.
	_phantom: PhantomData<Block>,
}

impl<BE: Backend<Block>, Block: BlockT, Client> ChainHead<BE, Block, Client> {
	/// Create a new [`ChainHead`].
	pub fn new<GenesisHash: AsRef<[u8]>>(
		client: Arc<Client>,
		backend: Arc<BE>,
		executor: SubscriptionTaskExecutor,
		genesis_hash: GenesisHash,
		max_pinned_blocks: usize,
		max_pinned_duration: Duration,
	) -> Self {
		let genesis_hash = format!("0x{:?}", HexDisplay::from(&genesis_hash.as_ref()));

		Self {
			client,
			backend: backend.clone(),
			executor,
			subscriptions: Arc::new(SubscriptionManagement::new(
				max_pinned_blocks,
				max_pinned_duration,
				backend,
			)),
			genesis_hash,
			_phantom: PhantomData,
		}
	}

	/// Accept the subscription and return the subscription ID on success.
	async fn accept_subscription(
		&self,
		pending: PendingSubscriptionSink,
	) -> Result<(SubscriptionSink, String), PendingSubscriptionAcceptError> {
		// The subscription must be accepted before it can provide a valid subscription ID.
		let sink = pending.accept().await?;

		// Get the string representation for the subscription.
		let sub_id = match sink.subscription_id() {
			SubscriptionId::Num(num) => num.to_string(),
			SubscriptionId::Str(id) => id.into_owned().into(),
		};

		Ok((sink, sub_id))
	}
}

struct MaybePendingSubscription(Option<PendingSubscriptionSink>);

impl MaybePendingSubscription {
	pub fn new(p: PendingSubscriptionSink) -> Self {
		Self(Some(p))
	}

	pub async fn accept(&mut self) -> Result<SubscriptionSink, PendingSubscriptionAcceptError> {
		if let Some(p) = self.0.take() {
			p.accept().await
		} else {
			Err(PendingSubscriptionAcceptError)
		}
	}

	pub async fn reject(&mut self, err: impl Into<ErrorObjectOwned>) {
		if let Some(p) = self.0.take() {
			p.reject(err).await;
		}
	}
}

/// Parse hex-encoded string parameter as raw bytes.
///
/// If the parsing fails, the subscription is rejected.
async fn parse_hex_param(
	pending: &mut MaybePendingSubscription,
	param: String,
) -> Result<Vec<u8>, PendingSubscriptionAcceptError> {
	// Methods can accept empty parameters.
	if param.is_empty() {
		return Ok(Vec::new())
	}

	match array_bytes::hex2bytes(&param) {
		Ok(bytes) => Ok(bytes),
		Err(_) => {
			pending.reject(ChainHeadRpcError::InvalidParam(param)).await;
			Err(PendingSubscriptionAcceptError)
		},
	}
}

#[async_trait]
impl<BE, Block, Client> ChainHeadApiServer<Block::Hash> for ChainHead<BE, Block, Client>
where
	Block: BlockT + 'static,
	Block::Header: Unpin,
	BE: Backend<Block> + 'static,
	Client: BlockBackend<Block>
		+ ExecutorProvider<Block>
		+ HeaderBackend<Block>
		+ HeaderMetadata<Block, Error = BlockChainError>
		+ BlockchainEvents<Block>
		+ CallApiAt<Block>
		+ StorageProvider<Block, BE>
		+ 'static,
{
	async fn chain_head_unstable_follow(
		&self,
		pending: PendingSubscriptionSink,
		runtime_updates: bool,
	) -> SubscriptionResponse<FollowEvent<Block::Hash>> {
		let Ok((sink, sub_id)) = self.accept_subscription(pending).await else {
			return SubscriptionResponse::Closed;
		};
		// Keep track of the subscription.
		let Some(rx_stop) = self.subscriptions.insert_subscription(sub_id.clone(), runtime_updates) else {
			// Inserting the subscription can only fail if the JsonRPSee
			// generated a duplicate subscription ID.
			debug!(target: LOG_TARGET, "[follow][id={:?}] Subscription already accepted", sub_id);
			return SubscriptionResponse::Event(FollowEvent::Stop);
		};
		debug!(target: LOG_TARGET, "[follow][id={:?}] Subscription accepted", sub_id);

		let subscriptions = self.subscriptions.clone();
		let backend = self.backend.clone();
		let client = self.client.clone();
		let sub_id2 = sub_id.clone();

		let res = sc_rpc::utils::spawn_subscription_task(&self.executor, async move {
			let mut chain_head_follow =
				ChainHeadFollower::new(client, backend, subscriptions, runtime_updates, sub_id2);

			chain_head_follow.generate_events(sink, rx_stop).await
		})
		.await;

		self.subscriptions.remove_subscription(&sub_id);
		debug!(target: LOG_TARGET, "[follow][id={:?}] Subscription removed", sub_id);

		res
	}

	async fn chain_head_unstable_body(
		&self,
		pending: PendingSubscriptionSink,
		follow_subscription: String,
		hash: Block::Hash,
		_network_config: Option<NetworkConfig>,
	) -> SubscriptionResponse<ChainHeadEvent<String>> {
		let client = self.client.clone();
		let subscriptions = self.subscriptions.clone();

		let block_guard = match subscriptions.lock_block(&follow_subscription, hash) {
			Ok(block) => {
				if pending.accept().await.is_err() {
					return SubscriptionResponse::Closed
				}
				block
			},
			Err(SubscriptionManagementError::SubscriptionAbsent) => {
				// Invalid invalid subscription ID.
				let _sink = pending.accept().await;
				return SubscriptionResponse::Event(ChainHeadEvent::Disjoint)
			},
			Err(SubscriptionManagementError::BlockHashAbsent) => {
				// Block is not part of the subscription.
				pending.reject(ChainHeadRpcError::InvalidBlock).await;
				return SubscriptionResponse::Closed
			},
			Err(error) => {
				let _sink = pending.accept().await;
				return SubscriptionResponse::Event(ChainHeadEvent::Error(ErrorEvent {
					error: error.to_string(),
				}))
			},
		};

		let fut = async move {
			let _block_guard = block_guard;
			let event = match client.block(hash) {
				Ok(Some(signed_block)) => {
					let extrinsics = signed_block.block.extrinsics();
					let result = format!("0x{:?}", HexDisplay::from(&extrinsics.encode()));
					ChainHeadEvent::Done(ChainHeadResult { result })
				},
				Ok(None) => {
					// The block's body was pruned. This subscription ID has become invalid.
					debug!(
						target: LOG_TARGET,
						"[body][id={:?}] Stopping subscription because hash={:?} was pruned",
						&follow_subscription,
						hash
					);
					subscriptions.remove_subscription(&follow_subscription);
					ChainHeadEvent::<String>::Disjoint
				},
				Err(error) => ChainHeadEvent::Error(ErrorEvent { error: error.to_string() }),
			};
			SubscriptionResponse::Event(event)
		};

		sc_rpc::utils::spawn_subscription_task(&self.executor, fut).await
	}

	fn chain_head_unstable_header(
		&self,
		follow_subscription: String,
		hash: Block::Hash,
	) -> RpcResult<Option<String>> {
		let _block_guard = match self.subscriptions.lock_block(&follow_subscription, hash) {
			Ok(block) => block,
			Err(SubscriptionManagementError::SubscriptionAbsent) => {
				// Invalid invalid subscription ID.
				return Ok(None)
			},
			Err(SubscriptionManagementError::BlockHashAbsent) => {
				// Block is not part of the subscription.
				return Err(ChainHeadRpcError::InvalidBlock.into())
			},
			Err(_) => return Err(ChainHeadRpcError::InvalidBlock.into()),
		};

		self.client
			.header(hash)
			.map(|opt_header| opt_header.map(|h| format!("0x{:?}", HexDisplay::from(&h.encode()))))
			.map_err(ChainHeadRpcError::FetchBlockHeader)
			.map_err(Into::into)
	}

	fn chain_head_unstable_genesis_hash(&self) -> RpcResult<String> {
		Ok(self.genesis_hash.clone())
	}

	async fn chain_head_unstable_storage(
		&self,
		pending: PendingSubscriptionSink,
		follow_subscription: String,
		hash: Block::Hash,
		key: String,
		child_key: Option<String>,
		_network_config: Option<NetworkConfig>,
	) -> SubscriptionResponse<ChainHeadEvent<Option<String>>> {
		let mut pending = MaybePendingSubscription::new(pending);
		let Ok(key) = parse_hex_param(&mut pending, key).await.map(|k| StorageKey(k)) else {
			return SubscriptionResponse::Closed;
		};

		let child_key = match child_key {
			Some(k) => {
				let Ok(key) = parse_hex_param(&mut pending, k).await else {
					return SubscriptionResponse::Closed;
				};
				Some(ChildInfo::new_default_from_vec(key))
			},
			None => None,
		};

		let client = self.client.clone();
		let subscriptions = self.subscriptions.clone();

		let block_guard = match subscriptions.lock_block(&follow_subscription, hash) {
			Ok(block) => {
				if pending.accept().await.is_err() {
					return SubscriptionResponse::Closed
				}
				block
			},
			Err(SubscriptionManagementError::SubscriptionAbsent) => {
				let _sink = pending.accept().await;
				// Invalid invalid subscription ID.
				return SubscriptionResponse::Event(ChainHeadEvent::Disjoint)
			},
			Err(SubscriptionManagementError::BlockHashAbsent) => {
				// Block is not part of the subscription.
				pending.reject(ChainHeadRpcError::InvalidBlock).await;
				return SubscriptionResponse::Closed
			},
			Err(error) => {
				let _sink = pending.accept().await;
				return SubscriptionResponse::Event(ChainHeadEvent::Error(ErrorEvent {
					error: error.to_string(),
				}))
			},
		};

		let fut = async move {
			let _block_guard = block_guard;
			// The child key is provided, use the key to query the child trie.
			if let Some(child_key) = child_key {
				// The child key must not be prefixed with ":child_storage:" nor
				// ":child_storage:default:".
				if well_known_keys::is_default_child_storage_key(child_key.storage_key()) ||
					well_known_keys::is_child_storage_key(child_key.storage_key())
				{
					return SubscriptionResponse::Event(ChainHeadEvent::Done(ChainHeadResult {
						result: None::<String>,
					}))
				}

				let res = client
					.child_storage(hash, &child_key, &key)
					.map(|result| {
						let result =
							result.map(|storage| format!("0x{:?}", HexDisplay::from(&storage.0)));
						ChainHeadEvent::Done(ChainHeadResult { result })
					})
					.unwrap_or_else(|error| {
						ChainHeadEvent::Error(ErrorEvent { error: error.to_string() })
					});
				return SubscriptionResponse::Event(res)
			}

			// The main key must not be prefixed with b":child_storage:" nor
			// b":child_storage:default:".
			if well_known_keys::is_default_child_storage_key(&key.0) ||
				well_known_keys::is_child_storage_key(&key.0)
			{
				return SubscriptionResponse::Event(ChainHeadEvent::Done(ChainHeadResult {
					result: None::<String>,
				}))
			}

			// Main root trie storage query.
			let res = client
				.storage(hash, &key)
				.map(|result| {
					let result =
						result.map(|storage| format!("0x{:?}", HexDisplay::from(&storage.0)));
					ChainHeadEvent::Done(ChainHeadResult { result })
				})
				.unwrap_or_else(|error| {
					ChainHeadEvent::Error(ErrorEvent { error: error.to_string() })
				});

			SubscriptionResponse::Event(res)
		};

		sc_rpc::utils::spawn_subscription_task(&self.executor, fut).await
	}

	async fn chain_head_unstable_call(
		&self,
		pending: PendingSubscriptionSink,
		follow_subscription: String,
		hash: Block::Hash,
		function: String,
		call_parameters: String,
		_network_config: Option<NetworkConfig>,
	) -> SubscriptionResponse<ChainHeadEvent<String>> {
		let mut pending = MaybePendingSubscription::new(pending);

		let Ok(call_parameters) = parse_hex_param(&mut pending, call_parameters).await.map(|b| Bytes::from(b)) else {
			return SubscriptionResponse::Closed
		};

		let client = self.client.clone();
		let subscriptions = self.subscriptions.clone();

		let block_guard = match subscriptions.lock_block(&follow_subscription, hash) {
			Ok(block) => block,
			Err(SubscriptionManagementError::SubscriptionAbsent) => {
				let _ = pending.accept().await;
				// Invalid invalid subscription ID.
				return SubscriptionResponse::Event(ChainHeadEvent::Disjoint)
			},
			Err(SubscriptionManagementError::BlockHashAbsent) => {
				// Block is not part of the subscription.
				pending.reject(ChainHeadRpcError::InvalidBlock).await;
				return SubscriptionResponse::Closed
			},
			Err(error) => {
				let _ = pending.accept().await;
				return SubscriptionResponse::Event(ChainHeadEvent::Error(ErrorEvent {
					error: error.to_string(),
				}))
			},
		};

		let fut = async move {
			// Reject subscription if runtime_updates is false.
			if !block_guard.has_runtime() {
				pending
					.reject(ChainHeadRpcError::InvalidParam(
						"The runtime updates flag must be set".into(),
					))
					.await;
				return SubscriptionResponse::Closed
			}

			if pending.accept().await.is_err() {
				return SubscriptionResponse::Closed
			}

			let res = client
				.executor()
				.call(
					hash,
					&function,
					&call_parameters,
					client.execution_extensions().strategies().other,
					CallContext::Offchain,
				)
				.map(|result| {
					let result = format!("0x{:?}", HexDisplay::from(&result));
					ChainHeadEvent::Done(ChainHeadResult { result })
				})
				.unwrap_or_else(|error| {
					ChainHeadEvent::Error(ErrorEvent { error: error.to_string() })
				});

			SubscriptionResponse::Event(res)
		};

		sc_rpc::utils::spawn_subscription_task(&self.executor, fut).await
	}

	fn chain_head_unstable_unpin(
		&self,
		follow_subscription: String,
		hash: Block::Hash,
	) -> RpcResult<()> {
		match self.subscriptions.unpin_block(&follow_subscription, hash) {
			Ok(()) => Ok(()),
			Err(SubscriptionManagementError::SubscriptionAbsent) => {
				// Invalid invalid subscription ID.
				Ok(())
			},
			Err(SubscriptionManagementError::BlockHashAbsent) => {
				// Block is not part of the subscription.
				Err(ChainHeadRpcError::InvalidBlock.into())
			},
			Err(_) => Err(ChainHeadRpcError::InvalidBlock.into()),
		}
	}
}
