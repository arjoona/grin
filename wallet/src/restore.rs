// Copyright 2018 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use failure::{Fail, ResultExt};
use keychain::{Identifier, Keychain};
use util::LOGGER;
use util::secp::pedersen;
use api;
use core::global;
use core::core::transaction::ProofMessageElements;
use types::{Error, ErrorKind, OutputData, OutputStatus, WalletConfig, WalletData};
use byteorder::{BigEndian, ByteOrder};

pub fn _get_chain_height(config: &WalletConfig) -> Result<u64, Error> {
	let url = format!("{}/v1/chain", config.check_node_api_http_addr);

	match api::client::get::<api::Tip>(url.as_str()) {
		Ok(tip) => Ok(tip.height),
		Err(e) => {
			// if we got anything other than 200 back from server, bye
			error!(
				LOGGER,
				"get_chain_height: Restore failed... unable to contact API {}. Error: {}",
				config.check_node_api_http_addr,
				e
			);
			Err(e.context(ErrorKind::Node).into())
		}
	}
}

fn coinbase_status(output: &api::OutputPrintable) -> bool {
	match output.output_type {
		api::OutputType::Coinbase => true,
		api::OutputType::Transaction => false,
	}
}

pub fn outputs_batch(
	config: &WalletConfig,
	start_height: u64,
	max: u64,
) -> Result<api::OutputListing, Error> {
	let query_param = format!("start_index={}&max={}", start_height, max);

	let url = format!(
		"{}/v1/txhashset/outputs?{}",
		config.check_node_api_http_addr, query_param,
	);

	match api::client::get::<api::OutputListing>(url.as_str()) {
		Ok(o) => Ok(o),
		Err(e) => {
			// if we got anything other than 200 back from server, bye
			error!(
				LOGGER,
				"outputs_batch: Restore failed... unable to contact API {}. Error: {}",
				config.check_node_api_http_addr,
				e
			);
			Err(e.context(ErrorKind::Node))?
		}
	}
}

// TODO - wrap the many return values in a struct
fn find_outputs_with_key(
	keychain: &Keychain,
	outputs: Vec<api::OutputPrintable>,
) -> Vec<(pedersen::Commitment, Identifier, u32, u64, u64, u64, bool)> {
	let mut wallet_outputs: Vec<(pedersen::Commitment, Identifier, u32, u64, u64, u64, bool)> =
		Vec::new();

	let max_derivations = 1_000_000;

	info!(LOGGER, "Scanning {} outputs", outputs.len(),);

	// skey doesn't matter in this case
	let skey = keychain.derive_key_id(1).unwrap();
	for output in outputs.iter().filter(|x| !x.spent) {
		// attempt to unwind message from the RP and get a value.. note
		// this will only return okay if the value is included in the
		// message 3 times, indicating a strong match. Also, sec_key provided
		// to unwind in this case will be meaningless. With only the nonce known
		// only the first 32 bytes of the recovered message will be accurate
		let info = keychain
			.rewind_range_proof(&skey, output.commit, None, output.range_proof().unwrap())
			.unwrap();
		let message = ProofMessageElements::from_proof_message(info.message).unwrap();
		let value = message.value();
		if value.is_err() {
			continue;
		}
		// we have a match, now check through our key iterations to find a partial match
		let mut found = false;
		for i in 1..max_derivations {
			let key_id = &keychain.derive_key_id(i as u32).unwrap();
			if !message.compare_bf_first_8(key_id) {
				continue;
			}
			found = true;
			// we have a partial match, let's just confirm
			let info = keychain
				.rewind_range_proof(key_id, output.commit, None, output.range_proof().unwrap())
				.unwrap();
			let message = ProofMessageElements::from_proof_message(info.message).unwrap();
			let value = message.value();
			if value.is_err() || !message.zeroes_correct() {
				continue;
			}
			let value = value.unwrap();
			info!(
				LOGGER,
				"Output found: {:?}, key_index: {:?}", output.commit, i,
			);

			// add it to result set here
			let commit_id = output.commit.0;

			let is_coinbase = coinbase_status(output);

			info!(LOGGER, "Amount: {}", value);

			let commit = keychain
				.commit_with_key_index(BigEndian::read_u64(&commit_id), i as u32)
				.expect("commit with key index");

			//let height = outputs.header.height;
			let height = 0;
			let lock_height = if is_coinbase {
				height + global::coinbase_maturity()
			} else {
				0
			};

			wallet_outputs.push((
				commit,
				key_id.clone(),
				i as u32,
				value,
				height,
				lock_height,
				is_coinbase,
			));

			break;
		}
		if !found {
			warn!(
				LOGGER,
				"Very probable matching output found with amount: {} \
				 but didn't match key child key up to {}",
				message.value().unwrap(),
				max_derivations,
			);
		}
	}
	debug!(LOGGER, "Found {} wallet_outputs", wallet_outputs.len(),);

	wallet_outputs
}

pub fn restore(config: &WalletConfig, keychain: &Keychain) -> Result<(), Error> {
	// Don't proceed if wallet.dat has anything in it
	let is_empty = WalletData::read_wallet(&config.data_file_dir, |wallet_data| {
		Ok(wallet_data.outputs.len() == 0)
	}).context(ErrorKind::WalletData("could not read wallet"))?;
	if !is_empty {
		error!(
			LOGGER,
			"Not restoring. Please back up and remove existing wallet.dat first."
		);
		return Ok(());
	}

	info!(LOGGER, "Starting restore.");

	let batch_size = 1000;
	let mut start_index = 1;
	// this will start here, then lower as outputs are found, moving backwards on
	// the chain
	loop {
		let output_listing = outputs_batch(config, start_index, batch_size)?;
		info!(
			LOGGER,
			"Retrieved {} outputs, up to index {}. (Highest index: {})",
			output_listing.outputs.len(),
			output_listing.last_retrieved_index,
			output_listing.highest_index
		);

		let _ = WalletData::with_wallet(&config.data_file_dir, |wallet_data| {
			let result_vec = find_outputs_with_key(keychain, output_listing.outputs.clone());
			if result_vec.len() > 0 {
				for output in result_vec.clone() {
					let root_key_id = keychain.root_key_id();
					// Just plonk it in for now, and refresh actual values via wallet info
					// command later
					wallet_data.add_output(OutputData {
						root_key_id: root_key_id.clone(),
						key_id: output.1.clone(),
						n_child: output.2,
						value: output.3,
						status: OutputStatus::Unconfirmed,
						height: output.4,
						lock_height: output.5,
						is_coinbase: output.6,
						block: None,
						merkle_proof: None,
					});
				}
			}
		});
		if output_listing.highest_index == output_listing.last_retrieved_index {
			break;
		}
		start_index = output_listing.last_retrieved_index + 1;
	}
	Ok(())
}
