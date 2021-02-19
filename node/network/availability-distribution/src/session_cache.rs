// Copyright 2021 Parity Technologies (UK) Ltd.
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

use std::collections::HashSet;

use lru::LruCache;
use rand::{seq::SliceRandom, thread_rng};

use sp_application_crypto::AppKey;
use sp_core::crypto::Public;
use sp_keystore::{CryptoStore, SyncCryptoStorePtr};

use polkadot_node_subsystem_util::{
	request_session_index_for_child_ctx, request_session_info_ctx,
};
use polkadot_primitives::v1::SessionInfo as GlobalSessionInfo;
use polkadot_primitives::v1::{
	AuthorityDiscoveryId, GroupIndex, Hash, SessionIndex, ValidatorId, ValidatorIndex,
};
use polkadot_subsystem::SubsystemContext;

use super::{
	error::{recv_runtime, Result},
	Error,
};

/// Caching of session info as needed by availability distribution.
///
/// It should be ensured that a cached session stays live in the cache as long as we might need it.
/// A warning will be logged, if an already dead entry gets fetched.
pub struct SessionCache {
	/// Get the session index for a given relay parent.
	///
	/// We query this up to a 100 times per block, so caching it here without roundtrips over the
	/// overseer seems sensible.
	session_index_cache: LruCache<Hash, SessionIndex>,

	/// Look up cached sessions by SessionIndex.
	///
	/// Note: Performance of fetching is really secondary here, but we need to ensure we are going
	/// to get any existing cache entry, before fetching new information, as we should not mess up
	/// the order of validators. (We want live TCP connections wherever possible.)
	session_info_cache: LruCache<SessionIndex, SessionInfo>,

	/// Key store for determining whether we are a validator and what `ValidatorIndex` we have.
	keystore: SyncCryptoStorePtr,
}

/// Localized session information, tailored for the needs of availability distribution.
#[derive(Clone)]
pub struct SessionInfo {
	/// The index of this session.
	pub session_index: SessionIndex,
	/// Validator groups of the current session.
	///
	/// Each group's order is randomized. This way we achieve load balancing when requesting
	/// chunks, as the validators in a group will be tried in that randomized order. Each node
	/// should arrive at a different order, therefore we distribute the load.
	pub validator_groups: Vec<Vec<AuthorityDiscoveryId>>,

	/// Information about ourself:
	pub our_index: ValidatorIndex,

	/// Remember to which group we belong, so we won't start fetching chunks for candidates those
	/// candidates (We should have them via PoV distribution).
	pub our_group: GroupIndex,
}

/// Report of bad validators.
pub struct BadValidators {
	/// The session index that was used.
	pub session_index: SessionIndex,
	/// The group the not properly responding validators are.
	pub group_index: GroupIndex,
	/// The indeces of the bad validators.
	pub bad_validators: Vec<AuthorityDiscoveryId>,
}

impl SessionCache {
	pub fn new(keystore: SyncCryptoStorePtr) -> Self {
		SessionCache {
			// 5 relatively conservative, 1 to 2 should suffice:
			session_index_cache: LruCache::new(5),
			// We need to cache the current and the last session the most:
			session_info_cache: LruCache::new(2),
			keystore,
		}
	}

	/// Tries to retrieve `SessionInfo` and calls `with_info` if successful.
	///
	/// If this node is not a validator, the function will return `None`.
	///
	/// Use this function over `fetch_session_info` if all you need is a reference to
	/// `SessionInfo`, as it avoids an expensive clone.
	pub async fn with_session_info<Context, F, R>(
		&mut self,
		ctx: &mut Context,
		parent: Hash,
		with_info: F,
	) -> Result<Option<R>>
	where
		Context: SubsystemContext,
		F: FnOnce(&SessionInfo) -> R,
	{
		let session_index = match self.session_index_cache.get(&parent) {
			Some(index) => *index,
			None => {
				let index =
					recv_runtime(request_session_index_for_child_ctx(parent, ctx).await)
						.await?;
				self.session_index_cache.put(parent, index);
				index
			}
		};

		if let Some(info) = self.session_info_cache.get(&session_index) {
			return Ok(Some(with_info(info)));
		}

		if let Some(info) = self
			.query_info_from_runtime(ctx, parent, session_index)
			.await?
		{
			let r = with_info(&info);
			self.session_info_cache.put(session_index, info);
			return Ok(Some(r));
		}
		Ok(None)
	}

	/// Make sure we try unresponsive or misbehaving validators last.
	///
	/// We assume validators in a group are tried in reverse order, so the reported bad validators
	/// will be put at the beginning of the group.
	pub fn report_bad(&mut self, mut report: BadValidators) -> Result<()> {
		let session = self
			.session_info_cache
			.get_mut(&report.session_index)
			.ok_or(Error::ReportBadValidators("Session is not cached."))?;
		let group = session
			.validator_groups
			.get_mut(report.group_index.0 as usize)
			.ok_or(Error::ReportBadValidators("Validator group not found"))?;
		let bad_set = report.bad_validators.iter().collect::<HashSet<_>>();

		// Get rid of bad boys:
		group.retain(|v| !bad_set.contains(v));

		// We are trying validators in reverse order, so bad ones should be first:
		let mut new_group = report.bad_validators;
		new_group.append(group);
		*group = new_group;
		Ok(())
	}

	/// Query needed information from runtime.
	///
	/// We need to pass in the relay parent for our call to `request_session_info_ctx`. We should
	/// actually don't need that, I suppose it is used for internal caching based on relay parents,
	/// which we don't use here. It should not do any harm though.
	async fn query_info_from_runtime<Context>(
		&self,
		ctx: &mut Context,
		parent: Hash,
		session_index: SessionIndex,
	) -> Result<Option<SessionInfo>>
	where
		Context: SubsystemContext,
	{
		let GlobalSessionInfo {
			validators,
			discovery_keys,
			mut validator_groups,
			..
		} = recv_runtime(request_session_info_ctx(parent, session_index, ctx).await)
			.await?
			.ok_or(Error::NoSuchSession(session_index))?;

		if let Some(our_index) = self.get_our_index(validators).await {
			// Get our group index:
			let our_group = validator_groups
				.iter()
				.enumerate()
				.find_map(|(i, g)| {
					g.iter().find_map(|v| {
						if *v == our_index {
							Some(GroupIndex(i as u32))
						} else {
							None
						}
					})
				})
				// TODO: Make sure this is correct and should be enforced:
				.expect("Every validator should be in a validator group. qed.");

			// Shuffle validators in groups:
			let mut rng = thread_rng();
			for g in validator_groups.iter_mut() {
				g.shuffle(&mut rng)
			}
			// Look up `AuthorityDiscoveryId`s right away:
			let validator_groups: Vec<Vec<_>> = validator_groups
				.into_iter()
				.map(|group| {
					group
						.into_iter()
						.map(|index| {
							discovery_keys.get(index.0 as usize)
							.expect("There should be a discovery key for each validator of each validator group. qed.").clone()
						})
						.collect()
				})
				.collect();

			let info = SessionInfo {
				validator_groups,
				our_index,
				session_index,
				our_group,
			};
			return Ok(Some(info));
		}
		return Ok(None);
	}

	/// Get our validator id and the validators in the current session.
	///
	/// Returns: Ok(None) if we are not a validator.
	async fn get_our_index(&self, validators: Vec<ValidatorId>) -> Option<ValidatorIndex> {
		for (i, v) in validators.iter().enumerate() {
			if CryptoStore::has_keys(&*self.keystore, &[(v.to_raw_vec(), ValidatorId::ID)])
				.await
			{
				return Some(ValidatorIndex(i as u32));
			}
		}
		None
	}
}